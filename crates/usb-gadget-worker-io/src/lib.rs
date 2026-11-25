#![cfg(unix)]
//! File-shaped compatibility API for physical and simulated worker handles.
//! The harness emulates endpoint activation/cancellation at the handle boundary.

use std::{
    fs,
    io::{self, Read, Write},
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
    path::Path,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
};
pub const MAX_TRANSFER_LENGTH: usize = 65536;
pub const DATA: u8 = 0;
pub const ENABLE: u8 = 1;
pub const DISABLE: u8 = 2;
const ENDPOINT: u8 = 3;
const DIGITAL_INPUT: u8 = 4;
const SINK: u8 = 5;

/// Synthetic handle kind, selected by the provider rather than worker policy.
#[derive(Clone, Copy)]
pub enum HandleKind {
    Endpoint,
    DigitalInput,
    Sink,
}
pub fn initialize(socket: &impl AsRawFd, kind: HandleKind) -> io::Result<()> {
    send_packet(
        socket,
        &[match kind {
            HandleKind::Endpoint => ENDPOINT,
            HandleKind::DigitalInput => DIGITAL_INPUT,
            HandleKind::Sink => SINK,
        }],
        0,
    )
}
pub fn event(socket: &impl AsRawFd, enabled: bool) -> io::Result<()> {
    send_packet(socket, &[if enabled { ENABLE } else { DISABLE }], 0)
}
pub fn send_transfer(socket: &impl AsRawFd, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > MAX_TRANSFER_LENGTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "simulated transfer too large",
        ));
    }
    let mut packet = Vec::with_capacity(bytes.len() + 1);
    packet.push(DATA);
    packet.extend_from_slice(bytes);
    send_packet(socket, &packet, 0)
}
pub fn receive_transfer(socket: &impl AsRawFd) -> io::Result<Vec<u8>> {
    let packet = receive_packet(socket, 0)?;
    if packet.first() != Some(&DATA) {
        return Err(invalid("expected simulated data transfer"));
    }
    Ok(packet[1..].to_vec())
}

fn send_packet(socket: &impl AsRawFd, bytes: &[u8], flags: i32) -> io::Result<()> {
    // SAFETY: valid descriptor and initialized buffer for this syscall.
    let result = unsafe {
        libc::send(
            socket.as_raw_fd(),
            bytes.as_ptr().cast(),
            bytes.len(),
            libc::MSG_NOSIGNAL | flags,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if result as usize != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "partial simulated transfer",
        ));
    }
    Ok(())
}
fn receive_packet(socket: &impl AsRawFd, flags: i32) -> io::Result<Vec<u8>> {
    let mut packet = vec![0; MAX_TRANSFER_LENGTH + 1];
    let mut vector = libc::iovec {
        iov_base: packet.as_mut_ptr().cast(),
        iov_len: packet.len(),
    };
    // SAFETY: zero-initialized msghdr and all pointed-to buffers remain live.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    let result = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut message, flags) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if message.msg_flags & libc::MSG_TRUNC != 0 {
        return Err(invalid("truncated simulated packet"));
    }
    if result == 0 {
        return Err(io::Error::from_raw_os_error(libc::ENODEV));
    }
    packet.truncate(result as usize);
    Ok(packet)
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn unavailable() -> io::Error {
    io::Error::from_raw_os_error(libc::ENODEV)
}

