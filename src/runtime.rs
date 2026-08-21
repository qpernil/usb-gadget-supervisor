//! Privileged Linux ConfigFS, FunctionFS, UDC, and worker lifecycle.

use crate::functionfs::{self, Direction};
use crate::profile::{
    decode_hex_blob, decode_hex_descriptor, CharacterDeviceResource, FunctionProfile, GpioBias,
    GpioDirection, GpioEdge, GpioLinesResource, HidFunction, Profile, ResourceAccess,
    ResourceProfile,
};
use crate::protocol::{
    Message, CONTROL_FD, PACKET_LENGTH, RUNTIME_DIRECTORY_ENV, STATE_DIRECTORY_ENV,
};
use crate::{RESTART_REQUESTED, STOP_REQUESTED};
use std::ffi::{c_void, CString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{chown, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

const CONFIGFS: &str = "/sys/kernel/config";
const GADGET_ROOT: &str = "/sys/kernel/config/usb_gadget";
const LOCK_FILE: &str = "/run/lock/usb-gadget-supervisor.lock";

struct WorkerIdentity {
    name: String,
    uid: u32,
    gid: u32,
}

pub(crate) struct Runtime {
    profile_path: PathBuf,
    profile: Profile,
    identity: WorkerIdentity,
    gadget: PathBuf,
    _lock: File,
    configfs_mounted_by_us: bool,
    owns_gadget: bool,
    mounted_functionfs: Vec<PathBuf>,
    worker: Option<Child>,
    control: Option<UnixStream>,
    udc: Option<String>,
    incarnation: u64,
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
                "USB gadget setup needs root; run the supervisor through systemd or sudo",
            ));
        }

        validate_root_owned_file(&profile_path, "profile")?;
        let identity = resolve_worker_identity(&profile.worker.run_as)?;
        validate_worker_executable(&profile.worker.command, &identity)?;
        for function in &profile.functions {
            if let FunctionProfile::Hid(hid) = function {
                if let Some(path) = &hid.report_descriptor {
                    validate_root_owned_file(path, "HID report descriptor")?;
                }
            }
        }

        let lock = acquire_lock()?;
        let configfs_mounted_by_us = ensure_configfs()?;
        let gadget = Path::new(GADGET_ROOT).join(&profile.name);
        let mut runtime = Self {
            profile_path,
            profile,
            identity,
            gadget,
            _lock: lock,
            configfs_mounted_by_us,
            owns_gadget: false,
            mounted_functionfs: Vec::new(),
            worker: None,
            control: None,
            udc: Some(select_udc(requested_udc)?),
            incarnation: 0,
            cleaned: false,
        };

        runtime.cleanup_stale_state()?;
        runtime.start_incarnation()?;
        Ok(runtime)
    }

    pub(crate) fn serve(&mut self, signal_fd: i32) -> io::Result<()> {
        while !STOP_REQUESTED.load(Ordering::Relaxed) {
            let mut replacement = None;
            let mut restart_reason = None;
            if RESTART_REQUESTED.swap(false, Ordering::Relaxed) {
                match self.load_replacement_profile() {
                    Ok(loaded) => {
                        replacement = Some(loaded);
                        restart_reason =
                            Some("SIGHUP requested a validated profile reload".to_owned());
                    }
                    Err(error) => {
                        eprintln!(
                            "usb-gadget-supervisor: rejected SIGHUP profile reload; current incarnation remains active: {error}"
                        );
                        continue;
                    }
                }
            }
            if restart_reason.is_none() {
                if let Some(status) = self.worker.as_mut().expect("worker exists").try_wait()? {
                    restart_reason = Some(format!("worker exited with {status}"));
                }
            }
            if restart_reason.is_none() {
                match self.wait_for_control_activity(signal_fd) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        restart_reason = Some(format!("wait for worker lifecycle: {error}"));
                    }
                }
            }
            if STOP_REQUESTED.load(Ordering::Relaxed) {
                continue;
            }
            if restart_reason.is_none() {
                if let Some(status) = self.worker.as_mut().expect("worker exists").try_wait()? {
                    restart_reason = Some(format!("worker exited with {status}"));
                } else {
                    match self.receive() {
                        Ok((message, count)) => {
                            restart_reason = Some(format!(
                                "unexpected runtime control message {message:?} with {count} descriptors"
                            ));
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            restart_reason = Some(format!("control channel ended: {error}"));
                        }
                    }
                }
            }
            if let Some(reason) = restart_reason {
                eprintln!(
                    "usb-gadget-supervisor: incarnation {} ended ({reason}); rebuilding",
                    self.incarnation
                );
                self.cleanup_incarnation()?;
                if STOP_REQUESTED.load(Ordering::Relaxed) {
                    break;
                }
                if let Some((profile, identity)) = replacement {
                    self.gadget = Path::new(GADGET_ROOT).join(&profile.name);
                    self.profile = profile;
                    self.identity = identity;
                    println!(
                        "usb-gadget-supervisor: accepted reloaded profile {}",
                        self.profile.name
                    );
                }
                thread::sleep(Duration::from_millis(250));
                self.start_incarnation()?;
            }
        }
        Ok(())
    }

    pub(crate) fn cleanup(&mut self) -> io::Result<()> {
        if self.cleaned {
            return Ok(());
        }

        let mut first_error = self.cleanup_incarnation().err();
        if self.configfs_mounted_by_us {
            record_error(
                &mut first_error,
                unmount_filesystem(Path::new(CONFIGFS), "configfs").map(|_| ()),
            );
            self.configfs_mounted_by_us = false;
        }

        self.cleaned = true;
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn start_incarnation(&mut self) -> io::Result<()> {
        self.incarnation += 1;
        println!(
            "usb-gadget-supervisor: starting worker incarnation {} for profile {}",
            self.incarnation, self.profile.name
        );
        fs::create_dir(&self.gadget)?;
        self.owns_gadget = true;
        self.populate_gadget()?;
        self.prepare_worker_directories()?;
        self.mount_functionfs()?;
        let mut prebind = self.publish_and_open_functionfs()?;
        prebind.extend(self.open_resources()?);
        self.spawn_worker(&prebind)?;
        drop(prebind);
        self.link_functions()?;
        let udc = self.udc.clone().expect("UDC selected during setup");
        write_attribute(&self.gadget.join("UDC"), &udc)?;
        let postbind = self.open_hid_devices()?;
        self.send_files(Message::PostbindResources, &postbind)?;
        drop(postbind);
        self.expect(Message::Serving)?;
        self.control
            .as_ref()
            .expect("control channel exists")
            .set_read_timeout(None)?;
        println!(
            "USB gadget profile {} attached through UDC {} as {:04x}:{:04x}; incarnation {} is serving as user {}",
            self.profile.name,
            udc,
            self.profile.usb.vendor_id,
            self.profile.usb.product_id,
            self.incarnation,
            self.identity.name,
        );
        Ok(())
    }

    fn cleanup_incarnation(&mut self) -> io::Result<()> {
        let mut first_error = None;
        if self.owns_gadget {
            record_error(&mut first_error, self.unbind());
        }
        self.control = None;
        record_error(&mut first_error, stop_worker(&mut self.worker));
        for mount in self.mounted_functionfs.iter().rev() {
            record_error(
                &mut first_error,
                unmount_filesystem(mount, "functionfs").map(|_| ()),
            );
        }
        self.mounted_functionfs.clear();
        if self.owns_gadget {
            record_error(&mut first_error, self.remove_gadget_tree());
            self.owns_gadget = false;
        }
        for function in &self.profile.functions {
            if let FunctionProfile::Functionfs(ffs) = function {
                record_error(&mut first_error, remove_dir_if_exists(&ffs.mount));
            }
        }
        record_error(
            &mut first_error,
            remove_dir_if_exists(&self.profile.worker.runtime_directory),
        );
        if self.incarnation != 0 {
            println!(
                "usb-gadget-supervisor: worker incarnation {} cleaned up",
                self.incarnation
            );
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn cleanup_stale_state(&mut self) -> io::Result<()> {
        if self.gadget.exists() {
            self.unbind()?;
        }
        for function in &self.profile.functions {
            if let FunctionProfile::Functionfs(ffs) = function {
                unmount_filesystem(&ffs.mount, "functionfs")?;
            }
        }
        if self.gadget.exists() {
            self.remove_gadget_tree()?;
        }
        for function in &self.profile.functions {
            if let FunctionProfile::Functionfs(ffs) = function {
                remove_dir_if_exists(&ffs.mount)?;
            }
        }
        Ok(())
    }

    fn populate_gadget(&self) -> io::Result<()> {
        let usb = &self.profile.usb;
        write_attribute(&self.gadget.join("max_speed"), &usb.max_speed)?;
        write_attribute(
            &self.gadget.join("idVendor"),
            &format!("0x{:04x}", usb.vendor_id),
        )?;
        write_attribute(
            &self.gadget.join("idProduct"),
            &format!("0x{:04x}", usb.product_id),
        )?;
        write_attribute(
            &self.gadget.join("bcdUSB"),
            &format!("0x{:04x}", usb.bcd_usb),
        )?;
        write_attribute(
            &self.gadget.join("bcdDevice"),
            &format!("0x{:04x}", usb.bcd_device),
        )?;
        write_attribute(
            &self.gadget.join("bDeviceClass"),
            &format!("0x{:02x}", usb.device_class),
        )?;
        write_attribute(
            &self.gadget.join("bDeviceSubClass"),
            &format!("0x{:02x}", usb.device_subclass),
        )?;
        write_attribute(
            &self.gadget.join("bDeviceProtocol"),
            &format!("0x{:02x}", usb.device_protocol),
        )?;

        let strings = self.gadget.join("strings/0x409");
        fs::create_dir(&strings)?;
        write_attribute(&strings.join("manufacturer"), &usb.manufacturer)?;
        write_attribute(&strings.join("product"), &usb.product)?;
        if let Some(serial) = &usb.serial {
            write_attribute(&strings.join("serialnumber"), serial)?;
        }

        let config = self.gadget.join("configs/c.1");
        fs::create_dir(&config)?;
        write_attribute(&config.join("MaxPower"), &usb.max_power_ma.to_string())?;

        if let Some(microsoft) = &usb.microsoft_os_1 {
            let os_desc = self.gadget.join("os_desc");
            write_attribute(
                &os_desc.join("b_vendor_code"),
                &format!("0x{:02x}", microsoft.vendor_code),
            )?;
            write_attribute(&os_desc.join("qw_sign"), &microsoft.signature)?;
            write_attribute(&os_desc.join("use"), "1")?;
            std::os::unix::fs::symlink(&config, os_desc.join("c.1"))?;
        }

        if let Some(webusb) = &usb.webusb {
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

        for function in &self.profile.functions {
            match function {
                FunctionProfile::Hid(hid) => {
                    let directory = self.gadget.join(format!("functions/hid.{}", hid.name));
                    fs::create_dir(&directory)?;
                    write_attribute(&directory.join("protocol"), &hid.protocol.to_string())?;
                    write_attribute(&directory.join("subclass"), &hid.subclass.to_string())?;
                    write_attribute(
                        &directory.join("report_length"),
                        &hid.report_length.to_string(),
                    )?;
                    fs::write(directory.join("report_desc"), hid_report_descriptor(hid)?)?;
                }
                FunctionProfile::Functionfs(ffs) => {
                    fs::create_dir(self.gadget.join(format!("functions/ffs.{}", ffs.name)))?;
                }
            }
        }
        Ok(())
    }

    fn prepare_worker_directories(&self) -> io::Result<()> {
        prepare_owned_directory(
            &self.profile.worker.state_directory,
            self.identity.uid,
            self.identity.gid,
        )?;
        prepare_owned_directory(
            &self.profile.worker.runtime_directory,
            self.identity.uid,
            self.identity.gid,
        )
    }

    fn mount_functionfs(&mut self) -> io::Result<()> {
        for function in &self.profile.functions {
            if let FunctionProfile::Functionfs(ffs) = function {
                fs::create_dir_all(&ffs.mount)?;
                let options = "uid=0,gid=0,rmode=0500,fmode=0600";
                mount_filesystem(&ffs.name, &ffs.mount, "functionfs", Some(options))?;
                self.mounted_functionfs.push(ffs.mount.clone());
            }
        }
        Ok(())
    }

    fn publish_and_open_functionfs(&self) -> io::Result<Vec<File>> {
        let mut files = Vec::new();
        for function in &self.profile.functions {
            let FunctionProfile::Functionfs(ffs) = function else {
                continue;
            };
            let descriptors = decode_hex_blob(
                &ffs.descriptors_hex,
                &format!("FunctionFS {} descriptors", ffs.name),
            )?;
            let strings = decode_hex_blob(
                &ffs.strings_hex,
                &format!("FunctionFS {} strings", ffs.name),
            )?;
            let inspection = functionfs::inspect(&descriptors, &strings)?;
            let mut ep0 = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(ffs.mount.join("ep0"))?;
            ep0.write_all(&descriptors)?;
            ep0.write_all(&strings)?;
            files.push(ep0);
            for (index, endpoint) in inspection.endpoints.iter().enumerate() {
                let path = ffs.mount.join(format!("ep{}", index + 1));
                let metadata = fs::symlink_metadata(&path)?;
                if metadata.file_type().is_symlink() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "FunctionFS endpoint {} must not be a symlink",
                            path.display()
                        ),
                    ));
                }
                let mut options = OpenOptions::new();
                match endpoint.direction {
                    Direction::Out => {
                        options.read(true);
                    }
                    Direction::In => {
                        options.write(true);
                    }
                }
                files.push(options.custom_flags(libc::O_NONBLOCK).open(path)?);
            }
            println!(
                "usb-gadget-supervisor: published FunctionFS {} with {} data endpoints",
                ffs.name,
                inspection.endpoints.len()
            );
        }
        Ok(files)
    }

    fn spawn_worker(&mut self, prebind: &[File]) -> io::Result<()> {
        let (mut supervisor, worker_control) = seqpacket_pair()?;
        supervisor.set_read_timeout(Some(Duration::from_millis(
            self.profile.worker.readiness_timeout_ms,
        )))?;
        let control_fd = worker_control.as_raw_fd();
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

        let mut child = command.spawn()?;
        drop(worker_control);
        if let Err(error) =
            send_message_with_files(&mut supervisor, Message::PrebindResources, prebind)
                .and_then(|_| expect_message(&mut supervisor, Message::Prepared))
        {
            let _ = terminate_child(&mut child);
            return Err(io::Error::new(
                error.kind(),
                format!("device worker did not become ready: {error}"),
            ));
        }

        self.worker = Some(child);
        self.control = Some(supervisor);
        Ok(())
    }

    fn open_resources(&self) -> io::Result<Vec<File>> {
        let mut opened = Vec::new();
        for resource in &self.profile.resources {
            let handle = match resource {
                ResourceProfile::CharacterDevice(resource) => {
                    self.open_character_device(resource)?
                }
                ResourceProfile::GpioLines(resource) => self.request_gpio_lines(resource)?,
            };
            opened.push(handle);
        }
        Ok(opened)
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
        let file = options.open(&resource.path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "open resource {} at {}: {error}",
                    resource.name,
                    resource.path.display()
                ),
            )
        })?;
        println!(
            "usb-gadget-supervisor: opened required character-device resource {} at {}",
            resource.name,
            resource.path.display()
        );
        Ok(file)
    }

    fn request_gpio_lines(&self, resource: &GpioLinesResource) -> io::Result<File> {
        use gpiocdev_uapi::v2;

        validate_character_device(&resource.name, &resource.path)?;
        let chip = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&resource.path)
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "open GPIO chip for resource {} at {}: {error}",
                        resource.name,
                        resource.path.display()
                    ),
                )
            })?;

        let lines = v2::get_line(&chip, gpio_line_request(resource)).map_err(|error| {
            io::Error::other(format!(
                "request GPIO resource {} at {} offsets {:?}: {error}",
                resource.name,
                resource.path.display(),
                resource.offsets
            ))
        })?;
        println!(
            "usb-gadget-supervisor: claimed required GPIO resource {} at {} offsets {:?}",
            resource.name,
            resource.path.display(),
            resource.offsets
        );
        Ok(lines)
    }

    fn link_functions(&self) -> io::Result<()> {
        for function in &self.profile.functions {
            let directory = match function {
                FunctionProfile::Hid(hid) => format!("hid.{}", hid.name),
                FunctionProfile::Functionfs(ffs) => format!("ffs.{}", ffs.name),
            };
            std::os::unix::fs::symlink(
                self.gadget.join("functions").join(&directory),
                self.gadget.join("configs/c.1").join(directory),
            )?;
        }
        Ok(())
    }

    fn open_hid_devices(&self) -> io::Result<Vec<File>> {
        let mut opened = Vec::new();
        for function in &self.profile.functions {
            if let FunctionProfile::Hid(hid) = function {
                wait_for_device(&hid.device, Duration::from_secs(5))?;
                opened.push(
                    OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&hid.device)?,
                );
            }
        }
        Ok(opened)
    }

    fn send_files(&mut self, message: Message, files: &[File]) -> io::Result<()> {
        send_message_with_files(
            self.control.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "worker channel closed")
            })?,
            message,
            files,
        )
    }

    fn expect(&mut self, expected: Message) -> io::Result<()> {
        expect_message(
            self.control.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "worker channel closed")
            })?,
            expected,
        )
    }

    fn receive(&mut self) -> io::Result<(Message, u16)> {
        receive_message(
            self.control.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "worker channel closed")
            })?,
        )
    }

    fn wait_for_control_activity(&self, signal_fd: i32) -> io::Result<()> {
        let mut descriptors = [
            libc::pollfd {
                fd: self
                    .control
                    .as_ref()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "worker channel closed")
                    })?
                    .as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            },
            libc::pollfd {
                fd: signal_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, -1) };
        if ready < 0 {
            return Err(io::Error::last_os_error());
        }
        if descriptors[1].revents != 0 {
            drain_signal_notifications(signal_fd)?;
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        Ok(())
    }

    fn load_replacement_profile(&self) -> io::Result<(Profile, WorkerIdentity)> {
        validate_root_owned_file(&self.profile_path, "profile")?;
        let profile = Profile::load(&self.profile_path)?;
        let identity = resolve_worker_identity(&profile.worker.run_as)?;
        validate_worker_executable(&profile.worker.command, &identity)?;
        for function in &profile.functions {
            if let FunctionProfile::Hid(hid) = function {
                if let Some(path) = &hid.report_descriptor {
                    validate_root_owned_file(path, "HID report descriptor")?;
                }
            }
        }
        Ok((profile, identity))
    }

    fn unbind(&self) -> io::Result<()> {
        let path = self.gadget.join("UDC");
        if !path.exists() {
            return Ok(());
        }
        match fs::write(&path, "\n") {
            Ok(()) => Ok(()),
            Err(error) if error.raw_os_error() == Some(libc::ENODEV) => Ok(()),
            Err(error) => Err(io::Error::new(
                error.kind(),
                format!("write {}: {error}", path.display()),
            )),
        }
    }

    fn remove_gadget_tree(&self) -> io::Result<()> {
        remove_file_if_exists(&self.gadget.join("os_desc/c.1"))?;
        for function in self.profile.functions.iter().rev() {
            let directory = match function {
                FunctionProfile::Hid(hid) => format!("hid.{}", hid.name),
                FunctionProfile::Functionfs(ffs) => format!("ffs.{}", ffs.name),
            };
            remove_file_if_exists(&self.gadget.join("configs/c.1").join(&directory))?;
            remove_dir_if_exists(&self.gadget.join("functions").join(directory))?;
        }
        remove_dir_if_exists(&self.gadget.join("configs/c.1/strings/0x409"))?;
        remove_dir_if_exists(&self.gadget.join("configs/c.1"))?;
        remove_dir_if_exists(&self.gadget.join("strings/0x409"))?;
        remove_dir_if_exists(&self.gadget)
    }
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

