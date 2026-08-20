//! Structural validation of FunctionFS descriptor and string blobs.

use std::collections::HashSet;
use std::io;

const DESCRIPTORS_MAGIC_V2: u32 = 3;
const STRINGS_MAGIC: u32 = 2;
const HAS_FS_DESC: u32 = 1;
const HAS_HS_DESC: u32 = 2;
const HAS_SS_DESC: u32 = 4;
const SUPPORTED_FLAGS: u32 = HAS_FS_DESC | HAS_HS_DESC | HAS_SS_DESC;
const USB_DT_ENDPOINT: u8 = 5;
const USB_DIR_IN: u8 = 0x80;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Direction {
    Out,
    In,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Endpoint {
    pub(crate) direction: Direction,
    pub(crate) transfer_type: u8,
}

pub(crate) fn inspect(descriptors: &[u8], strings: &[u8]) -> io::Result<Vec<Endpoint>> {
    let endpoints = inspect_descriptors(descriptors)?;
    inspect_strings(strings)?;
    Ok(endpoints)
}

fn inspect_descriptors(bytes: &[u8]) -> io::Result<Vec<Endpoint>> {
    if bytes.len() < 12 {
        return invalid("FunctionFS descriptor blob is shorter than its v2 header");
    }
    if le_u32(bytes, 0)? != DESCRIPTORS_MAGIC_V2 {
        return invalid("FunctionFS descriptors must use the v2 format");
    }
    if le_u32(bytes, 4)? as usize != bytes.len() {
        return invalid("FunctionFS descriptor length does not match the blob size");
    }
    let flags = le_u32(bytes, 8)?;
    if flags == 0 || flags & !SUPPORTED_FLAGS != 0 {
        return invalid(format!(
            "unsupported FunctionFS descriptor flags 0x{flags:08x}"
        ));
    }

    let mut offset = 12;
    let mut sets = Vec::new();
    for flag in [HAS_FS_DESC, HAS_HS_DESC, HAS_SS_DESC] {
        if flags & flag != 0 {
            sets.push((flag, read_count(bytes, &mut offset)?));
        }
    }

    let mut canonical = None;
    for (speed, count) in sets {
        let endpoints = parse_descriptor_set(bytes, &mut offset, count)?;
        if let Some(expected) = &canonical {
            if expected != &endpoints {
                return invalid(format!(
                    "FunctionFS endpoint topology differs in speed set 0x{speed:02x}"
                ));
            }
        } else {
            canonical = Some(endpoints);
        }
    }
    if offset != bytes.len() {
        return invalid("FunctionFS descriptor blob contains trailing data");
    }
    Ok(canonical.unwrap_or_default())
}

fn parse_descriptor_set(
    bytes: &[u8],
    offset: &mut usize,
    count: usize,
) -> io::Result<Vec<Endpoint>> {
    let mut endpoints = Vec::new();
    let mut addresses = HashSet::new();
    for _ in 0..count {
        let header = bytes
            .get(*offset..*offset + 2)
            .ok_or_else(|| data_error("truncated FunctionFS USB descriptor"))?;
        let length = header[0] as usize;
        if length < 2 {
            return invalid("FunctionFS USB descriptor has bLength smaller than two");
        }
        let descriptor = bytes
            .get(*offset..*offset + length)
            .ok_or_else(|| data_error("FunctionFS USB descriptor exceeds its blob"))?;
        *offset += length;

        if descriptor[1] == USB_DT_ENDPOINT {
            if descriptor.len() < 7 {
                return invalid("FunctionFS endpoint descriptor is shorter than seven bytes");
            }
            let address = descriptor[2];
            if address & 0x0f == 0 {
                return invalid("FunctionFS data endpoint uses reserved endpoint number zero");
            }
            if !addresses.insert(address) {
                return invalid(format!(
                    "duplicate FunctionFS endpoint address 0x{address:02x}"
                ));
            }
            endpoints.push(Endpoint {
                direction: if address & USB_DIR_IN != 0 {
                    Direction::In
                } else {
                    Direction::Out
                },
                transfer_type: descriptor[3] & 0x03,
            });
        }
    }
    Ok(endpoints)
}

fn inspect_strings(bytes: &[u8]) -> io::Result<()> {
    if bytes.len() < 16 {
        return invalid("FunctionFS string blob is shorter than its header");
    }
    if le_u32(bytes, 0)? != STRINGS_MAGIC {
        return invalid("invalid FunctionFS string magic");
    }
    if le_u32(bytes, 4)? as usize != bytes.len() {
        return invalid("FunctionFS string length does not match the blob size");
    }
    let string_count = le_u32(bytes, 8)? as usize;
    let language_count = le_u32(bytes, 12)? as usize;
    let mut offset = 16;
    for _ in 0..language_count {
        bytes
            .get(offset..offset + 2)
            .ok_or_else(|| data_error("truncated FunctionFS language identifier"))?;
        offset += 2;
        for _ in 0..string_count {
            let tail = bytes
                .get(offset..)
                .ok_or_else(|| data_error("truncated FunctionFS string table"))?;
            let length = tail
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| data_error("unterminated FunctionFS UTF-8 string"))?;
            std::str::from_utf8(&tail[..length])
                .map_err(|_| data_error("FunctionFS string is not valid UTF-8"))?;
            offset += length + 1;
        }
    }
    if offset != bytes.len() {
        return invalid("FunctionFS string blob contains trailing data");
    }
    Ok(())
}

