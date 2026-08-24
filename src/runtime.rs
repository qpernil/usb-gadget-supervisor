//! Privileged ConfigFS projection of a worker-owned USB device.

use crate::functionfs::{self, Direction};
use crate::profile::{
    CharacterDeviceResource, GpioBias, GpioDirection, GpioEdge, GpioLinesResource, Profile,
    ResourceAccess, ResourceProfile,
};
use crate::protocol::{self, Kind, Record, CONTROL_FD, RUNTIME_DIRECTORY_ENV, STATE_DIRECTORY_ENV};
use crate::usb_personality::{self, Personality};
use crate::{RESTART_REQUESTED, STOP_REQUESTED};
use std::ffi::{c_void, CString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{chown, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};
use usb_gadget_worker::UsbBusEvent;

const CONFIGFS: &str = "/sys/kernel/config";
const GADGET_ROOT: &str = "/sys/kernel/config/usb_gadget";
const LOCK_FILE: &str = "/run/lock/usb-gadget-supervisor.lock";
const DEBUG_BUNDLE_ROOT: &str = "/run/usb-gadget-supervisor";
const USB_RECONNECT_DWELL: Duration = Duration::from_millis(250);

struct WorkerIdentity {
    name: String,
    uid: u32,
    gid: u32,
}

struct UsbGeneration {
    ep0: File,
    endpoints: Vec<File>,
    endpoints_enabled: bool,
    endpoint_activation: u64,
}

pub(crate) struct Runtime {
    profile_path: PathBuf,
    profile: Profile,
    identity: WorkerIdentity,
    gadget: PathBuf,
    _lock: File,
    configfs_mounted_by_us: bool,
    owns_gadget: bool,
    functionfs_mounted: bool,
    worker: Option<Child>,
    control: Option<UnixStream>,
    usb: Option<UsbGeneration>,
    udc: String,
    generation: u32,
    next_control_request: u32,
    detached_at: Option<Instant>,
    cleaned: bool,
}

impl Runtime {
    pub(crate) fn setup(
        profile_path: PathBuf,
        profile: Profile,
        requested_udc: Option<&str>,
    ) -> io::Result<Self> {
        if unsafe { libc::geteuid() } != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "USB gadget setup needs root",
            ));
        }
        validate_root_owned_file(&profile_path, "profile")?;
        let identity = resolve_worker_identity(&profile.worker.run_as)?;
        validate_worker_executable(&profile.worker.command, &identity)?;
        let lock = acquire_lock()?;
        let configfs_mounted_by_us = ensure_configfs()?;
        let gadget = Path::new(GADGET_ROOT).join(&profile.name);
        let udc = select_udc(requested_udc)?;
        let mut runtime = Self {
            profile_path,
            profile,
            identity,
            gadget,
            _lock: lock,
            configfs_mounted_by_us,
            owns_gadget: false,
            functionfs_mounted: false,
            worker: None,
            control: None,
            usb: None,
            udc,
            generation: 0,
            next_control_request: 1,
            detached_at: None,
            cleaned: false,
        };
        runtime.cleanup_stale_state()?;
        runtime.start_worker()?;
        let configuration = protocol::receive(runtime.control.as_ref().unwrap())?;
        runtime.configure(configuration, true)?;
        Ok(runtime)
    }

    pub(crate) fn serve(&mut self, signal_fd: i32) -> io::Result<()> {
        while !STOP_REQUESTED.load(Ordering::Relaxed) {
            if let Some(status) = self.worker.as_mut().expect("worker exists").try_wait()? {
                eprintln!("usb-gadget-supervisor: worker exited with {status}; restarting");
                self.restart_worker()?;
                continue;
            }
            let control = self.control.as_ref().expect("control exists").as_raw_fd();
            let ep0 = self
                .usb
                .as_ref()
                .map_or(-1, |generation| generation.ep0.as_raw_fd());
            let mut pollfds = [
                libc::pollfd {
                    fd: signal_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: control,
                    events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                    revents: 0,
                },
                libc::pollfd {
                    fd: ep0,
                    events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                    revents: 0,
                },
            ];
            let ready = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as _, -1) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if pollfds[0].revents != 0 {
                drain_signal_notifications(signal_fd)?;
                if STOP_REQUESTED.load(Ordering::Relaxed) {
                    continue;
                }
                if RESTART_REQUESTED.swap(false, Ordering::Relaxed) {
                    self.reload_profile()?;
                    continue;
                }
            }
            if pollfds[1].revents != 0 {
                match protocol::receive_nonblocking(self.control.as_ref().unwrap()) {
                    Ok(record) if record.kind == Kind::Configure => {
                        self.configure(record, false)?;
                        continue;
                    }
                    Ok(record) => {
                        return invalid(format!(
                            "unexpected asynchronous worker message {:?}",
                            record.kind
                        ))
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(error) => {
                        eprintln!(
                            "usb-gadget-supervisor: worker channel ended ({error}); restarting"
                        );
                        self.restart_worker()?;
                        continue;
                    }
                }
            }
            if pollfds[2].revents != 0 {
                self.service_ep0()?;
            }
        }
        Ok(())
    }

    pub(crate) fn cleanup(&mut self) -> io::Result<()> {
        if self.cleaned {
            return Ok(());
        }
        let mut first_error = None;
        if self.owns_gadget {
            record_error(&mut first_error, self.unbind());
        }
        if self.usb.is_some() {
            record_error(&mut first_error, self.quiesce_worker(0));
        }
        self.control = None;
        record_error(&mut first_error, stop_worker(&mut self.worker));
        record_error(&mut first_error, self.stop_usb_generation());
        record_error(
            &mut first_error,
            remove_dir_if_exists(&self.profile.worker.runtime_directory),
        );
        if self.configfs_mounted_by_us {
            record_error(
                &mut first_error,
                unmount_filesystem(Path::new(CONFIGFS), "configfs").map(|_| ()),
            );
            self.configfs_mounted_by_us = false;
        }
        self.cleaned = true;
        first_error.map_or(Ok(()), Err)
    }

    fn start_worker(&mut self) -> io::Result<()> {
        prepare_owned_directory(
            &self.profile.worker.state_directory,
            self.identity.uid,
            self.identity.gid,
        )?;
        prepare_owned_directory(
            &self.profile.worker.runtime_directory,
            self.identity.uid,
            self.identity.gid,
        )?;
        let resources = self.open_resources()?;
        let (supervisor, worker_channel) = seqpacket_pair()?;
        supervisor.set_read_timeout(Some(Duration::from_millis(
            self.profile.worker.readiness_timeout_ms,
        )))?;
        let control_fd = worker_channel.as_raw_fd();
        let parent_pid = std::process::id() as libc::pid_t;
        let uid = self.identity.uid;
        let gid = self.identity.gid;
        let mut command = Command::new(&self.profile.worker.command);
        command
            .args(&self.profile.worker.arguments)
            .env_clear()
            .env(STATE_DIRECTORY_ENV, &self.profile.worker.state_directory)
            .env(
                RUNTIME_DIRECTORY_ENV,
                &self.profile.worker.runtime_directory,
            )
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        unsafe {
            command.pre_exec(move || {
                if control_fd != CONTROL_FD && libc::dup2(control_fd, CONTROL_FD) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::fcntl(CONTROL_FD, libc::F_SETFD, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::setgroups(0, std::ptr::null()) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::setgid(gid) != 0 || libc::setuid(uid) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    return Err(io::Error::from_raw_os_error(libc::EPIPE));
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        drop(worker_channel);
        let body = resource_names(&self.profile.resources)?;
        let record = Record::new(Kind::InitialResources, 0, 0, body);
        if let Err(error) = protocol::send(&supervisor, &record, &resources) {
            drop(supervisor);
            let _ = stop_worker(&mut Some(child));
            return Err(io::Error::new(
                error.kind(),
                format!("worker did not initialize: {error}"),
            ));
        }
        self.worker = Some(child);
        self.control = Some(supervisor);
        println!(
            "usb-gadget-supervisor: firmware worker started as {}",
            self.identity.name
        );
        Ok(())
    }

    fn configure(&mut self, declaration: Record, initial: bool) -> io::Result<()> {
        if declaration.kind != Kind::Configure
            || declaration.generation != self.generation
            || declaration.request_id == 0
            || !declaration.descriptors.is_empty()
        {
            return invalid("worker returned a mismatched USB configuration");
        }
        if declaration.body.is_empty() {
            if self.usb.is_none() {
                if initial {
                    self.control.as_ref().unwrap().set_read_timeout(None)?;
                    println!(
                        "USB gadget {} worker is ready without a USB personality; waiting for configuration",
                        self.profile.name
                    );
                    return Ok(());
                }
                return invalid(
                    "worker requested USB unconfiguration without a serving generation",
                );
            }
            let request_id = declaration.request_id;
            self.unbind()?;
            self.quiesce_worker(request_id)?;
            self.stop_usb_generation()?;
            // An empty personality has a worker-controlled detached lifetime.
            // Its later nonempty Configure must not add replacement dwell.
            self.detached_at = None;
            println!(
                "USB gadget {} unconfigured at generation {}; waiting for worker configuration",
                self.profile.name, self.generation
            );
            return Ok(());
        }
        let request_id = declaration.request_id;
        let (bundle, personality) = match usb_personality::discover_bundle(&declaration.body) {
            Ok(configuration) => configuration,
            Err(error) => {
                eprintln!(
                    "usb-gadget-supervisor: rejected USB configuration request {request_id}: {error}"
                );
                self.reject_configuration(request_id, &error)?;
                return if initial { Err(error) } else { Ok(()) };
            }
        };
        eprintln!("usb-gadget-supervisor: USB configuration request {request_id}: {bundle:?}");
        self.control
            .as_ref()
            .unwrap()
            .set_read_timeout(Some(Duration::from_millis(
                self.profile.worker.readiness_timeout_ms,
            )))?;
        if self.usb.is_some() {
            self.unbind()?;
            self.quiesce_worker(request_id)?;
            self.stop_usb_generation()?;
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("USB generation overflow"))?;
        self.populate_gadget(&personality)?;
        self.mount_functionfs()?;
        let (ep0, endpoints) = self.publish_functionfs(&personality)?;
        self.usb = Some(UsbGeneration {
            ep0,
            endpoints,
            endpoints_enabled: false,
            endpoint_activation: 0,
        });
        self.link_function()?;
        let body = endpoint_map(&personality)?;
        let files = self
            .usb
            .as_ref()
            .expect("USB generation exists")
            .endpoints
            .iter()
            .map(AsFd::as_fd)
            .collect::<Vec<_>>();
        protocol::send(
            self.control.as_ref().unwrap(),
            &Record::new(Kind::UsbEndpoints, self.generation, request_id, body),
            &files,
        )?;
        drop(files);
        expect_record(
            self.control.as_ref().unwrap(),
            Kind::Serving,
            self.generation,
            request_id,
        )?;
        self.wait_for_reconnect_dwell();
        write_attribute(&self.gadget.join("UDC"), &self.udc)?;
        println!(
            "USB gadget {} attached as {:04x}:{:04x}; generation {} has {} endpoints",
            self.profile.name,
            personality.device.vendor_id,
            personality.device.product_id,
            self.generation,
            personality.endpoints.len()
        );
        self.control.as_ref().unwrap().set_read_timeout(None)?;
        self.persist_bundle(&declaration.body)?;
        Ok(())
    }

    fn service_ep0(&mut self) -> io::Result<()> {
        const EVENT_LENGTH: usize = 12;
        let mut events = [0_u8; EVENT_LENGTH * 16];
        let ep0 = self
            .usb
            .as_ref()
            .ok_or_else(|| io::Error::other("FunctionFS EP0 is unavailable"))?
            .ep0
            .as_raw_fd();
        loop {
            let length = unsafe { libc::read(ep0, events.as_mut_ptr().cast(), events.len()) };
            if length < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(());
                }
                return Err(error);
            }
            if length == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "FunctionFS EP0 closed",
                ));
            }
            let length = length as usize;
            if length % EVENT_LENGTH != 0 {
                return invalid("truncated FunctionFS event stream");
            }
            for event in events[..length].chunks_exact(EVENT_LENGTH) {
                match event[8] {
                    0 | 1 | 2 | 3 | 5 | 6 => self.forward_bus_event(event[8])?,
                    4 => match self.forward_control_request(&event[..8]) {
                        Ok(()) => {}
                        Err(error) if control_request_cancelled(&error) => {
                            eprintln!("usb-gadget-supervisor: USB control request was superseded");
                        }
                        Err(error) => return Err(error),
                    },
                    kind => return invalid(format!("unknown FunctionFS event {kind}")),
                }
            }
        }
    }

    fn forward_bus_event(&mut self, event: u8) -> io::Result<()> {
        let event = UsbBusEvent::from_byte(event)?;
        let activation = {
            let generation = self.usb.as_mut().expect("USB generation exists");
            if event == UsbBusEvent::Enable && !generation.endpoints_enabled {
                generation.endpoint_activation = generation
                    .endpoint_activation
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("endpoint activation overflow"))?;
                generation.endpoints_enabled = true;
            }
            if matches!(
                event,
                UsbBusEvent::Bind | UsbBusEvent::Unbind | UsbBusEvent::Disable
            ) {
                generation.endpoints_enabled = false;
            }
            generation.endpoint_activation
        };
        protocol::send(
            self.control.as_ref().expect("control exists"),
            &Record::new(
                Kind::UsbBusEvent,
                self.generation,
                0,
                event.encode(activation).to_vec(),
            ),
            &[] as &[File],
        )
    }

    fn forward_control_request(&mut self, setup: &[u8]) -> io::Result<()> {
        let direction_in = setup[0] & 0x80 != 0;
        let transfer_length = u16::from_le_bytes([setup[6], setup[7]]) as usize;
        let mut body = Vec::with_capacity(8 + if direction_in { 0 } else { transfer_length });
        body.extend_from_slice(setup);
        if !direction_in && transfer_length != 0 {
            let offset = body.len();
            body.resize(offset + transfer_length, 0);
            transfer_ep0(
                self.usb
                    .as_ref()
                    .expect("USB generation exists")
                    .ep0
                    .as_raw_fd(),
                &mut body[offset..],
                false,
            )?;
        }
        let request_id = self.next_control_request;
        self.next_control_request = self
            .next_control_request
            .checked_add(1)
            .filter(|value| *value != 0)
            .ok_or_else(|| io::Error::other("USB control request ID overflow"))?;
        protocol::send(
            self.control.as_ref().expect("control exists"),
            &Record::new(Kind::UsbControlRequest, self.generation, request_id, body),
            &[] as &[File],
        )?;
        let control = self.control.as_ref().expect("control exists");
        control.set_read_timeout(Some(Duration::from_millis(
            self.profile.worker.readiness_timeout_ms,
        )))?;
        let response = protocol::receive(control);
        control.set_read_timeout(None)?;
        let response = response?;
        if response.kind != Kind::UsbControlResponse
            || response.generation != self.generation
            || response.request_id != request_id
            || !response.descriptors.is_empty()
            || response.body.is_empty()
        {
            return invalid("worker returned a mismatched USB control response");
        }
        let ep0 = self
            .usb
            .as_ref()
            .expect("USB generation exists")
            .ep0
            .as_raw_fd();
        match response.body[0] {
            0 if response.body.len() == 1 => stall_ep0(ep0, direction_in),
            1 if response.body.len() == 1 && !direction_in => transfer_ep0(ep0, &mut [], true),
            2 if direction_in => {
                let length = response.body.len().saturating_sub(1).min(transfer_length);
                let mut data = response.body[1..1 + length].to_vec();
                transfer_ep0(ep0, &mut data, true)
            }
            _ => invalid("invalid worker USB control response"),
        }
    }

    fn reject_configuration(&self, request_id: u32, error: &io::Error) -> io::Result<()> {
        protocol::send(
            self.control.as_ref().unwrap(),
            &Record::new(
                Kind::ConfigurationRejected,
                self.generation,
                request_id,
                error.to_string().into_bytes(),
            ),
            &[] as &[File],
        )
    }

    fn persist_bundle(&self, bundle: &[u8]) -> io::Result<()> {
        fs::create_dir_all(DEBUG_BUNDLE_ROOT)?;
        fs::set_permissions(DEBUG_BUNDLE_ROOT, fs::Permissions::from_mode(0o700))?;
        let path = Path::new(DEBUG_BUNDLE_ROOT).join(format!("{}.cbor", self.profile.name));
        let temporary = Path::new(DEBUG_BUNDLE_ROOT).join(format!(
            ".{}.{}.tmp",
            self.profile.name,
            std::process::id()
        ));
        let mut output = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        output.write_all(bundle)?;
        output.sync_all()?;
        fs::rename(temporary, path)
    }

    fn quiesce_worker(&mut self, request_id: u32) -> io::Result<()> {
        let Some(control) = self.control.as_ref() else {
            return Ok(());
        };
        println!(
            "usb-gadget-supervisor: quiescing {} generation={} request={request_id}",
            self.profile.name, self.generation
        );
        let result = (|| {
            control.set_read_timeout(Some(Duration::from_millis(
                self.profile.worker.readiness_timeout_ms,
            )))?;
            let result = protocol::send(
                control,
                &Record::new(Kind::Quiesce, self.generation, request_id, Vec::new()),
                &[] as &[File],
            )
            .and_then(|_| expect_record(control, Kind::Quiesced, self.generation, request_id));
            let timeout_result = control.set_read_timeout(None);
            result.and(timeout_result)
        })();
        match result {
            Ok(()) => {
                println!(
                    "usb-gadget-supervisor: quiesced {} generation={} request={request_id}",
                    self.profile.name, self.generation
                );
                Ok(())
            }
            Err(error) => {
                eprintln!(
                    "usb-gadget-supervisor: quiesce failed for {} generation={} request={request_id}: {error}; closing worker control channel",
                    self.profile.name, self.generation
                );
                // Do not repeat the same readiness timeout from cleanup. Closing
                // the channel lets a responsive worker exit; stop_worker bounds
                // an unresponsive endpoint helper with TERM/KILL.
                self.control = None;
                Err(error)
            }
        }
    }

    fn stop_usb_generation(&mut self) -> io::Result<()> {
        let mut first_error = None;
        if self.owns_gadget {
            record_error(&mut first_error, self.unbind());
        }
        self.usb = None;
        if self.functionfs_mounted {
            record_error(
                &mut first_error,
                unmount_filesystem(&self.profile.functionfs_mount, "functionfs").map(|_| ()),
            );
            self.functionfs_mounted = false;
        }
        if self.owns_gadget {
            record_error(&mut first_error, self.remove_gadget_tree());
            self.owns_gadget = false;
        }
        record_error(
            &mut first_error,
            remove_dir_if_exists(&self.profile.functionfs_mount),
        );
        first_error.map_or(Ok(()), Err)
    }

    fn restart_worker(&mut self) -> io::Result<()> {
        if self.owns_gadget {
            self.unbind()?;
        }
        self.control = None;
        stop_worker(&mut self.worker)?;
        self.stop_usb_generation()?;
        self.generation = 0;
        self.start_worker()?;
        let configuration = protocol::receive(self.control.as_ref().unwrap())?;
        self.configure(configuration, true)
    }

    fn reload_profile(&mut self) -> io::Result<()> {
        validate_root_owned_file(&self.profile_path, "profile")?;
        let profile = Profile::load(&self.profile_path)?;
        let identity = resolve_worker_identity(&profile.worker.run_as)?;
        validate_worker_executable(&profile.worker.command, &identity)?;
        if self.owns_gadget {
            self.unbind()?;
        }
        if self.usb.is_some() {
            self.quiesce_worker(0)?;
        }
        self.control = None;
        stop_worker(&mut self.worker)?;
        self.stop_usb_generation()?;
        self.profile = profile;
        self.identity = identity;
        self.gadget = Path::new(GADGET_ROOT).join(&self.profile.name);
        self.generation = 0;
        self.cleanup_stale_state()?;
        self.start_worker()?;
        let configuration = protocol::receive(self.control.as_ref().unwrap())?;
        self.configure(configuration, true)
    }

    fn cleanup_stale_state(&mut self) -> io::Result<()> {
        if self.gadget.exists() {
            let _ = self.unbind();
            self.owns_gadget = true;
            self.remove_gadget_tree()?;
            self.owns_gadget = false;
        }
        unmount_filesystem(&self.profile.functionfs_mount, "functionfs")?;
        remove_dir_if_exists(&self.profile.functionfs_mount)
    }

    fn populate_gadget(&mut self, personality: &Personality) -> io::Result<()> {
        fs::create_dir(&self.gadget)?;
        self.owns_gadget = true;
        let usb = &personality.device;
        write_attribute(&self.gadget.join("max_speed"), personality.max_speed)?;
        for (name, value) in [
            ("idVendor", format!("0x{:04x}", usb.vendor_id)),
            ("idProduct", format!("0x{:04x}", usb.product_id)),
            ("bcdUSB", format!("0x{:04x}", usb.bcd_usb)),
            ("bcdDevice", format!("0x{:04x}", usb.bcd_device)),
            ("bDeviceClass", format!("0x{:02x}", usb.device_class)),
            ("bDeviceSubClass", format!("0x{:02x}", usb.device_subclass)),
            ("bDeviceProtocol", format!("0x{:02x}", usb.device_protocol)),
        ] {
            write_attribute(&self.gadget.join(name), &value)?;
        }
        let strings = self.gadget.join("strings/0x409");
        fs::create_dir(&strings)?;
        if let Some(value) = &usb.manufacturer {
            write_attribute(&strings.join("manufacturer"), value)?;
        }
        if let Some(value) = &usb.product {
            write_attribute(&strings.join("product"), value)?;
        }
        if let Some(value) = &usb.serial {
            write_attribute(&strings.join("serialnumber"), value)?;
        }
        let config = self.gadget.join("configs/c.1");
        fs::create_dir(&config)?;
        write_attribute(
            &config.join("MaxPower"),
            &personality.max_power_ma.to_string(),
        )?;
        write_attribute(
            &config.join("bmAttributes"),
            &format!("0x{:02x}", personality.configuration_attributes),
        )?;
        if let Some(microsoft) = &personality.microsoft_os_1 {
            let os = self.gadget.join("os_desc");
            write_attribute(
                &os.join("b_vendor_code"),
                &format!("0x{:02x}", microsoft.vendor_code),
            )?;
            write_attribute(&os.join("qw_sign"), &microsoft.signature)?;
            write_attribute(&os.join("use"), "1")?;
            std::os::unix::fs::symlink(&config, os.join("c.1"))?;
        }
        if let Some(webusb) = &personality.webusb {
            let directory = self.gadget.join("webusb");
            write_attribute(
                &directory.join("bcdVersion"),
                &format!("0x{:04x}", webusb.version),
            )?;
            write_attribute(
                &directory.join("bVendorCode"),
                &format!("0x{:02x}", webusb.vendor_code),
            )?;
            if !webusb.landing_page.is_empty() {
                write_attribute(&directory.join("landingPage"), &webusb.landing_page)?;
            }
            write_attribute(&directory.join("use"), "1")?;
        }
        fs::create_dir(
            self.gadget
                .join(format!("functions/ffs.{}", self.profile.name)),
        )
    }

    fn mount_functionfs(&mut self) -> io::Result<()> {
        fs::create_dir_all(&self.profile.functionfs_mount)?;
        mount_filesystem(
            &self.profile.name,
            &self.profile.functionfs_mount,
            "functionfs",
            Some("uid=0,gid=0,rmode=0500,fmode=0600"),
        )?;
        self.functionfs_mounted = true;
        Ok(())
    }

    fn publish_functionfs(&self, personality: &Personality) -> io::Result<(File, Vec<File>)> {
        let inspection = functionfs::inspect(&personality.descriptors, &personality.strings)?;
        if inspection.endpoints.len() != personality.endpoints.len() {
            return invalid("projected endpoint topology changed during validation");
        }
        let mut ep0 = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(self.profile.functionfs_mount.join("ep0"))?;
        ep0.write_all(&personality.descriptors)?;
        ep0.write_all(&personality.strings)?;
        let mut files = Vec::new();
        for (index, endpoint) in inspection.endpoints.iter().enumerate() {
            let path = self
                .profile
                .functionfs_mount
                .join(format!("ep{}", index + 1));
            let mut options = OpenOptions::new();
            match endpoint.direction {
                Direction::Out => {
                    options.read(true);
                }
                Direction::In => {
                    options.write(true);
                }
            }
            files.push(options.open(path)?);
        }
        Ok((ep0, files))
    }

    fn link_function(&self) -> io::Result<()> {
        let name = format!("ffs.{}", self.profile.name);
        std::os::unix::fs::symlink(
            self.gadget.join("functions").join(&name),
            self.gadget.join("configs/c.1").join(name),
        )
    }

    fn open_resources(&self) -> io::Result<Vec<File>> {
        self.profile
            .resources
            .iter()
            .map(|resource| match resource {
                ResourceProfile::CharacterDevice(resource) => self.open_character_device(resource),
                ResourceProfile::GpioLines(resource) => self.request_gpio_lines(resource),
            })
            .collect()
    }

    fn open_character_device(&self, resource: &CharacterDeviceResource) -> io::Result<File> {
        validate_character_device(&resource.name, &resource.path)?;
        let mut options = OpenOptions::new();
        match resource.access {
            ResourceAccess::Read => {
                options.read(true);
            }
            ResourceAccess::Write => {
                options.write(true);
            }
            ResourceAccess::ReadWrite => {
                options.read(true).write(true);
            }
        }
        options.open(&resource.path)
    }

    fn request_gpio_lines(&self, resource: &GpioLinesResource) -> io::Result<File> {
        validate_character_device(&resource.name, &resource.path)?;
        let chip = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&resource.path)?;
        gpiocdev_uapi::v2::get_line(&chip, gpio_line_request(resource)).map_err(|error| {
            io::Error::other(format!("request GPIO resource {}: {error}", resource.name))
        })
    }

    fn unbind(&mut self) -> io::Result<()> {
        let path = self.gadget.join("UDC");
        if !path.exists() {
            return Ok(());
        }
        match fs::write(&path, "\n") {
            Ok(()) => {
                self.detached_at = Some(Instant::now());
                Ok(())
            }
            Err(error) if error.raw_os_error() == Some(libc::ENODEV) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn wait_for_reconnect_dwell(&mut self) {
        let Some(detached_at) = self.detached_at.take() else {
            return;
        };
        let remaining = USB_RECONNECT_DWELL.saturating_sub(detached_at.elapsed());
        if !remaining.is_zero() {
            thread::sleep(remaining);
        }
    }

    fn remove_gadget_tree(&self) -> io::Result<()> {
        let function = format!("ffs.{}", self.profile.name);
        remove_file_if_exists(&self.gadget.join("os_desc/c.1"))?;
        remove_file_if_exists(&self.gadget.join("configs/c.1").join(&function))?;
        remove_dir_if_exists(&self.gadget.join("functions").join(function))?;
        remove_dir_if_exists(&self.gadget.join("configs/c.1/strings/0x409"))?;
        remove_dir_if_exists(&self.gadget.join("configs/c.1"))?;
        remove_dir_if_exists(&self.gadget.join("strings/0x409"))?;
        remove_dir_if_exists(&self.gadget)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!("usb-gadget-supervisor: cleanup failed: {error}");
        }
    }
}