fn drain_signal_notifications(descriptor: i32) -> io::Result<()> {
    let mut bytes = [0_u8; 64];
    loop {
        let length =
            unsafe { libc::read(descriptor, bytes.as_mut_ptr().cast::<c_void>(), bytes.len()) };
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

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!("usb-gadget-supervisor: cleanup failed: {error}");
        }
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
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "resource {name} at {} must be a non-symlink character device",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn validate_root_owned_file(path: &Path, label: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("inspect {label} {}: {error}", path.display()),
        )
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::other(format!(
            "{label} {} must be a regular non-symlink file",
            path.display()
        )));
    }
    if metadata.uid() != 0 || metadata.mode() & 0o6022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{label} {} must be root-owned, non-set-ID, and not group/world writable",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn validate_worker_executable(path: &Path, identity: &WorkerIdentity) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("inspect worker executable {}: {error}", path.display()),
        )
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::other(format!(
            "worker executable {} must be a regular non-symlink file",
            path.display()
        )));
    }
    let mode = metadata.mode();
    let invalid_owner = metadata.uid() != 0 && metadata.uid() != identity.uid;
    let set_id = mode & 0o6000 != 0;
    let world_writable = mode & 0o0002 != 0;
    let foreign_group_writable = mode & 0o0020 != 0 && metadata.gid() != identity.gid;
    if invalid_owner || set_id || world_writable || foreign_group_writable {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "worker executable {} must be owned by root or {}, non-set-ID, not world writable, and writable by no group other than the worker's primary group",
                path.display(), identity.name
            ),
        ));
    }

    let executable_bit = if metadata.uid() == identity.uid {
        0o100
    } else if metadata.gid() == identity.gid {
        0o010
    } else {
        0o001
    };
    if mode & executable_bit == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "worker executable {} is not executable by {}",
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
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the worker account must not be root",
        ));
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
            format!("cannot resolve worker account {name:?}"),
        ));
    }
    std::str::from_utf8(&output.stdout)
        .map_err(|_| io::Error::other("id returned non-UTF-8 output"))?
        .trim()
        .parse()
        .map_err(|_| io::Error::other(format!("id returned an invalid ID for {name:?}")))
}

