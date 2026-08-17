use std::io;

pub(crate) const CONTROL_FD_ENV: &str = "USB_GADGET_CONTROL_FD";
pub(crate) const STATE_DIRECTORY_ENV: &str = "USB_GADGET_STATE_DIRECTORY";
pub(crate) const RUNTIME_DIRECTORY_ENV: &str = "USB_GADGET_RUNTIME_DIRECTORY";
pub(crate) const FUNCTIONFS_ENV_PREFIX: &str = "USB_GADGET_FUNCTIONFS_";
pub(crate) const HID_ENV_PREFIX: &str = "USB_GADGET_HID_";
pub(crate) const RESOURCE_FD_ENV_PREFIX: &str = "USB_GADGET_RESOURCE_";

const MAGIC: [u8; 4] = *b"UGSP";
pub(crate) const VERSION: u8 = 1;
pub(crate) const PACKET_LENGTH: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum Message {
    ResourcesReady = 0x01,
    UsbAttached = 0x02,
    UsbDetached = 0x03,
    Shutdown = 0x04,
    FunctionFsReady = 0x81,
    ReconnectRequest = 0x82,
    Stopped = 0x83,
    Fatal = 0x84,
}

impl Message {
    pub(crate) fn encode(self) -> [u8; PACKET_LENGTH] {
        [
            MAGIC[0], MAGIC[1], MAGIC[2], MAGIC[3], VERSION, self as u8, 0, 0,
        ]
    }

    pub(crate) fn decode(packet: [u8; PACKET_LENGTH]) -> io::Result<Self> {
        if packet[..4] != MAGIC {
            return invalid("invalid worker-control magic");
        }
        if packet[4] != VERSION {
            return invalid(format!("unsupported worker-control version {}", packet[4]));
        }
        if packet[6..] != [0, 0] {
            return invalid("unsupported worker-control flags");
        }
        match packet[5] {
            0x01 => Ok(Self::ResourcesReady),
            0x02 => Ok(Self::UsbAttached),
            0x03 => Ok(Self::UsbDetached),
            0x04 => Ok(Self::Shutdown),
            0x81 => Ok(Self::FunctionFsReady),
            0x82 => Ok(Self::ReconnectRequest),
            0x83 => Ok(Self::Stopped),
            0x84 => Ok(Self::Fatal),
            kind => invalid(format!("unknown worker-control message 0x{kind:02x}")),
        }
    }
}

fn invalid<T>(message: impl Into<String>) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_encoding_is_fixed_and_c_friendly() {
        assert_eq!(
            Message::FunctionFsReady.encode(),
            [b'U', b'G', b'S', b'P', 1, 0x81, 0, 0]
        );
        assert_eq!(
            Message::decode(Message::UsbAttached.encode()).unwrap(),
            Message::UsbAttached
        );
    }

    #[test]
    fn rejects_unknown_versions_and_flags() {
        let mut version = Message::Stopped.encode();
        version[4] = 2;
        assert!(Message::decode(version).is_err());
        let mut flags = Message::Stopped.encode();
        flags[7] = 1;
        assert!(Message::decode(flags).is_err());
    }
}