fn resource_names(resources: &[ResourceProfile]) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    body.extend_from_slice(
        &u16::try_from(resources.len())
            .map_err(|_| io::Error::other("too many resources"))?
            .to_be_bytes(),
    );
    for resource in resources {
        let name = resource.name().as_bytes();
        body.extend_from_slice(
            &u16::try_from(name.len())
                .map_err(|_| io::Error::other("resource name too long"))?
                .to_be_bytes(),
        );
        body.extend_from_slice(name);
    }
    Ok(body)
}

fn endpoint_map(personality: &Personality) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    body.extend_from_slice(
        &u16::try_from(personality.endpoints.len())
            .map_err(|_| io::Error::other("too many USB endpoints"))?
            .to_be_bytes(),
    );
    for endpoint in &personality.endpoints {
        body.push(endpoint.address);
        body.push(endpoint.transfer_type);
        body.extend_from_slice(&endpoint.max_packet_size.to_be_bytes());
    }
    Ok(body)
}

fn expect_record(
    channel: &UnixStream,
    kind: Kind,
    generation: u32,
    request_id: u32,
) -> io::Result<()> {
    let record = protocol::receive(channel)?;
    if record.kind != kind
        || record.generation != generation
        || record.request_id != request_id
        || !record.body.is_empty()
        || !record.descriptors.is_empty()
    {
        return invalid(format!(
            "worker returned {:?} instead of {kind:?}",
            record.kind
        ));
    }
    Ok(())
}

