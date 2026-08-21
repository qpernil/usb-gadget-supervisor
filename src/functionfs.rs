//! Structural validation of FunctionFS descriptor and string blobs.

use std::collections::HashSet;
use std::io;

const DESCRIPTORS_MAGIC_V2: u32 = 3;
const STRINGS_MAGIC: u32 = 2;
const HAS_FS_DESC: u32 = 1;
const HAS_HS_DESC: u32 = 2;
const HAS_SS_DESC: u32 = 4;
const HAS_MS_OS_DESC: u32 = 8;
const SUPPORTED_FLAGS: u32 = HAS_FS_DESC | HAS_HS_DESC | HAS_SS_DESC | HAS_MS_OS_DESC;
const USB_DT_INTERFACE: u8 = 4;
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Inspection {
    pub(crate) endpoints: Vec<Endpoint>,
    pub(crate) has_ms_os_descriptors: bool,
}

pub(crate) fn inspect(descriptors: &[u8], strings: &[u8]) -> io::Result<Inspection> {
    let inspection = inspect_descriptors(descriptors)?;
    inspect_strings(strings)?;
    Ok(inspection)
}

fn inspect_descriptors(bytes: &[u8]) -> io::Result<Inspection> {
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
    let os_count = if flags & HAS_MS_OS_DESC != 0 {
        read_count(bytes, &mut offset)?
    } else {
        0
    };

    let mut canonical = None;
    let mut canonical_interfaces = None;
    for (speed, count) in sets {
        let (endpoints, interfaces) = parse_descriptor_set(bytes, &mut offset, count)?;
        if let Some(expected) = &canonical {
            if expected != &endpoints {
                return invalid(format!(
                    "FunctionFS endpoint topology differs in speed set 0x{speed:02x}"
                ));
            }
        } else {
            canonical = Some(endpoints);
        }
        if let Some(expected) = &canonical_interfaces {
            if expected != &interfaces {
                return invalid(format!(
                    "FunctionFS interface topology differs in speed set 0x{speed:02x}"
                ));
            }
        } else {
            canonical_interfaces = Some(interfaces);
        }
    }
    if flags & HAS_MS_OS_DESC != 0 {
        if os_count == 0 {
            return invalid("FunctionFS Microsoft OS descriptor count must not be zero");
        }
        parse_ms_os_descriptors(
            bytes,
            &mut offset,
            os_count,
            canonical_interfaces.as_ref().unwrap(),
        )?;
    }
    if offset != bytes.len() {
        return invalid("FunctionFS descriptor blob contains trailing data");
    }
    Ok(Inspection {
        endpoints: canonical.unwrap_or_default(),
        has_ms_os_descriptors: flags & HAS_MS_OS_DESC != 0,
    })
}

fn parse_descriptor_set(
    bytes: &[u8],
    offset: &mut usize,
    count: usize,
) -> io::Result<(Vec<Endpoint>, HashSet<u8>)> {
    let mut endpoints = Vec::new();
    let mut addresses = HashSet::new();
    let mut interfaces = HashSet::new();
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

        if descriptor[1] == USB_DT_INTERFACE {
            if descriptor.len() < 9 {
                return invalid("FunctionFS interface descriptor is shorter than nine bytes");
            }
            interfaces.insert(descriptor[2]);
        } else if descriptor[1] == USB_DT_ENDPOINT {
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
    Ok((endpoints, interfaces))
}

fn parse_ms_os_descriptors(
    bytes: &[u8],
    offset: &mut usize,
    count: usize,
    interfaces: &HashSet<u8>,
) -> io::Result<()> {
    for _ in 0..count {
        let start = *offset;
        let header = bytes
            .get(start..start + 11)
            .ok_or_else(|| data_error("truncated FunctionFS Microsoft OS descriptor header"))?;
        let interface = header[0];
        if !interfaces.contains(&interface) {
            return invalid(format!(
                "FunctionFS Microsoft OS descriptor refers to missing interface {interface}"
            ));
        }
        let length = le_u32(bytes, start + 1)? as usize;
        if length < 11 {
            return invalid("FunctionFS Microsoft OS descriptor is shorter than its header");
        }
        let end = start
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| data_error("FunctionFS Microsoft OS descriptor exceeds its blob"))?;
        if le_u16(bytes, start + 5)? != 0x0100 {
            return invalid("FunctionFS Microsoft OS descriptor must use version 1.0");
        }
        match le_u16(bytes, start + 7)? {
            4 => parse_extended_compat_id(bytes, start, end, interfaces)?,
            5 => parse_extended_properties(bytes, start, end)?,
            index => {
                return invalid(format!(
                    "unsupported FunctionFS Microsoft OS descriptor index {index}"
                ));
            }
        }
        *offset = end;
    }
    Ok(())
}