fn acquire_lock() -> io::Result<File> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(LOCK_FILE)?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another USB gadget supervisor owns the UDC lifecycle lock",
            ));
        }
        return Err(error);
    }
    Ok(lock)
}

fn ensure_configfs() -> io::Result<bool> {
    fs::create_dir_all(CONFIGFS)?;
    let mut mounted_by_us = false;
    if !is_mounted_as(Path::new(CONFIGFS), "configfs")? {
        mount_filesystem("none", Path::new(CONFIGFS), "configfs", None)?;
        mounted_by_us = true;
    }
    let result = (|| {
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
                "configfs USB gadget support is unavailable",
            ));
        }
        Ok(mounted_by_us)
    })();
    if result.is_err() && mounted_by_us {
        let _ = unmount_filesystem(Path::new(CONFIGFS), "configfs");
    }
    result
}

fn select_udc(requested: Option<&str>) -> io::Result<String> {
    let mut available = fs::read_dir("/sys/class/udc")?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect::<Vec<_>>();
    available.sort();
    if let Some(name) = requested {
        if available.iter().any(|candidate| candidate == name) {
            return Ok(name.to_owned());
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "requested UDC {name:?} is unavailable; found: {}",
                available.join(", ")
            ),
        ));
    }
    available.into_iter().next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no USB device controller found; enable peripheral mode and reboot",
        )
    })
}

