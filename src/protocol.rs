//! Process-lifetime supervisor/worker channel.

use std::ffi::c_void;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

pub(crate) const CONTROL_FD: i32 = 3;
pub(crate) const STATE_DIRECTORY_ENV: &str = "STATE_DIRECTORY";
pub(crate) const RUNTIME_DIRECTORY_ENV: &str = "RUNTIME_DIRECTORY";
const MAGIC: [u8; 4] = *b"UGSP";
const VERSION: u8 = 1;
const HEADER_LENGTH: usize = 20;
const MAX_BODY_LENGTH: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum Kind {
    InitialResources = 0x01,
    UsbEndpoints = 0x02,
    UsbBusEvent = 0x03,
    UsbControlRequest = 0x04,
    Quiesce = 0x11,
    ConfigurationRejected = 0x12,
    Configure = 0x80,
    UsbControlResponse = 0x81,
    Serving = 0x82,
    Quiesced = 0x84,
}

impl TryFrom<u8> for Kind {
    type Error = io::Error;
    fn try_from(value: u8) -> io::Result<Self> {
        match value {
            0x01 => Ok(Self::InitialResources),
            0x02 => Ok(Self::UsbEndpoints),
            0x03 => Ok(Self::UsbBusEvent),
            0x04 => Ok(Self::UsbControlRequest),
            0x11 => Ok(Self::Quiesce),
            0x12 => Ok(Self::ConfigurationRejected),
            0x80 => Ok(Self::Configure),
            0x81 => Ok(Self::UsbControlResponse),
            0x82 => Ok(Self::Serving),
            0x84 => Ok(Self::Quiesced),
            kind => invalid(format!("unknown worker-control message 0x{kind:02x}")),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Record {
    pub(crate) kind: Kind,
    pub(crate) generation: u32,
    pub(crate) request_id: u32,
    pub(crate) body: Vec<u8>,
    pub(crate) descriptors: Vec<OwnedFd>,
}

impl Record {
    pub(crate) fn new(kind: Kind, generation: u32, request_id: u32, body: Vec<u8>) -> Self {
        Self {
            kind,
            generation,
            request_id,
            body,
            descriptors: Vec::new(),
        }
    }
}

pub(crate) fn send<T: AsRawFd>(
    channel: &UnixStream,
    record: &Record,
    files: &[T],
) -> io::Result<()> {
    if record.body.len() > MAX_BODY_LENGTH {
        return invalid("worker-control body is too large");
    }
    let descriptor_count = u16::try_from(files.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many transferred descriptors",
        )
    })?;
    let body_length = u32::try_from(record.body.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker-control body is too large",
        )
    })?;
    let mut packet = Vec::with_capacity(HEADER_LENGTH + record.body.len());
    packet.extend_from_slice(&MAGIC);
    packet.push(VERSION);
    packet.push(record.kind as u8);
    packet.extend_from_slice(&descriptor_count.to_be_bytes());
    packet.extend_from_slice(&record.generation.to_be_bytes());
    packet.extend_from_slice(&record.request_id.to_be_bytes());
    packet.extend_from_slice(&body_length.to_be_bytes());
    packet.extend_from_slice(&record.body);

    let mut iovec = libc::iovec {
        iov_base: packet.as_mut_ptr().cast::<c_void>(),
        iov_len: packet.len(),
    };
    let raw = files.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>();
    let control_length = if raw.is_empty() {
        0
    } else {
        unsafe {
            libc::CMSG_SPACE((raw.len() * std::mem::size_of::<libc::c_int>()) as libc::c_uint)
                as usize
        }
    };
    let mut control = vec![0_u8; control_length];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iovec;
    message.msg_iovlen = 1;
    if !raw.is_empty() {
        message.msg_control = control.as_mut_ptr().cast::<c_void>();
        message.msg_controllen = control.len() as _;
        unsafe {
            let ancillary = libc::CMSG_FIRSTHDR(&message);
            (*ancillary).cmsg_level = libc::SOL_SOCKET;
            (*ancillary).cmsg_type = libc::SCM_RIGHTS;
            (*ancillary).cmsg_len =
                libc::CMSG_LEN((raw.len() * std::mem::size_of::<libc::c_int>()) as libc::c_uint)
                    as _;
            std::ptr::copy_nonoverlapping(
                raw.as_ptr(),
                libc::CMSG_DATA(ancillary).cast::<libc::c_int>(),
                raw.len(),
            );
        }
    }
    let length = unsafe { libc::sendmsg(channel.as_raw_fd(), &message, libc::MSG_NOSIGNAL) };
    if length < 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize != packet.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "worker-control record was not sent atomically",
        ));
    }
    Ok(())
}