fn read_count(bytes: &[u8], offset: &mut usize) -> io::Result<usize> {
    let count = le_u32(bytes, *offset)? as usize;
    *offset += 4;
    Ok(count)
}

fn le_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| data_error("truncated FunctionFS 32-bit field"))?;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

fn invalid<T>(message: impl Into<String>) -> io::Result<T> {
    Err(data_error(message))
}

fn data_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings() -> Vec<u8> {
        [2_u32, 16, 0, 0]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect()
    }

    fn descriptors(second_address: u8) -> Vec<u8> {
        let set = |interval| {
            vec![
                9,
                4,
                0,
                0,
                2,
                0xff,
                0,
                0,
                0,
                7,
                5,
                0x01,
                2,
                64,
                0,
                0,
                7,
                5,
                second_address,
                3,
                8,
                0,
                interval,
            ]
        };
        let fs = set(10);
        let hs = set(4);
        let length = 20 + fs.len() + hs.len();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&DESCRIPTORS_MAGIC_V2.to_le_bytes());
        bytes.extend_from_slice(&(length as u32).to_le_bytes());
        bytes.extend_from_slice(&(HAS_FS_DESC | HAS_HS_DESC).to_le_bytes());
        bytes.extend_from_slice(&3_u32.to_le_bytes());
        bytes.extend_from_slice(&3_u32.to_le_bytes());
        bytes.extend_from_slice(&fs);
        bytes.extend_from_slice(&hs);
        bytes
    }

    #[test]
    fn derives_endpoint_order_and_direction() {
        assert_eq!(
            inspect(&descriptors(0x82), &strings()).unwrap(),
            vec![
                Endpoint {
                    direction: Direction::Out,
                    transfer_type: 2,
                },
                Endpoint {
                    direction: Direction::In,
                    transfer_type: 3,
                }
            ]
        );
    }

    #[test]
    fn rejects_mismatched_speed_topology_and_trailing_strings() {
        let mut invalid_descriptors = descriptors(0x82);
        let hs_address = 20 + 23 + 9 + 7 + 2;
        invalid_descriptors[hs_address] = 0x02;
        assert!(inspect(&invalid_descriptors, &strings()).is_err());

        let mut invalid_strings = strings();
        invalid_strings.push(0);
        assert!(inspect(&descriptors(0x82), &invalid_strings).is_err());
    }
}