fn transfer_ep0(fd: i32, buffer: &mut [u8], write_transfer: bool) -> io::Result<()> {
    let mut offset = 0;
    loop {
        let (pointer, length) = if buffer.is_empty() {
            (std::ptr::null_mut(), 0)
        } else {
            (
                unsafe { buffer.as_mut_ptr().add(offset) }.cast::<c_void>(),
                buffer.len() - offset,
            )
        };
        let transferred = unsafe {
            if write_transfer {
                libc::write(fd, pointer.cast_const(), length)
            } else {
                libc::read(fd, pointer, length)
            }
        };
        if transferred >= 0 {
            let transferred = transferred as usize;
            if write_transfer {
                return if transferred == length {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "short FunctionFS EP0 write",
                    ))
                };
            }
            offset += transferred;
            if offset == buffer.len() {
                return Ok(());
            }
            if transferred == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "short FunctionFS EP0 read",
                ));
            }
            continue;
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(error);
        }
        let mut waiter = libc::pollfd {
            fd,
            events: if write_transfer {
                libc::POLLOUT
            } else {
                libc::POLLIN
            },
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut waiter, 1, -1) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if waiter.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::from_raw_os_error(libc::ESHUTDOWN));
        }
    }
}

fn control_request_cancelled(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::EIDRM)
}

