//! Protocol-neutral process harness for supervisor-facing workers on Unix.
//! Device protocols belong in the consumer's tests, not in this crate.
#![cfg(unix)]

#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{
            fs::{DirBuilderExt, OpenOptionsExt},
            net::UnixDatagram,
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
use usb_gadget_worker::control;
pub use usb_gadget_worker::{
    UsbBusEvent, UsbPersonality,
    control::{Kind, Record},
};

use usb_gadget_worker_io::{self as transport, HandleKind, MAX_TRANSFER_LENGTH, receive_transfer};

const TIMEOUT: Duration = Duration::from_secs(5);
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

/// Private state/runtime directories. Keeping this alive across worker launches
/// permits persistence tests. Dropping it removes all test state and logs.
pub struct TestDirectory {
    root: PathBuf,
}
impl TestDirectory {
    pub fn new() -> io::Result<Self> {
        // macOS's per-user temporary path can exceed sockaddr_un's path limit
        // once workers append their local presence socket name.
        let base = if cfg!(target_os = "macos") {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        loop {
            let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let root = base.join(format!("usb-worker-{}-{id}", std::process::id()));
            match fs::DirBuilder::new().mode(0o700).create(&root) {
                Ok(()) => {
                    let directory = Self { root };
                    fs::create_dir(directory.state())?;
                    fs::create_dir(directory.runtime())?;
                    return Ok(directory);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
    pub fn state(&self) -> PathBuf {
        self.root.join("state")
    }
    pub fn runtime(&self) -> PathBuf {
        self.root.join("run")
    }
    pub fn path(&self) -> &Path {
        &self.root
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Caller-defined local resources. Names have no meaning to the harness.
pub enum Resource<'a> {
    /// An inert socket handle for an output such as a simulated display.
    Sink(&'a str),
    /// A released input; `set_input` sends explicit level transitions.
    DigitalInput(&'a str),
    /// A descriptor whose behavior is provided by the caller.
    File(&'a str, File),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EndpointDescriptor {
    pub address: u8,
    pub transfer_type: u8,
    pub max_packet_size: u16,
}

/// Host end of an endpoint. One send/receive is one transfer, including a ZLP.
pub struct HostEndpoint {
    socket: UnixDatagram,
    descriptor: EndpointDescriptor,
}
impl HostEndpoint {
    pub fn descriptor(&self) -> EndpointDescriptor {
        self.descriptor
    }
    pub fn send(&self, bytes: &[u8]) -> io::Result<()> {
        if self.descriptor.address & 0x80 != 0 || bytes.len() > MAX_TRANSFER_LENGTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid host OUT transfer",
            ));
        }
        transport::send_transfer(&self.socket, bytes)
    }
    pub fn receive(&self) -> io::Result<Vec<u8>> {
        if self.descriptor.address & 0x80 == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "host receive requires IN endpoint",
            ));
        }
        receive_transfer(&self.socket)
    }
    pub fn set_read_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.socket.set_read_timeout(Some(timeout))
    }
}

/// Owns and reaps a real worker process. Every blocking exchange is bounded;
/// Drop kills/reaps an unfinished child, including when a test panics.
pub struct Harness {
    child: Child,
    channel: UnixDatagram,
    inputs: BTreeMap<String, UnixDatagram>,
    // Keep the supplied capabilities and sink peers alive for this incarnation.
    resources: Vec<File>,
    _sinks: Vec<UnixDatagram>,
    endpoints: BTreeMap<u8, HostEndpoint>,
    generation: u32,
    activation: u64,
    request_id: u32,
    log_path: PathBuf,
}

impl Harness {
    /// Launches the ordinary worker command with FD 3 and InitialResources.
    /// No worker flag, privilege, USB kernel driver or attached device is needed.
    pub fn spawn(
        command: &mut Command,
        directory: &TestDirectory,
        resources: &[Resource<'_>],
    ) -> io::Result<Self> {
        let (channel, worker_channel) = control_pair()?;
        channel.set_read_timeout(Some(TIMEOUT))?;
        channel.set_write_timeout(Some(TIMEOUT))?;
        let mut inputs = BTreeMap::new();
        let mut sinks = Vec::new();
        let mut files = Vec::new();
        let mut names = Vec::new();
        let count = u16::try_from(resources.len()).map_err(|_| invalid("too many resources"))?;
        names.extend_from_slice(&count.to_be_bytes());
        let mut seen = std::collections::BTreeSet::new();
        for resource in resources {
            let name = match resource {
                Resource::Sink(name) | Resource::DigitalInput(name) | Resource::File(name, _) => {
                    *name
                }
            };
            if name.is_empty() || !seen.insert(name) {
                return Err(invalid("empty or duplicate resource name"));
            }
            let length =
                u16::try_from(name.len()).map_err(|_| invalid("resource name too long"))?;
            names.extend_from_slice(&length.to_be_bytes());
            names.extend_from_slice(name.as_bytes());
            let file = match resource {
                Resource::File(_, file) => file.try_clone()?,
                Resource::Sink(_) | Resource::DigitalInput(_) => {
                    let (host, worker) = datagram_pair()?;
                    transport::initialize(
                        &host,
                        if matches!(resource, Resource::DigitalInput(_)) {
                            HandleKind::DigitalInput
                        } else {
                            HandleKind::Sink
                        },
                    )?;
                    if matches!(resource, Resource::DigitalInput(_)) {
                        inputs.insert(name.to_owned(), host);
                    } else {
                        sinks.push(host);
                    }
                    File::from(OwnedFd::from(worker))
                }
            };
            files.push(file);
        }
        let log_path = directory.path().join("worker.log");
        let log = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&log_path)?;
        command
            .env(control::STATE_DIRECTORY_ENV, directory.state())
            .env(control::RUNTIME_DIRECTORY_ENV, directory.runtime())
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        let descriptor = worker_channel.as_raw_fd();
        // SAFETY: the pre-exec closure performs only async-signal-safe descriptor
        // syscalls. All other inherited handles have FD_CLOEXEC set.
        unsafe {
            command.pre_exec(move || {
                if descriptor != control::CONTROL_FD
                    && libc::dup2(descriptor, control::CONTROL_FD) < 0
                {
                    return Err(io::Error::last_os_error());
                }
                if libc::fcntl(control::CONTROL_FD, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        drop(worker_channel);
        let mut harness = Self {
            child,
            channel,
            inputs,
            resources: Vec::new(),
            _sinks: sinks,
            endpoints: BTreeMap::new(),
            generation: 0,
            activation: 0,
            request_id: 0,
            log_path,
        };
        harness.send(&Record::new(Kind::InitialResources, 0, 0, names), &files)?;
        harness.resources = files;
        Ok(harness)
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }
    pub fn logs(&self) -> String {
        fs::read_to_string(&self.log_path).unwrap_or_default()
    }
    pub fn endpoint(&self, address: u8) -> io::Result<&HostEndpoint> {
        self.endpoints
            .get(&address)
            .ok_or_else(|| invalid("endpoint is not configured"))
    }
    pub fn set_input(&self, name: &str, pressed: bool) -> io::Result<()> {
        let input = self
            .inputs
            .get(name)
            .ok_or_else(|| invalid("unknown digital input"))?;
        transport::send_transfer(input, &[u8::from(pressed)])
    }
    /// Low-level access permits malformed-message and rejection scenarios.
    pub fn send(&mut self, record: &Record, files: &[File]) -> io::Result<()> {
        control::send(&self.channel, record, files)
    }
    pub fn receive(&mut self) -> io::Result<Record> {
        control::receive(&self.channel).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("worker control: {error}\n{}", self.logs()),
            )
        })
    }

    /// Accepts the next nonempty personality, supplies fresh endpoint handles,
    /// and waits for Serving with matching generation/request identifiers.
    pub fn configure(&mut self) -> io::Result<UsbPersonality> {
        let record = self.receive()?;
        self.accept_configuration(record)
    }
    pub fn accept_configuration(&mut self, record: Record) -> io::Result<UsbPersonality> {
        if record.kind != Kind::Configure
            || record.generation != self.generation
            || record.request_id == 0
            || record.body.is_empty()
            || !record.descriptors.is_empty()
        {
            return Err(invalid(
                "expected nonempty Configure for current generation",
            ));
        }
        let personality = UsbPersonality::from_cbor(&record.body)?;
        let descriptors = endpoints(&personality.configuration_descriptor)?;
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("generation overflow"))?;
        let mut body = (descriptors.len() as u16).to_be_bytes().to_vec();
        let mut files = Vec::new();
        let mut host_endpoints = BTreeMap::new();
        for descriptor in descriptors {
            let (host, worker) = datagram_pair()?;
            transport::initialize(&host, HandleKind::Endpoint)?;
            host.set_read_timeout(Some(TIMEOUT))?;
            host.set_write_timeout(Some(TIMEOUT))?;
            body.extend_from_slice(&[descriptor.address, descriptor.transfer_type]);
            body.extend_from_slice(&descriptor.max_packet_size.to_be_bytes());
            files.push(File::from(OwnedFd::from(worker)));
            host_endpoints.insert(
                descriptor.address,
                HostEndpoint {
                    socket: host,
                    descriptor,
                },
            );
        }
        self.send(
            &Record::new(Kind::UsbEndpoints, generation, record.request_id, body),
            &files,
        )?;
        let serving = self.receive()?;
        expect(&serving, Kind::Serving, generation, record.request_id)?;
        self.generation = generation;
        self.activation = 0;
        self.endpoints = host_endpoints;
        Ok(personality)
    }
    pub fn bus_event(&mut self, event: UsbBusEvent) -> io::Result<()> {
        if matches!(
            event,
            UsbBusEvent::Enable | UsbBusEvent::Disable | UsbBusEvent::Unbind
        ) {
            for endpoint in self.endpoints.values() {
                transport::event(&endpoint.socket, event == UsbBusEvent::Enable)?;
            }
        }
        if event == UsbBusEvent::Enable {
            self.activation = self
                .activation
                .checked_add(1)
                .ok_or_else(|| invalid("activation overflow"))?;
        }
        self.send(
            &Record::new(
                Kind::UsbBusEvent,
                self.generation,
                0,
                event.encode(self.activation).to_vec(),
            ),
            &[],
        )
    }
    pub fn activate(&mut self) -> io::Result<()> {
        self.bus_event(UsbBusEvent::Bind)?;
        self.bus_event(UsbBusEvent::Enable)
    }
    pub fn control(&mut self, setup: [u8; 8], out_data: &[u8]) -> io::Result<Vec<u8>> {
        self.request_id = self
            .request_id
            .checked_add(1)
            .ok_or_else(|| invalid("request overflow"))?;
        let mut body = setup.to_vec();
        body.extend_from_slice(out_data);
        self.send(
            &Record::new(
                Kind::UsbControlRequest,
                self.generation,
                self.request_id,
                body,
            ),
            &[],
        )?;
        let response = self.receive()?;
        if response.kind != Kind::UsbControlResponse
            || response.generation != self.generation
            || response.request_id != self.request_id
            || !response.descriptors.is_empty()
        {
            return Err(invalid("mismatched control response"));
        }
        Ok(response.body)
    }
    /// Cancels simulated endpoint I/O, as UDC detachment does for FunctionFS.
    pub fn retire_endpoints(&self) -> io::Result<()> {
        for endpoint in self.endpoints.values() {
            transport::event(&endpoint.socket, false)?;
        }
        Ok(())
    }
    pub fn quiesce(&mut self, request_id: u32) -> io::Result<()> {
        self.retire_endpoints()?;
        self.send(
            &Record::new(Kind::Quiesce, self.generation, request_id, Vec::new()),
            &[],
        )?;
        let response = self.receive()?;
        expect(&response, Kind::Quiesced, self.generation, request_id)?;
        self.endpoints.clear();
        Ok(())
    }
    pub fn wait_for_exit(&mut self) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("worker did not exit\n{}", self.logs()),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    pub fn shutdown(mut self) -> io::Result<()> {
        self.quiesce(0)?;
        let status = self.wait_for_exit()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "worker exited {status}\n{}",
                self.logs()
            )))
        }
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn expect(record: &Record, kind: Kind, generation: u32, request_id: u32) -> io::Result<()> {
    if record.kind != kind
        || record.generation != generation
        || record.request_id != request_id
        || !record.body.is_empty()
        || !record.descriptors.is_empty()
    {
        return Err(invalid(format!(
            "expected {kind:?} generation={generation} request={request_id}"
        )));
    }
    Ok(())
}
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn datagram_pair() -> io::Result<(UnixDatagram, UnixDatagram)> {
    let pair = UnixDatagram::pair()?;
    for socket in [&pair.0, &pair.1] {
        let size: libc::c_int = (MAX_TRANSFER_LENGTH + 64) as _;
        // macOS's default Unix datagram buffer is too small for long transfers.
        if unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of_val(&size) as _,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        socket.set_write_timeout(Some(TIMEOUT))?;
    }
    Ok(pair)
}
fn control_pair() -> io::Result<(UnixDatagram, UnixDatagram)> {
    #[cfg(target_os = "linux")]
    {
        let mut descriptors = [-1; 2];
        // Linux exercises the production control socket kind; macOS preserves
        // record boundaries and SCM_RIGHTS using Unix datagrams instead.
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                descriptors.as_mut_ptr(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe {
            (
                UnixDatagram::from_raw_fd(descriptors[0]),
                UnixDatagram::from_raw_fd(descriptors[1]),
            )
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        datagram_pair()
    }
}

fn endpoints(configuration: &[u8]) -> io::Result<Vec<EndpointDescriptor>> {
    if configuration.len() < 9
        || configuration[..2] != [9, 2]
        || u16::from_le_bytes([configuration[2], configuration[3]]) as usize != configuration.len()
    {
        return Err(invalid("invalid configuration descriptor"));
    }
    let mut endpoints = Vec::new();
    let mut offset = 0;
    let mut alternate = 0;
    while offset < configuration.len() {
        let header = configuration
            .get(offset..offset + 2)
            .ok_or_else(|| invalid("truncated descriptor"))?;
        let length = header[0] as usize;
        if length < 2 {
            return Err(invalid("invalid descriptor length"));
        }
        let descriptor = configuration
            .get(offset..offset + length)
            .ok_or_else(|| invalid("truncated descriptor"))?;
        if descriptor[1] == 4 {
            if length < 9 {
                return Err(invalid("short interface descriptor"));
            }
            alternate = descriptor[3];
            if alternate != 0 {
                return Err(invalid("harness requires alternate setting zero"));
            }
        }
        if descriptor[1] == 5 {
            if length < 7 || alternate != 0 {
                return Err(invalid("invalid endpoint descriptor"));
            }
            let endpoint = EndpointDescriptor {
                address: descriptor[2],
                transfer_type: descriptor[3] & 3,
                max_packet_size: u16::from_le_bytes([descriptor[4], descriptor[5]]) & 0x7ff,
            };
            if endpoint.address & 0x0f == 0
                || endpoint.max_packet_size == 0
                || endpoints
                    .iter()
                    .any(|previous: &EndpointDescriptor| previous.address == endpoint.address)
            {
                return Err(invalid("invalid or duplicate endpoint"));
            }
            endpoints.push(endpoint);
        }
        offset += length;
    }
    Ok(endpoints)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descriptor_parser_rejects_truncation_and_duplicates() {
        let mut descriptor = vec![
            9, 2, 23, 0, 1, 1, 0, 0x80, 50, 7, 5, 1, 2, 64, 0, 0, 7, 5, 0x81, 2, 64, 0, 0,
        ];
        assert_eq!(endpoints(&descriptor).unwrap().len(), 2);
        descriptor[18] = 1;
        assert!(endpoints(&descriptor).is_err());
        descriptor.truncate(22);
        assert!(endpoints(&descriptor).is_err());
    }
    #[test]
    fn test_directories_are_private_unique_and_removed() {
        use std::os::unix::fs::PermissionsExt;
        let first = TestDirectory::new().unwrap();
        let second = TestDirectory::new().unwrap();
        let path = first.path().to_owned();
        assert_ne!(path, second.path());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(first);
        assert!(!path.exists());
    }
}
