//! Privileged Linux ConfigFS, FunctionFS, UDC, and worker lifecycle.

use crate::profile::{
    decode_hex_descriptor, FunctionProfile, HidFunction, Profile, ResourceAccess,
};
use crate::protocol::{
    Message, CONTROL_FD_ENV, FUNCTIONFS_ENV_PREFIX, HID_ENV_PREFIX, PACKET_LENGTH,
    RESOURCE_FD_ENV_PREFIX, RUNTIME_DIRECTORY_ENV, STATE_DIRECTORY_ENV,
};
use crate::STOP_REQUESTED;
use std::ffi::{c_void, CString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{chown, FileTypeExt, MetadataExt, PermissionsExt};
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
            profile,
            identity,
            gadget,
            _lock: lock,
            configfs_mounted_by_us,
            owns_gadget: false,
            mounted_functionfs: Vec::new(),
            worker: None,
            control: None,
            udc: None,
            cleaned: false,
        };

        runtime.cleanup_stale_state()?;
        fs::create_dir(&runtime.gadget)?;
        runtime.owns_gadget = true;
        runtime.populate_gadget()?;
        runtime.prepare_worker_directories()?;
        runtime.mount_functionfs()?;
        runtime.spawn_worker()?;
        runtime.link_functions()?;
        let udc = select_udc(requested_udc)?;
        write_attribute(&runtime.gadget.join("UDC"), &udc)?;
        runtime.udc = Some(udc.clone());
        runtime.prepare_hid_devices()?;
        runtime.send(Message::UsbAttached)?;
        if let Some(control) = runtime.control.as_ref() {
            control.set_read_timeout(Some(Duration::from_millis(100)))?;
        }

        println!(
            "USB gadget profile {} attached through UDC {} as {:04x}:{:04x}; worker is user {}",
            runtime.profile.name,
            udc,
            runtime.profile.usb.vendor_id,
            runtime.profile.usb.product_id,
            runtime.identity.name,
        );
        Ok(runtime)
    }

    pub(crate) fn serve(&mut self) -> io::Result<()> {
        while !STOP_REQUESTED.load(Ordering::Relaxed) {
            if let Some(status) = self
                .worker
                .as_mut()
                .expect("worker exists after setup")
                .try_wait()?
            {
                return Err(io::Error::other(format!(
                    "device worker exited unexpectedly with {status}"
                )));
            }

            match self.receive() {
                Ok(Message::ReconnectRequest) => self.reconnect()?,
                Ok(Message::Fatal) => {
                    return Err(io::Error::other("device worker reported a fatal error"));
                }
                Ok(Message::Stopped) => {
                    return Err(io::Error::other("device worker stopped unexpectedly"));
                }
                Ok(message) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected worker message while attached: {message:?}"),
                    ));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
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
        if self.control.is_some() {
            record_error(&mut first_error, self.send(Message::Shutdown));
        }
        record_error(&mut first_error, stop_worker(&mut self.worker));
        self.control = None;

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
                let options = format!(
                    "uid={},gid={},rmode=0500,fmode=0600",
                    self.identity.uid, self.identity.gid
                );
                mount_filesystem(&ffs.name, &ffs.mount, "functionfs", Some(&options))?;
                self.mounted_functionfs.push(ffs.mount.clone());
            }
        }
        Ok(())
    }

    fn spawn_worker(&mut self) -> io::Result<()> {
        let resources = self.open_resources()?;
        let (mut supervisor, worker_control) = seqpacket_pair()?;
        supervisor.set_read_timeout(Some(Duration::from_millis(
            self.profile.worker.readiness_timeout_ms,
        )))?;
        let control_fd = worker_control.as_raw_fd();
        let parent_pid = std::process::id() as libc::pid_t;
        let uid = self.identity.uid;
        let gid = self.identity.gid;
        let resource_fds = resources
            .iter()
            .map(|(_, file)| file.as_raw_fd())
            .collect::<Vec<_>>();

        let mut command = Command::new(&self.profile.worker.command);
        command
            .args(&self.profile.worker.arguments)
            .env_clear()
            .env(CONTROL_FD_ENV, control_fd.to_string())
            .env(STATE_DIRECTORY_ENV, &self.profile.worker.state_directory)
            .env(
                RUNTIME_DIRECTORY_ENV,
                &self.profile.worker.runtime_directory,
            )
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        for function in &self.profile.functions {
            match function {
                FunctionProfile::Functionfs(ffs) => {
                    command.env(
                        format!(
                            "{FUNCTIONFS_ENV_PREFIX}{}",
                            Profile::function_key(&ffs.name)
                        ),
                        &ffs.mount,
                    );
                }
                FunctionProfile::Hid(hid) => {
                    command.env(
                        format!("{HID_ENV_PREFIX}{}", Profile::function_key(&hid.name)),
                        &hid.device,
                    );
                }
            }
        }
        for (key, file) in &resources {
            command.env(
                format!("{RESOURCE_FD_ENV_PREFIX}{key}_FD"),
                file.as_raw_fd().to_string(),
            );
        }

        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(control_fd, libc::F_SETFD, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                for descriptor in &resource_fds {
                    if libc::fcntl(*descriptor, libc::F_SETFD, 0) != 0 {
                        return Err(io::Error::last_os_error());
                    }
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
        if let Err(error) = send_message(&mut supervisor, Message::ResourcesReady)
            .and_then(|_| receive_message(&mut supervisor))
            .and_then(|message| {
                if message == Message::FunctionFsReady {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("worker sent {message:?} instead of FUNCTIONFS_READY"),
                    ))
                }
            })
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

    fn open_resources(&self) -> io::Result<Vec<(String, File)>> {
        let mut opened = Vec::new();
        for resource in &self.profile.resources {
            let metadata = match fs::symlink_metadata(&resource.path) {
                Ok(metadata) => metadata,
                Err(error) if resource.optional && error.kind() == io::ErrorKind::NotFound => {
                    eprintln!(
                        "usb-gadget-supervisor: optional resource {} is unavailable at {}",
                        resource.name,
                        resource.path.display()
                    );
                    continue;
                }
                Err(error) => {
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "inspect resource {} at {}: {error}",
                            resource.name,
                            resource.path.display()
                        ),
                    ));
                }
            };
            if metadata.file_type().is_symlink() || !metadata.file_type().is_char_device() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "resource {} at {} must be a non-symlink character device",
                        resource.name,
                        resource.path.display()
                    ),
                ));
            }
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
            match options.open(&resource.path) {
                Ok(file) => opened.push((Profile::function_key(&resource.name), file)),
                Err(error) if resource.optional && error.kind() == io::ErrorKind::NotFound => {
                    eprintln!(
                        "usb-gadget-supervisor: optional resource {} disappeared before open",
                        resource.name
                    );
                }
                Err(error) => {
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "open resource {} at {}: {error}",
                            resource.name,
                            resource.path.display()
                        ),
                    ));
                }
            }
        }
        Ok(opened)
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

    fn prepare_hid_devices(&self) -> io::Result<()> {
        for function in &self.profile.functions {
            if let FunctionProfile::Hid(hid) = function {
                prepare_device(
                    &hid.device,
                    self.identity.uid,
                    self.identity.gid,
                    Duration::from_secs(5),
                )?;
            }
        }
        Ok(())
    }

    fn reconnect(&mut self) -> io::Result<()> {
        self.unbind()?;
        self.send(Message::UsbDetached)?;
        let udc = self
            .udc
            .clone()
            .ok_or_else(|| io::Error::other("cannot reconnect before UDC selection"))?;
        write_attribute(&self.gadget.join("UDC"), &udc)?;
        self.prepare_hid_devices()?;
        self.send(Message::UsbAttached)
    }

    fn send(&mut self, message: Message) -> io::Result<()> {
        send_message(
            self.control.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "worker channel closed")
            })?,
            message,
        )
    }

    fn receive(&mut self) -> io::Result<Message> {
        receive_message(
            self.control.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "worker channel closed")
            })?,
        )
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

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!("usb-gadget-supervisor: cleanup failed: {error}");
        }
    }
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

fn prepare_device(path: &Path, uid: u32, gid: u32, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() {
            chown(path, Some(uid), Some(gid))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            return Ok(());
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

fn send_message(channel: &mut UnixStream, message: Message) -> io::Result<()> {
    let packet = message.encode();
    let length = unsafe {
        libc::send(
            channel.as_raw_fd(),
            packet.as_ptr().cast::<c_void>(),
            packet.len(),
            0,
        )
    };
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

fn receive_message(channel: &mut UnixStream) -> io::Result<Message> {
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