fn stall_ep0(fd: i32, direction_in: bool) -> io::Result<()> {
    let result = unsafe {
        if direction_in {
            libc::read(fd, std::ptr::null_mut(), 0)
        } else {
            libc::write(fd, std::ptr::null(), 0)
        }
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EL2HLT) {
            return Ok(());
        }
        return Err(error);
    }
    invalid("FunctionFS did not stall EP0")
}

fn gpio_line_request(resource: &GpioLinesResource) -> gpiocdev_uapi::v2::LineRequest {
    use gpiocdev_uapi::v2::{LineConfig, LineFlags, LineRequest, LineValues, Offsets};
    let mut flags = match resource.direction {
        GpioDirection::Input => LineFlags::INPUT,
        GpioDirection::Output => LineFlags::OUTPUT,
    };
    if resource.active_low {
        flags |= LineFlags::ACTIVE_LOW;
    }
    flags |= match resource.bias {
        Some(GpioBias::PullUp) => LineFlags::BIAS_PULL_UP,
        Some(GpioBias::PullDown) => LineFlags::BIAS_PULL_DOWN,
        Some(GpioBias::Disabled) => LineFlags::BIAS_DISABLED,
        None => LineFlags::empty(),
    };
    flags |= match resource.edge {
        Some(GpioEdge::Rising) => LineFlags::EDGE_RISING,
        Some(GpioEdge::Falling) => LineFlags::EDGE_FALLING,
        Some(GpioEdge::Both) => LineFlags::EDGE_RISING | LineFlags::EDGE_FALLING,
        None => LineFlags::empty(),
    };
    let mut config = LineConfig {
        flags,
        ..Default::default()
    };
    if let Some(values) = &resource.initial_values {
        config.add_values(&LineValues::from_slice(values));
    }
    LineRequest {
        offsets: Offsets::from_slice(&resource.offsets),
        consumer: resource.name.as_str().into(),
        config,
        num_lines: resource.offsets.len() as u32,
        ..Default::default()
    }
}