fn parse_extended_compat_id(
    bytes: &[u8],
    start: usize,
    end: usize,
    interfaces: &HashSet<u8>,
) -> io::Result<()> {
    let count = bytes[start + 9] as usize;
    if count == 0 || bytes[start + 10] != 0 || end - start != 11 + count * 24 {
        return invalid("invalid FunctionFS extended compatible-ID descriptor length or count");
    }
    let mut cursor = start + 11;
    for _ in 0..count {
        let feature = &bytes[cursor..cursor + 24];
        if !interfaces.contains(&feature[0]) {
            return invalid(format!(
                "FunctionFS compatible ID refers to missing interface {}",
                feature[0]
            ));
        }
        if feature[1] != 1 || feature[2..10].iter().all(|byte| *byte == 0) {
            return invalid("invalid FunctionFS extended compatible-ID feature");
        }
        if feature[2..18]
            .iter()
            .any(|byte| *byte != 0 && !byte.is_ascii_graphic())
            || feature[18..].iter().any(|byte| *byte != 0)
        {
            return invalid("invalid FunctionFS compatible-ID bytes or reserved fields");
        }
        cursor += 24;
    }
    Ok(())
}

fn parse_extended_properties(bytes: &[u8], start: usize, end: usize) -> io::Result<()> {
    let count = le_u16(bytes, start + 9)? as usize;
    if count == 0 {
        return invalid("FunctionFS extended-properties descriptor count must not be zero");
    }
    let mut cursor = start + 11;
    for _ in 0..count {
        let size = le_u32(bytes, cursor)? as usize;
        if size < 14
            || cursor
                .checked_add(size)
                .is_none_or(|item_end| item_end > end)
        {
            return invalid("invalid FunctionFS extended-property size");
        }
        let item_end = cursor + size;
        let property_type = le_u32(bytes, cursor + 4)?;
        if !(1..=7).contains(&property_type) {
            return invalid(format!(
                "unsupported FunctionFS extended-property type {property_type}"
            ));
        }
        let name_length = le_u16(bytes, cursor + 8)? as usize;
        let name_start = cursor + 10;
        let name_end = name_start
            .checked_add(name_length)
            .filter(|name_end| *name_end + 4 <= item_end)
            .ok_or_else(|| data_error("FunctionFS extended-property name exceeds its item"))?;
        let name = &bytes[name_start..name_end];
        if name.is_empty()
            || name.last() != Some(&0)
            || std::str::from_utf8(&name[..name.len() - 1]).is_err()
        {
            return invalid("FunctionFS extended-property name must be NUL-terminated UTF-8");
        }
        let data_length = le_u32(bytes, name_end)? as usize;
        if name_end + 4 + data_length != item_end {
            return invalid("FunctionFS extended-property data length does not match its item");
        }
        cursor = item_end;
    }
    if cursor != end {
        return invalid("FunctionFS extended-properties descriptor contains trailing data");
    }
    Ok(())
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

fn le_u16(bytes: &[u8], offset: usize) -> io::Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| data_error("truncated FunctionFS 16-bit field"))?;
    Ok(u16::from_le_bytes(value.try_into().unwrap()))
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

    fn descriptors_with_winusb() -> Vec<u8> {
        let mut bytes = descriptors(0x82);
        bytes[8..12].copy_from_slice(&(HAS_FS_DESC | HAS_HS_DESC | HAS_MS_OS_DESC).to_le_bytes());
        bytes.splice(20..20, 1_u32.to_le_bytes());
        bytes.extend_from_slice(&[
            0x00, 0x23, 0x00, 0x00, 0x00, 0x00, 0x01, 0x04, 0x00, 0x01, 0x00, 0x00, 0x01, b'W',
            b'I', b'N', b'U', b'S', b'B', 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]);
        let length = bytes.len() as u32;
        bytes[4..8].copy_from_slice(&length.to_le_bytes());
        bytes
    }

    #[test]
    fn derives_endpoint_order_and_direction() {
        assert_eq!(
            inspect(&descriptors(0x82), &strings()).unwrap(),
            Inspection {
                endpoints: vec![
                    Endpoint {
                        direction: Direction::Out,
                        transfer_type: 2,
                    },
                    Endpoint {
                        direction: Direction::In,
                        transfer_type: 3,
                    }
                ],
                has_ms_os_descriptors: false,
            }
        );
    }

    #[test]
    fn accepts_winusb_compatible_id_for_a_declared_interface() {
        let inspection = inspect(&descriptors_with_winusb(), &strings()).unwrap();
        assert!(inspection.has_ms_os_descriptors);
        assert_eq!(inspection.endpoints.len(), 2);
    }

    #[test]
    fn rejects_winusb_compatible_id_for_a_missing_interface() {
        let mut descriptors = descriptors_with_winusb();
        let feature_interface = descriptors.len() - 24;
        descriptors[feature_interface] = 1;
        assert!(inspect(&descriptors, &strings()).is_err());
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