pub fn is_simulated(handle: &impl AsRawFd) -> io::Result<bool> {
    let mut kind: libc::c_int = 0;
    let mut length = std::mem::size_of_val(&kind) as libc::socklen_t;
    // SAFETY: output buffers have the declared sizes and the handle remains live.
    if unsafe {
        libc::getsockopt(
            handle.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut libc::c_int).cast(),
            &mut length,
        )
    } < 0
    {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENOTSOCK) {
            Ok(false)
        } else {
            Err(error)
        };
    }
    let mut address: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of_val(&address) as libc::socklen_t;
    if unsafe {
        libc::getsockname(
            handle.as_raw_fd(),
            (&mut address as *mut libc::sockaddr_storage).cast(),
            &mut length,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    if kind != libc::SOCK_DGRAM || address.ss_family as libc::c_int != libc::AF_UNIX {
        return Err(invalid("simulated handles require Unix datagrams"));
    }
    Ok(true)
}

#[derive(Default)]
struct Simulated {
    kind: Option<u8>,
    enabled: bool,
    pressed: bool,
    pending: Vec<u8>,
    offset: usize,
}

/// File-compatible handle. Real device calls pass through; simulated calls
/// reproduce the operations used by worker endpoint and GPIO loops.
pub struct File {
    file: fs::File,
    simulated: Arc<OnceLock<bool>>,
    state: Arc<Mutex<Simulated>>,
}
impl From<OwnedFd> for File {
    fn from(fd: OwnedFd) -> Self {
        Self {
            file: fs::File::from(fd),
            simulated: Arc::new(OnceLock::new()),
            state: Arc::new(Mutex::new(Simulated::default())),
        }
    }
}
impl File {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self::from(OwnedFd::from(fs::File::open(path)?)))
    }
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            simulated: Arc::clone(&self.simulated),
            state: Arc::clone(&self.state),
        })
    }
    fn simulated(&self) -> io::Result<bool> {
        if let Some(simulated) = self.simulated.get() {
            return Ok(*simulated);
        }
        let simulated = is_simulated(self)?;
        Ok(*self.simulated.get_or_init(|| simulated))
    }
    pub fn physical_file(&self) -> &fs::File {
        &self.file
    }
    fn state(&self) -> io::Result<MutexGuard<'_, Simulated>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("simulated handle lock poisoned"))?;
        if state.kind.is_none() {
            let hello = receive_packet(self, libc::MSG_DONTWAIT)?;
            if hello.len() != 1 || !matches!(hello[0], ENDPOINT | DIGITAL_INPUT | SINK) {
                return Err(invalid("missing simulated handle declaration"));
            }
            state.kind = Some(hello[0]);
        }
        Ok(state)
    }
    pub fn digital_level(&self) -> io::Result<Option<bool>> {
        if !self.simulated()? {
            return Ok(None);
        }
        let state = self.state()?;
        if state.kind != Some(DIGITAL_INPUT) {
            return Err(invalid("expected digital input handle"));
        }
        Ok(Some(state.pressed))
    }
}
impl AsRawFd for File {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}
impl AsFd for File {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}
impl Read for File {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if !self.simulated()? {
            return self.file.read(bytes);
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut state = self.state()?;
        if state.kind == Some(DIGITAL_INPUT) {
            if bytes.len() < 48 {
                return Err(invalid("GPIO edge buffer too small"));
            }
            let packet = receive_packet(self, libc::MSG_DONTWAIT)?;
            if packet.len() != 2 || packet[0] != DATA || packet[1] > 1 {
                return Err(invalid("invalid simulated input level"));
            }
            state.pressed = packet[1] == 1;
            bytes[..48].fill(0);
            bytes[8..12].copy_from_slice(&(if state.pressed { 1_u32 } else { 2 }).to_ne_bytes());
            return Ok(48);
        }
        if state.kind != Some(ENDPOINT) {
            return Err(invalid("expected endpoint handle"));
        }
        if state.offset == state.pending.len() {
            loop {
                wait(self, libc::POLLIN)?;
                let packet = match receive_packet(self, libc::MSG_DONTWAIT) {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                    result => result?,
                };
                match packet[0] {
                    ENABLE if packet.len() == 1 => state.enabled = true,
                    DISABLE if packet.len() == 1 => {
                        state.enabled = false;
                        return Err(unavailable());
                    }
                    DATA if state.enabled => {
                        state.pending = packet[1..].to_vec();
                        state.offset = 0;
                        break;
                    }
                    _ => return Err(invalid("invalid simulated endpoint packet")),
                }
            }
        }
        let count = bytes.len().min(state.pending.len() - state.offset);
        bytes[..count].copy_from_slice(&state.pending[state.offset..state.offset + count]);
        state.offset += count;
        Ok(count)
    }
}
impl Write for File {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.simulated()? {
            return self.file.write(bytes);
        }
        if bytes.len() > MAX_TRANSFER_LENGTH {
            return Err(invalid("simulated transfer too large"));
        }
        let mut state = self.state()?;
        if state.kind != Some(ENDPOINT) {
            return Err(invalid("expected endpoint handle"));
        }
        let mut packet = Vec::with_capacity(bytes.len() + 1);
        packet.push(DATA);
        packet.extend_from_slice(bytes);
        loop {
            loop {
                match receive_packet(self, libc::MSG_DONTWAIT) {
                    Ok(event) if event == [ENABLE] => state.enabled = true,
                    Ok(event) if event == [DISABLE] => state.enabled = false,
                    Ok(_) => return Err(invalid("invalid simulated IN endpoint event")),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(error),
                }
            }
            if !state.enabled {
                return Err(unavailable());
            }
            match send_packet(self, &packet, libc::MSG_DONTWAIT) {
                Ok(()) => return Ok(bytes.len()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    wait(self, libc::POLLIN | libc::POLLOUT)?
                }
                // Darwin reports a full peer datagram queue as ENOBUFS, and
                // POLLOUT may remain asserted. Bound retries while still
                // receiving cancellation immediately.
                Err(error) if error.raw_os_error() == Some(libc::ENOBUFS) => {
                    wait_with_timeout(self, libc::POLLIN, 10)?
                }
                Err(error) => return Err(error),
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
fn wait(file: &impl AsRawFd, events: i16) -> io::Result<()> {
    wait_with_timeout(file, events, -1)
}
fn wait_with_timeout(file: &impl AsRawFd, events: i16, timeout: i32) -> io::Result<()> {
    loop {
        let mut descriptor = libc::pollfd {
            fd: file.as_raw_fd(),
            events,
            revents: 0,
        };
        // SAFETY: a single valid pollfd remains live for this syscall.
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result == 0 || descriptor.revents & events != 0 {
            return Ok(());
        }
        if descriptor.revents != 0 {
            return Err(unavailable());
        }
    }
}

/// GPIO API shape used by existing workers.
pub mod gpio {
    pub mod v2 {
        use super::super::*;
        #[cfg(target_os = "linux")]
        pub use gpiocdev_uapi::v2::LineValues;
        #[cfg(not(target_os = "linux"))]
        pub struct LineValues {
            pub bits: u64,
            pub mask: u64,
        }
        #[cfg(not(target_os = "linux"))]
        impl LineValues {
            pub fn get(&self, index: usize) -> Option<bool> {
                (index < 64 && self.mask & (1 << index) != 0).then(|| self.bits & (1 << index) != 0)
            }
        }
        pub fn get_line_values(file: &File, values: &mut LineValues) -> io::Result<()> {
            if let Some(pressed) = file.digital_level()? {
                values.bits = u64::from(pressed);
                return Ok(());
            }
            #[cfg(target_os = "linux")]
            {
                gpiocdev_uapi::v2::get_line_values(file.physical_file(), values)
                    .map_err(io::Error::other)
            }
            #[cfg(not(target_os = "linux"))]
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "physical GPIO requires Linux",
            ))
        }
    }
}
/// Display API shape used by existing workers. The indicator scheduler remains
/// in display-backends; only actual device operations are substituted.
pub mod display {
    use super::*;
    pub use display_backends::Backend;
    struct Handle(RawFd);
    impl AsRawFd for Handle {
        fn as_raw_fd(&self) -> RawFd {
            self.0
        }
    }
    pub enum Display {
        Simulated,
        #[cfg(target_os = "linux")]
        Physical(display_backends::Display),
    }
    impl Display {
        pub fn from_raw_fds(
            backend: Backend,
            bus: RawFd,
            control: Option<RawFd>,
        ) -> io::Result<Self> {
            let simulated = is_simulated(&Handle(bus))?;
            if let Some(control) = control
                && is_simulated(&Handle(control))? != simulated
            {
                return Err(invalid("mixed display handle kinds"));
            }
            if simulated {
                return Ok(Self::Simulated);
            }
            #[cfg(target_os = "linux")]
            {
                Ok(Self::Physical(display_backends::Display::from_raw_fds(
                    backend, bus, control,
                )?))
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = backend;
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "physical displays require Linux",
                ))
            }
        }
        pub fn write_native_frame(&mut self, _frame: &[u8]) -> io::Result<()> {
            #[cfg(target_os = "linux")]
            if let Self::Physical(display) = self {
                return display.write_native_frame(_frame);
            }
            Ok(())
        }
        pub fn shutdown(&mut self) -> io::Result<()> {
            #[cfg(target_os = "linux")]
            if let Self::Physical(display) = self {
                return display.shutdown();
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::net::UnixDatagram, time::Duration};
    #[test]
    fn compatible_file_preserves_transfers_zlps_and_cancellation() {
        let (host, worker) = UnixDatagram::pair().unwrap();
        initialize(&host, HandleKind::Endpoint).unwrap();
        event(&host, true).unwrap();
        let mut file = File::from(OwnedFd::from(worker));
        send_transfer(&host, &[1, 2, 3, 4]).unwrap();
        send_transfer(&host, &[]).unwrap();
        let mut bytes = [0; 2];
        assert_eq!(file.read(&mut bytes).unwrap(), 2);
        assert_eq!(bytes, [1, 2]);
        assert_eq!(file.read(&mut bytes).unwrap(), 2);
        assert_eq!(bytes, [3, 4]);
        assert_eq!(file.read(&mut bytes).unwrap(), 0);
        let (sender, receiver) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            sender
                .send(file.read(&mut [0; 64]).unwrap_err().raw_os_error())
                .unwrap()
        });
        event(&host, false).unwrap();
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            Some(libc::ENODEV)
        );
        thread.join().unwrap();
    }
    #[test]
    fn disable_cancels_a_backpressured_writer() {
        let (host, worker) = UnixDatagram::pair().unwrap();
        initialize(&host, HandleKind::Endpoint).unwrap();
        event(&host, true).unwrap();
        let mut packet = vec![0; 1024];
        packet[0] = DATA;
        loop {
            match send_packet(&worker, &packet, libc::MSG_DONTWAIT) {
                Ok(()) => {}
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.raw_os_error() == Some(libc::ENOBUFS) =>
                {
                    break;
                }
                Err(error) => panic!("fill endpoint queue: {error}"),
            }
        }
        let mut file = File::from(OwnedFd::from(worker));
        let (sender, receiver) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            sender
                .send(file.write(&[1; 1023]).unwrap_err().raw_os_error())
                .unwrap()
        });
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(20)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        event(&host, false).unwrap();
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            Some(libc::ENODEV)
        );
        thread.join().unwrap();
    }

    #[test]
    fn gpio_api_samples_shared_input_level_without_automatic_touch() {
        let (host, worker) = UnixDatagram::pair().unwrap();
        initialize(&host, HandleKind::DigitalInput).unwrap();
        let mut file = File::from(OwnedFd::from(worker));
        let sensor = file.try_clone().unwrap();
        let mut values = gpio::v2::LineValues { bits: 0, mask: 1 };
        gpio::v2::get_line_values(&sensor, &mut values).unwrap();
        assert_eq!(values.get(0), Some(false));
        send_transfer(&host, &[1]).unwrap();
        assert_eq!(file.read(&mut [0; 48]).unwrap(), 48);
        gpio::v2::get_line_values(&sensor, &mut values).unwrap();
        assert_eq!(values.get(0), Some(true));
    }
}