fn drain_signal_notifications(fd: i32) -> io::Result<()> {
    let mut bytes = [0_u8; 64];
    loop {
        let length = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        if length > 0 {
            continue;
        }
        if length == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(());
        }
        return Err(error);
    }
}

fn validate_character_device(name: &str, path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("inspect resource {name} at {}: {error}", path.display()),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_char_device() {
        return invalid(format!(
            "resource {name} at {} must be a non-symlink character device",
            path.display()
        ));
    }
    Ok(())
}

fn validate_root_owned_file(path: &Path, label: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != 0
        || metadata.mode() & 0o6022 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{label} {} must be a root-owned, non-set-ID, non-writable regular file",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn validate_worker_executable(path: &Path, identity: &WorkerIdentity) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    let mode = metadata.mode();
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || (metadata.uid() != 0 && metadata.uid() != identity.uid)
        || mode & 0o6002 != 0
        || (mode & 0o0020 != 0 && metadata.gid() != identity.gid)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "worker {} has unsafe ownership or permissions",
                path.display()
            ),
        ));
    }
    let executable = if metadata.uid() == identity.uid {
        0o100
    } else if metadata.gid() == identity.gid {
        0o010
    } else {
        0o001
    };
    if mode & executable == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "worker {} is not executable by {}",
                path.display(),
                identity.name
            ),
        ));
    }
    Ok(())
}