fn prepare_owned_directory(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::other(format!(
            "{} is not a real directory",
            path.display()
        )));
    }
    chown(path, Some(uid), Some(gid))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn wait_for_device(path: &Path, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match fs::symlink_metadata(path) {
            Ok(metadata)
                if metadata.file_type().is_char_device() && !metadata.file_type().is_symlink() =>
            {
                return Ok(());
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not a non-symlink character device", path.display()),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} did not appear after UDC binding", path.display()),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn hid_report_descriptor(hid: &HidFunction) -> io::Result<Vec<u8>> {
    match (&hid.report_descriptor, &hid.report_descriptor_hex) {
        (Some(path), None) => {
            let source = fs::read_to_string(path)?;
            decode_hex_descriptor(&source, &path.display().to_string())
        }
        (None, Some(source)) => decode_hex_descriptor(source, "inline HID report descriptor"),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "HID report descriptor source was not validated",
        )),
    }
}

fn seqpacket_pair() -> io::Result<(UnixStream, UnixStream)> {
    let mut descriptors = [-1; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            descriptors.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe {
        (
            UnixStream::from_raw_fd(descriptors[0]),
            UnixStream::from_raw_fd(descriptors[1]),
        )
    })
}

fn send_message_with_files<T: AsRawFd>(
    channel: &mut UnixStream,
    message: Message,
    files: &[T],
) -> io::Result<()> {
    let descriptor_count = u16::try_from(files.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many descriptors for one worker-control packet",
        )
    })?;
    let packet = message.encode(descriptor_count);
    let mut iovec = libc::iovec {
        iov_base: packet.as_ptr().cast::<c_void>().cast_mut(),
        iov_len: packet.len(),
    };
    let raw_descriptors = files.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>();
    let control_length = if raw_descriptors.is_empty() {
        0
    } else {
        unsafe {
            libc::CMSG_SPACE(
                (raw_descriptors.len() * std::mem::size_of::<libc::c_int>()) as libc::c_uint,
            ) as usize
        }
    };
    let mut control = vec![0_u8; control_length];
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &mut iovec;
    header.msg_iovlen = 1;
    if !control.is_empty() {
        header.msg_control = control.as_mut_ptr().cast::<c_void>();
        header.msg_controllen = control.len();
        unsafe {
            let ancillary = libc::CMSG_FIRSTHDR(&header);
            (*ancillary).cmsg_level = libc::SOL_SOCKET;
            (*ancillary).cmsg_type = libc::SCM_RIGHTS;
            (*ancillary).cmsg_len = libc::CMSG_LEN(
                (raw_descriptors.len() * std::mem::size_of::<libc::c_int>()) as libc::c_uint,
            ) as usize;
            std::ptr::copy_nonoverlapping(
                raw_descriptors.as_ptr(),
                libc::CMSG_DATA(ancillary).cast::<libc::c_int>(),
                raw_descriptors.len(),
            );
        }
    }
    let length = unsafe { libc::sendmsg(channel.as_raw_fd(), &header, libc::MSG_NOSIGNAL) };
    if length < 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize != packet.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "worker-control packet was not sent atomically",
        ));
    }
    Ok(())
}

