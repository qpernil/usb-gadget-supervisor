//! Simulated implementations of the worker's existing low-level APIs.
use crate::wire::*;
use std::{
    fs,
    io::{self, Read, Write},
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};
fn unavailable() -> io::Error {
    io::Error::from_raw_os_error(libc::ENODEV)
}

#[derive(Default)]
struct Simulated {
    kind: Option<u8>,
    enabled: bool,
    pressed: bool,
    pending: Vec<u8>,
    offset: usize,
}

/// File-compatible fake handle for the existing endpoint and GPIO calls.
pub struct File {
    file: fs::File,
    state: Arc<Mutex<Simulated>>,
}
impl From<OwnedFd> for File {
    fn from(fd: OwnedFd) -> Self {
        Self {
            file: fs::File::from(fd),
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
            state: Arc::clone(&self.state),
        })
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
    pub fn digital_level(&self) -> io::Result<bool> {
        let state = self.state()?;
        if state.kind != Some(DIGITAL_INPUT) {
            return Err(invalid("expected digital input handle"));
        }
        Ok(state.pressed)
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

pub mod gpio {
    pub mod v2 {
        use super::super::*;
        pub struct LineValues {
            pub bits: u64,
            pub mask: u64,
        }
        impl LineValues {
            pub fn get(&self, index: usize) -> Option<bool> {
                (index < 64 && self.mask & (1 << index) != 0).then(|| self.bits & (1 << index) != 0)
            }
        }
        pub fn get_line_values(file: &File, values: &mut LineValues) -> io::Result<()> {
            values.bits = u64::from(file.digital_level()?);
            Ok(())
        }
    }
}
pub mod display {
    use super::*;
    pub use display_backends::Backend;
    pub struct Display;
    impl Display {
        pub fn from_raw_fds(
            _backend: Backend,
            _bus: RawFd,
            _control: Option<RawFd>,
        ) -> io::Result<Self> {
            Ok(Self)
        }
        pub fn write_native_frame(&mut self, _frame: &[u8]) -> io::Result<()> {
            Ok(())
        }
        pub fn shutdown(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
