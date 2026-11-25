//! Private framing for simulated handles; USB payloads remain opaque.

use std::{io, os::fd::AsRawFd};

pub const MAX_TRANSFER_LENGTH: usize = 65536;
pub const DATA: u8 = 0;
pub const ENABLE: u8 = 1;
pub const DISABLE: u8 = 2;
pub(crate) const ENDPOINT: u8 = 3;
pub(crate) const DIGITAL_INPUT: u8 = 4;
pub(crate) const SINK: u8 = 5;

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

pub(crate) fn send_packet(socket: &impl AsRawFd, bytes: &[u8], flags: i32) -> io::Result<()> {
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
pub(crate) fn receive_packet(socket: &impl AsRawFd, flags: i32) -> io::Result<Vec<u8>> {
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
pub(crate) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