fn resolve_worker_identity(name: &str) -> io::Result<WorkerIdentity> {
    let uid = query_account_id("-u", name)?;
    let gid = query_account_id("-g", name)?;
    if uid == 0 {
        return invalid("worker account must not be root");
    }
    Ok(WorkerIdentity {
        name: name.to_owned(),
        uid,
        gid,
    })
}

fn query_account_id(flag: &str, name: &str) -> io::Result<u32> {
    let output = Command::new("/usr/bin/id")
        .args([flag, "--", name])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("cannot resolve worker account {name}"),
        ));
    }
    std::str::from_utf8(&output.stdout)
        .map_err(io::Error::other)?
        .trim()
        .parse()
        .map_err(io::Error::other)
}

fn acquire_lock() -> io::Result<File> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(LOCK_FILE)?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(lock)
}

fn ensure_configfs() -> io::Result<bool> {
    fs::create_dir_all(CONFIGFS)?;
    let mounted = if is_mounted_as(Path::new(CONFIGFS), "configfs")? {
        false
    } else {
        mount_filesystem("none", Path::new(CONFIGFS), "configfs", None)?;
        true
    };
    if !Path::new(GADGET_ROOT).is_dir() {
        let status = Command::new("modprobe").arg("libcomposite").status()?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "modprobe libcomposite exited with {status}"
            )));
        }
    }
    if !Path::new(GADGET_ROOT).is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "ConfigFS USB gadget support is unavailable",
        ));
    }
    Ok(mounted)
}

