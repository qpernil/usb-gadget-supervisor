use std::io;

pub(crate) const CONTROL_FD: i32 = 3;
pub(crate) const STATE_DIRECTORY_ENV: &str = "USB_GADGET_STATE_DIRECTORY";
pub(crate) const RUNTIME_DIRECTORY_ENV: &str = "USB_GADGET_RUNTIME_DIRECTORY";

const MAGIC: [u8; 4] = *b"UGSP";
pub(crate) const VERSION: u8 = 1;
pub(crate) const PACKET_LENGTH: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum Message {
    PrebindResources = 0x01,
    PostbindResources = 0x02,
    Prepared = 0x81,
    Serving = 0x82,
}

impl Message {
    pub(crate) fn encode(self, descriptor_count: u16) -> [u8; PACKET_LENGTH] {
        let count = descriptor_count.to_be_bytes();
        [
            MAGIC[0], MAGIC[1], MAGIC[2], MAGIC[3], VERSION, self as u8, count[0], count[1],
        ]
    }

    pub(crate) fn decode(packet: [u8; PACKET_LENGTH]) -> io::Result<(Self, u16)> {
        if packet[..4] != MAGIC {
            return invalid("invalid worker-control magic");
        }
        if packet[4] != VERSION {
            return invalid(format!("unsupported worker-control version {}", packet[4]));
        }
        let message = match packet[5] {
            0x01 => Self::PrebindResources,
            0x02 => Self::PostbindResources,
            0x81 => Self::Prepared,
            0x82 => Self::Serving,
            kind => return invalid(format!("unknown worker-control message 0x{kind:02x}")),
        };
        Ok((message, u16::from_be_bytes([packet[6], packet[7]])))
    }
}

fn invalid<T>(message: impl Into<String>) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_encoding_carries_the_exact_descriptor_count() {
        assert_eq!(
            Message::PrebindResources.encode(4),
            [b'U', b'G', b'S', b'P', 1, 0x01, 0, 4]
        );
        assert_eq!(
            Message::decode(Message::Serving.encode(0)).unwrap(),
            (Message::Serving, 0)
        );
    }

    #[test]
    fn rejects_unknown_versions_and_types() {
        let mut version = Message::Prepared.encode(0);
        version[4] = 2;
        assert!(Message::decode(version).is_err());
        let mut kind = Message::Prepared.encode(0);
        kind[5] = 0xff;
        assert!(Message::decode(kind).is_err());
    }
}