pub(crate) fn receive(channel: &UnixStream) -> io::Result<Record> {
    receive_with_flags(channel, 0)
}

pub(crate) fn receive_nonblocking(channel: &UnixStream) -> io::Result<Record> {
    receive_with_flags(channel, libc::MSG_DONTWAIT)
}

fn receive_with_flags(channel: &UnixStream, extra_flags: libc::c_int) -> io::Result<Record> {
    let mut packet = vec![0_u8; HEADER_LENGTH + MAX_BODY_LENGTH + 1];
    let control_length = unsafe {
        libc::CMSG_SPACE((256 * std::mem::size_of::<libc::c_int>()) as libc::c_uint) as usize
    };
    let mut control = vec![0_u8; control_length];
    let mut iovec = libc::iovec {
        iov_base: packet.as_mut_ptr().cast::<c_void>(),
        iov_len: packet.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iovec;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast::<c_void>();
    message.msg_controllen = control.len() as _;
    #[cfg(target_os = "linux")]
    let receive_flags = libc::MSG_CMSG_CLOEXEC | extra_flags;
    #[cfg(not(target_os = "linux"))]
    let receive_flags = extra_flags;
    let length = unsafe { libc::recvmsg(channel.as_raw_fd(), &mut message, receive_flags) };
    if length < 0 {
        return Err(io::Error::last_os_error());
    }
    if length == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "worker-control channel closed",
        ));
    }
    if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
        || (length as usize) < HEADER_LENGTH
    {
        return invalid("truncated worker-control record");
    }
    packet.truncate(length as usize);
    if packet[..4] != MAGIC || packet[4] != VERSION {
        return invalid("invalid worker-control header");
    }
    let kind = Kind::try_from(packet[5])?;
    let declared_descriptors = u16::from_be_bytes([packet[6], packet[7]]) as usize;
    let generation = u32::from_be_bytes(packet[8..12].try_into().unwrap());
    let request_id = u32::from_be_bytes(packet[12..16].try_into().unwrap());
    let body_length = u32::from_be_bytes(packet[16..20].try_into().unwrap()) as usize;
    if body_length > MAX_BODY_LENGTH || packet.len() != HEADER_LENGTH + body_length {
        return invalid("worker-control body length does not match its record");
    }
    let mut descriptors = Vec::new();
    unsafe {
        let mut ancillary = libc::CMSG_FIRSTHDR(&message);
        while !ancillary.is_null() {
            if (*ancillary).cmsg_level != libc::SOL_SOCKET
                || (*ancillary).cmsg_type != libc::SCM_RIGHTS
            {
                return invalid("unexpected worker-control ancillary data");
            }
            let base = libc::CMSG_LEN(0) as usize;
            let size = (*ancillary).cmsg_len as usize;
            if size < base || (size - base) % std::mem::size_of::<libc::c_int>() != 0 {
                return invalid("malformed SCM_RIGHTS payload");
            }
            let count = (size - base) / std::mem::size_of::<libc::c_int>();
            let source = libc::CMSG_DATA(ancillary).cast::<libc::c_int>();
            for index in 0..count {
                descriptors.push(OwnedFd::from_raw_fd(*source.add(index)));
            }
            ancillary = libc::CMSG_NXTHDR(&message, ancillary);
        }
    }
    if descriptors.len() != declared_descriptors {
        return invalid(format!(
            "record declared {declared_descriptors} descriptors but carried {}",
            descriptors.len()
        ));
    }
    Ok(Record {
        kind,
        generation,
        request_id,
        body: packet[HEADER_LENGTH..].to_vec(),
        descriptors,
    })
}

fn invalid<T>(message: impl Into<String>) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::fd::AsFd;
    #[test]
    fn round_trip_preserves_record_and_descriptor() {
        let (left, right) = UnixStream::pair().unwrap();
        let file = File::open("/dev/null").unwrap();
        send(
            &left,
            &Record::new(Kind::UsbEndpoints, 7, 41, vec![1, 2, 3]),
            &[file.as_fd()],
        )
        .unwrap();
        let actual = receive(&right).unwrap();
        assert_eq!(
            (actual.kind, actual.generation, actual.request_id),
            (Kind::UsbEndpoints, 7, 41)
        );
        assert_eq!(actual.body, [1, 2, 3]);
        assert_eq!(actual.descriptors.len(), 1);
    }

    #[test]
    fn nonblocking_receive_does_not_wait_for_a_record() {
        let (_left, right) = UnixStream::pair().unwrap();
        let error = receive_nonblocking(&right).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }
}