fn receive_message(channel: &mut UnixStream) -> io::Result<(Message, u16)> {
    let mut record = [0_u8; PACKET_LENGTH + 1];
    let length = unsafe {
        libc::recv(
            channel.as_raw_fd(),
            record.as_mut_ptr().cast::<c_void>(),
            record.len(),
            0,
        )
    };
    if length < 0 {
        return Err(io::Error::last_os_error());
    }
    if length == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "worker-control channel closed",
        ));
    }
    if length as usize != PACKET_LENGTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("worker-control record has invalid length {length}"),
        ));
    }
    Message::decode(record[..PACKET_LENGTH].try_into().unwrap())
}

fn expect_message(channel: &mut UnixStream, expected: Message) -> io::Result<()> {
    let (message, descriptor_count) = receive_message(channel)?;
    if message != expected || descriptor_count != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "worker sent {message:?} with {descriptor_count} descriptors instead of {expected:?}"
            ),
        ));
    }
    Ok(())
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
    terminate_child(&mut child)
}

fn terminate_child(child: &mut Child) -> io::Result<()> {
    if child.try_wait()?.is_none()
        && unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) } != 0
    {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    child.wait().map(|_| ())
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
    let target_c = CString::new(target.as_os_str().as_bytes())?;
    if unsafe { libc::umount2(target_c.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

fn is_mounted_as(target: &Path, filesystem: &str) -> io::Result<bool> {
    let mounts = fs::read_to_string("/proc/self/mounts")?;
    let target = target.to_string_lossy();
    Ok(mounts.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let _source = fields.next();
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

fn record_error(first_error: &mut Option<io::Error>, result: io::Result<()>) {
    if let Err(error) = result {
        if first_error.is_none() {
            *first_error = Some(error);
        }
    }
}

#[cfg(test)]
mod gpio_tests {
    use super::gpio_line_request;
    use crate::profile::{GpioDirection, GpioLinesResource};
    use gpiocdev_uapi::v2::LineAttributeValue;
    use std::path::PathBuf;

    #[test]
    fn request_preserves_profile_order_and_value_bits() {
        let resource = GpioLinesResource {
            name: "display-control".into(),
            path: PathBuf::from("/dev/gpiochip0"),
            offsets: vec![25, 27, 24],
            direction: GpioDirection::Output,
            active_low: false,
            bias: None,
            edge: None,
            initial_values: Some(vec![false, true, false]),
        };

        let request = gpio_line_request(&resource);
        assert_eq!(request.num_lines, 3);
        assert_eq!(request.offsets.get(0), 25);
        assert_eq!(request.offsets.get(1), 27);
        assert_eq!(request.offsets.get(2), 24);
        assert_eq!(request.config.attr(0).mask, 0b111);
        assert_eq!(
            request.config.attr(0).attr.to_value(),
            Some(LineAttributeValue::Values(0b010))
        );
    }
}