fn select_udc(requested: Option<&str>) -> io::Result<String> {
    let mut names = fs::read_dir("/sys/class/udc")?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect::<Vec<_>>();
    names.sort();
    if let Some(requested) = requested {
        if names.iter().any(|name| name == requested) {
            return Ok(requested.to_owned());
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("UDC {requested} is unavailable"),
        ));
    }
    names
        .into_iter()
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no USB device controller found"))
}

fn prepare_owned_directory(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return invalid(format!("{} is not a real directory", path.display()));
    }
    chown(path, Some(uid), Some(gid))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn seqpacket_pair() -> io::Result<(UnixStream, UnixStream)> {
    let mut fds = [-1; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe {
        (
            UnixStream::from_raw_fd(fds[0]),
            UnixStream::from_raw_fd(fds[1]),
        )
    })
}

fn stop_worker(worker: &mut Option<Child>) -> io::Result<()> {
    let Some(mut child) = worker.take() else {
        return Ok(());
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait()?.is_none() {
        eprintln!(
            "usb-gadget-supervisor: worker pid {} did not exit after control-channel closure; sending SIGTERM",
            child.id()
        );
        signal_child(&child, libc::SIGTERM)?;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait()?.is_none() {
        eprintln!(
            "usb-gadget-supervisor: worker pid {} did not exit after SIGTERM; sending SIGKILL",
            child.id()
        );
        signal_child(&child, libc::SIGKILL)?;
    }
    child.wait().map(|_| ())
}

fn signal_child(child: &Child, signal: libc::c_int) -> io::Result<()> {
    if unsafe { libc::kill(child.id() as _, signal) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}

fn write_attribute(path: &Path, value: &str) -> io::Result<()> {
    fs::write(path, value)
        .map_err(|error| io::Error::new(error.kind(), format!("write {}: {error}", path.display())))
}

fn mount_filesystem(
    source: &str,
    target: &Path,
    filesystem: &str,
    options: Option<&str>,
) -> io::Result<()> {
    let source = CString::new(source)?;
    let target = CString::new(target.as_os_str().as_bytes())?;
    let filesystem = CString::new(filesystem)?;
    let options = options.map(CString::new).transpose()?;
    let data = options
        .as_ref()
        .map_or(std::ptr::null(), |value| value.as_ptr().cast::<c_void>());
    if unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            filesystem.as_ptr(),
            0,
            data,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn unmount_filesystem(target: &Path, filesystem: &str) -> io::Result<bool> {
    if !is_mounted_as(target, filesystem)? {
        return Ok(false);
    }
    let target = CString::new(target.as_os_str().as_bytes())?;
    if unsafe { libc::umount2(target.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

fn is_mounted_as(target: &Path, filesystem: &str) -> io::Result<bool> {
    let mounts = fs::read_to_string("/proc/self/mounts")?;
    let target = target.to_string_lossy();
    Ok(mounts.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let _ = fields.next();
        fields.next() == Some(target.as_ref()) && fields.next() == Some(filesystem)
    }))
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
fn remove_dir_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
fn record_error(first: &mut Option<io::Error>, result: io::Result<()>) {
    if let Err(error) = result {
        if first.is_none() {
            *first = Some(error);
        }
    }
}
fn invalid<T>(message: impl Into<String>) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}
