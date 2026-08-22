use crate::{
    MicrosoftCompatibleId, MicrosoftOs10, MicrosoftRegistryProperty, StringDescriptor,
    UsbPersonality, UsbSpeed, WebUsb,
};
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

const USB_REQ_GET_DESCRIPTOR: u8 = 0x06;
const USB_REQ_SET_CONFIGURATION: u8 = 0x09;
const USB_DT_DEVICE: u8 = 0x01;
const USB_DT_CONFIGURATION: u8 = 0x02;
const USB_DT_STRING: u8 = 0x03;
const USB_DT_INTERFACE: u8 = 0x04;
const USB_DT_BOS: u8 = 0x0f;
const USB_DT_DEVICE_CAPABILITY: u8 = 0x10;
const USB_DC_PLATFORM: u8 = 0x05;
const WEBUSB_UUID: [u8; 16] = [
    0x38, 0xb6, 0x08, 0x34, 0xa9, 0x09, 0xa0, 0x47, 0x8b, 0xfd, 0xa0, 0x76, 0x88, 0x15, 0xb6, 0x65,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetupPacket {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl SetupPacket {
    fn encode(self) -> [u8; 8] {
        let value = self.value.to_le_bytes();
        let index = self.index.to_le_bytes();
        let length = self.length.to_le_bytes();
        [
            self.request_type,
            self.request,
            value[0],
            value[1],
            index[0],
            index[1],
            length[0],
            length[1],
        ]
    }
}

pub fn discover<F>(speed: UsbSpeed, mut transfer: F) -> io::Result<UsbPersonality>
where
    F: FnMut(SetupPacket, &[u8]) -> io::Result<Option<Vec<u8>>>,
{
    let device = required_descriptor(&mut transfer, USB_DT_DEVICE, 0, 0, 18)?;
    if device.len() != 18 || device[0] != 18 || device[1] != USB_DT_DEVICE {
        return invalid("firmware returned an invalid USB device descriptor");
    }
    if device[17] != 1 {
        return invalid("USB personality must have exactly one configuration");
    }

    let languages = required_descriptor(&mut transfer, USB_DT_STRING, 0, 0, 255)?;
    if languages.len() < 4
        || languages[1] != USB_DT_STRING
        || usize::from(languages[0]) != languages.len()
        || languages.len() % 2 != 0
    {
        return invalid("firmware returned an invalid USB language descriptor");
    }
    let language_ids = languages[2..]
        .chunks_exact(2)
        .map(|word| u16::from_le_bytes([word[0], word[1]]))
        .collect::<BTreeSet<_>>();
    let mut strings = vec![StringDescriptor::new(0, 0, languages)];
    let mut seen_strings = BTreeSet::new();
    let mut string_indices = [device[14], device[15], device[16]]
        .into_iter()
        .filter(|index| *index != 0)
        .collect::<BTreeSet<_>>();

    let config_header = required_descriptor(&mut transfer, USB_DT_CONFIGURATION, 0, 0, 9)?;
    if config_header.len() != 9 || config_header[0] != 9 || config_header[1] != USB_DT_CONFIGURATION
    {
        return invalid("firmware returned an invalid USB configuration header");
    }
    let config_length = le_u16(&config_header, 2)?;
    if config_length < 9 {
        return invalid("firmware returned an invalid USB configuration length");
    }
    let configuration =
        required_descriptor(&mut transfer, USB_DT_CONFIGURATION, 0, 0, config_length)?;
    if configuration.len() != usize::from(config_length) {
        return invalid("firmware returned a truncated USB configuration");
    }
    if configuration[6] != 0 {
        string_indices.insert(configuration[6]);
    }
    let mut interfaces = BTreeSet::new();
    let mut offset = 9;
    while offset < configuration.len() {
        if configuration.len() - offset < 2 {
            return invalid("firmware returned a truncated configuration descriptor");
        }
        let length = usize::from(configuration[offset]);
        if length < 2 || length > configuration.len() - offset {
            return invalid("firmware returned a malformed configuration descriptor");
        }
        if configuration[offset + 1] == USB_DT_INTERFACE {
            if length < 9 {
                return invalid("firmware returned a short USB interface descriptor");
            }
            interfaces.insert(configuration[offset + 2]);
            if configuration[offset + 8] != 0 {
                string_indices.insert(configuration[offset + 8]);
            }
        }
        offset += length;
    }
    for language in language_ids {
        for index in &string_indices {
            discover_string(
                &mut transfer,
                &mut strings,
                &mut seen_strings,
                *index,
                language,
            )?;
        }
    }

    let microsoft_os_1 = discover_microsoft(&mut transfer, &mut strings, &interfaces)?;
    let webusb = discover_webusb(&mut transfer, le_u16(&device, 2)?)?;
    Ok(UsbPersonality {
        schema: crate::PERSONALITY_SCHEMA,
        max_speed: speed,
        device_descriptor: device,
        configuration_descriptor: configuration,
        strings,
        microsoft_os_1,
        webusb,
    })
}

fn discover_string<F>(
    transfer: &mut F,
    strings: &mut Vec<StringDescriptor>,
    seen: &mut BTreeSet<(u8, u16)>,
    index: u8,
    language: u16,
) -> io::Result<()>
where
    F: FnMut(SetupPacket, &[u8]) -> io::Result<Option<Vec<u8>>>,
{
    if index == 0 || !seen.insert((index, language)) {
        return Ok(());
    }
    let descriptor = required_descriptor(transfer, USB_DT_STRING, index, language, 255)?;
    if descriptor.len() < 2 || descriptor[1] != USB_DT_STRING {
        return invalid("firmware returned an invalid USB string descriptor");
    }
    strings.push(StringDescriptor::new(index, language, descriptor));
    Ok(())
}

fn discover_microsoft<F>(
    transfer: &mut F,
    strings: &mut Vec<StringDescriptor>,
    interfaces: &BTreeSet<u8>,
) -> io::Result<Option<MicrosoftOs10>>
where
    F: FnMut(SetupPacket, &[u8]) -> io::Result<Option<Vec<u8>>>,
{
    let Some(os_string) = optional_descriptor(transfer, USB_DT_STRING, 0xee, 0, 255)? else {
        return Ok(None);
    };
    if os_string.len() < 18 || os_string[1] != USB_DT_STRING {
        return invalid("firmware returned an invalid Microsoft OS string descriptor");
    }
    let signature = os_string[2..16]
        .chunks_exact(2)
        .map(|word| u16::from_le_bytes([word[0], word[1]]))
        .collect::<Vec<_>>();
    if String::from_utf16(&signature).ok().as_deref() != Some("MSFT100") || os_string[16] == 0 {
        return invalid("firmware returned an unsupported Microsoft OS string descriptor");
    }
    strings.push(StringDescriptor::new(0xee, 0, os_string));
    let vendor_code = strings.last().unwrap().descriptor[16];
    let mut microsoft = MicrosoftOs10::new(vendor_code);

    if let Some(compatible) = transfer(
        SetupPacket {
            request_type: 0xc0,
            request: vendor_code,
            value: 0,
            index: 4,
            length: 255,
        },
        &[],
    )? {
        if compatible.len() < 16 || le_u32(&compatible, 0)? as usize > compatible.len() {
            return invalid("firmware returned an invalid Microsoft compatible-ID descriptor");
        }
        let count = usize::from(compatible[8]);
        if count == 0 || 16 + count * 24 > compatible.len() {
            return invalid("firmware returned a truncated Microsoft compatible-ID descriptor");
        }
        for section in compatible[16..16 + count * 24].chunks_exact(24) {
            if !interfaces.contains(&section[0]) {
                return invalid("Microsoft compatible ID names an unknown interface");
            }
            microsoft.compatible_ids.push(MicrosoftCompatibleId::new(
                section[0],
                usb_identifier(&section[2..10])?,
                usb_identifier(&section[10..18])?,
            ));
        }
    }
    for interface in interfaces {
        let Some(properties) = transfer(
            SetupPacket {
                request_type: 0xc1,
                request: vendor_code,
                value: u16::from(*interface),
                index: 5,
                length: 255,
            },
            &[],
        )?
        else {
            continue;
        };
        if properties.len() < 10 || le_u32(&properties, 0)? as usize > properties.len() {
            return invalid("firmware returned an invalid Microsoft properties descriptor");
        }
        let count = usize::from(le_u16(&properties, 8)?);
        let mut offset = 10;
        for _ in 0..count {
            if properties.len() - offset < 14 {
                return invalid("firmware returned a truncated Microsoft registry property");
            }
            let feature_length = le_u32(&properties, offset)? as usize;
            let data_type = le_u32(&properties, offset + 4)?;
            let name_length = usize::from(le_u16(&properties, offset + 8)?);
            if feature_length < 14
                || feature_length > properties.len() - offset
                || name_length > feature_length - 14
            {
                return invalid("firmware returned an invalid Microsoft registry property");
            }
            let name_start = offset + 10;
            let data_length_offset = name_start + name_length;
            if data_length_offset + 4 > offset + feature_length {
                return invalid("firmware returned a truncated Microsoft registry property");
            }
            let data_length = le_u32(&properties, data_length_offset)? as usize;
            let data_start = data_length_offset + 4;
            if data_length > offset + feature_length - data_start {
                return invalid("firmware returned a truncated Microsoft registry value");
            }
            let name = utf16le_string(&properties[name_start..data_length_offset])?;
            microsoft
                .registry_properties
                .push(MicrosoftRegistryProperty::new(
                    *interface,
                    data_type,
                    name,
                    properties[data_start..data_start + data_length].to_vec(),
                ));
            offset += feature_length;
        }
    }
    Ok(Some(microsoft))
}

fn discover_webusb<F>(transfer: &mut F, bcd_usb: u16) -> io::Result<Option<WebUsb>>
where
    F: FnMut(SetupPacket, &[u8]) -> io::Result<Option<Vec<u8>>>,
{
    if bcd_usb < 0x0201 {
        return Ok(None);
    }
    let Some(header) = optional_descriptor(transfer, USB_DT_BOS, 0, 0, 5)? else {
        return Ok(None);
    };
    if header.len() != 5 || header[1] != USB_DT_BOS {
        return invalid("firmware returned an invalid BOS header");
    }
    let total = le_u16(&header, 2)?;
    let bos = required_descriptor(transfer, USB_DT_BOS, 0, 0, total)?;
    if bos.len() != usize::from(total) {
        return invalid("firmware returned a truncated BOS descriptor");
    }
    let mut offset = 5;
    while offset < bos.len() {
        let length = usize::from(bos[offset]);
        if length < 3 || length > bos.len() - offset {
            return invalid("firmware returned a malformed BOS capability");
        }
        let capability = &bos[offset..offset + length];
        if length >= 24
            && capability[1] == USB_DT_DEVICE_CAPABILITY
            && capability[2] == USB_DC_PLATFORM
            && capability[4..20] == WEBUSB_UUID
        {
            let version = le_u16(capability, 20)?;
            let vendor_code = capability[22];
            let landing_index = capability[23];
            let landing_page = if landing_index == 0 {
                String::new()
            } else {
                let setup = SetupPacket {
                    request_type: 0x00,
                    request: USB_REQ_SET_CONFIGURATION,
                    value: 1,
                    index: 0,
                    length: 0,
                };
                if transfer(setup, &[])?.is_none() {
                    return invalid("firmware rejected SET_CONFIGURATION during discovery");
                }
                let response = transfer(
                    SetupPacket {
                        request_type: 0xc0,
                        request: vendor_code,
                        value: u16::from(landing_index),
                        index: 2,
                        length: 255,
                    },
                    &[],
                )?
                .ok_or_else(|| data_error("firmware stalled its WebUSB URL request"))?;
                webusb_url(&response)?
            };
            return Ok(Some(WebUsb::new(version, vendor_code, landing_page)));
        }
        offset += length;
    }
    Ok(None)
}

fn required_descriptor<F>(
    transfer: &mut F,
    descriptor_type: u8,
    descriptor_index: u8,
    language: u16,
    length: u16,
) -> io::Result<Vec<u8>>
where
    F: FnMut(SetupPacket, &[u8]) -> io::Result<Option<Vec<u8>>>,
{
    optional_descriptor(
        transfer,
        descriptor_type,
        descriptor_index,
        language,
        length,
    )?
    .ok_or_else(|| data_error("firmware stalled a required descriptor request"))
}

fn optional_descriptor<F>(
    transfer: &mut F,
    descriptor_type: u8,
    descriptor_index: u8,
    language: u16,
    length: u16,
) -> io::Result<Option<Vec<u8>>>
where
    F: FnMut(SetupPacket, &[u8]) -> io::Result<Option<Vec<u8>>>,
{
    transfer(
        SetupPacket {
            request_type: 0x80,
            request: USB_REQ_GET_DESCRIPTOR,
            value: u16::from(descriptor_type) << 8 | u16::from(descriptor_index),
            index: language,
            length,
        },
        &[],
    )
}

fn usb_identifier(bytes: &[u8]) -> io::Result<String> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    if !bytes[..end].is_ascii() {
        return invalid("Microsoft compatible ID is not ASCII");
    }
    Ok(String::from_utf8(bytes[..end].to_vec()).unwrap())
}

fn utf16le_string(bytes: &[u8]) -> io::Result<String> {
    if bytes.len() % 2 != 0 {
        return invalid("Microsoft registry property name has an odd length");
    }
    let mut words = bytes
        .chunks_exact(2)
        .map(|word| u16::from_le_bytes([word[0], word[1]]))
        .collect::<Vec<_>>();
    if words.last() == Some(&0) {
        words.pop();
    }
    String::from_utf16(&words).map_err(|_| data_error("invalid UTF-16 registry property name"))
}

fn webusb_url(bytes: &[u8]) -> io::Result<String> {
    if bytes.len() < 3 || usize::from(bytes[0]) > bytes.len() || bytes[1] != 3 {
        return invalid("firmware returned an invalid WebUSB URL descriptor");
    }
    let prefix = match bytes[2] {
        0 => "http://",
        1 => "https://",
        255 => "",
        _ => return invalid("firmware returned an unsupported WebUSB URL scheme"),
    };
    let suffix = std::str::from_utf8(&bytes[3..usize::from(bytes[0])])
        .map_err(|_| data_error("WebUSB URL is not UTF-8"))?;
    Ok(format!("{prefix}{suffix}"))
}

fn le_u16(bytes: &[u8], offset: usize) -> io::Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| data_error("truncated USB 16-bit field"))?;
    Ok(u16::from_le_bytes(value.try_into().unwrap()))
}

fn le_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| data_error("truncated USB 32-bit field"))?;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

fn invalid<T>(message: impl Into<String>) -> io::Result<T> {
    Err(data_error(message))
}

fn data_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

pub type ControlTransferCallback = unsafe extern "C" fn(
    context: *mut c_void,
    setup: *const u8,
    out_data: *const u8,
    out_length: usize,
    response: *mut *const u8,
    response_length: *mut usize,
) -> bool;

#[no_mangle]
pub unsafe extern "C" fn ugsp_discover_usb_personality(
    speed: u8,
    callback: Option<ControlTransferCallback>,
    context: *mut c_void,
    output: *mut *mut u8,
    output_length: *mut usize,
) -> bool {
    if output.is_null() || output_length.is_null() {
        return false;
    }
    unsafe {
        *output = ptr::null_mut();
        *output_length = 0;
    }
    let Some(callback) = callback else {
        return false;
    };
    let speed = match speed {
        0 => UsbSpeed::LowSpeed,
        1 => UsbSpeed::FullSpeed,
        2 => UsbSpeed::HighSpeed,
        _ => return false,
    };
    let result = catch_unwind(AssertUnwindSafe(|| {
        discover(speed, |setup, out_data| {
            let setup = setup.encode();
            let mut response = ptr::null();
            let mut response_length = 0;
            let handled = unsafe {
                callback(
                    context,
                    setup.as_ptr(),
                    out_data.as_ptr(),
                    out_data.len(),
                    &mut response,
                    &mut response_length,
                )
            };
            if !handled {
                return Ok(None);
            }
            if response_length != 0 && response.is_null() {
                return invalid("firmware callback returned a null response");
            }
            let response = if response_length == 0 {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(response, response_length) }.to_vec()
            };
            Ok(Some(response))
        })?
        .to_cbor()
    }));
    let Ok(Ok(encoded)) = result else {
        return false;
    };
    let bytes = encoded.into_boxed_slice();
    let length = bytes.len();
    let raw = Box::into_raw(bytes).cast::<u8>();
    unsafe {
        *output = raw;
        *output_length = length;
    }
    true
}

#[no_mangle]
pub unsafe extern "C" fn ugsp_personality_cbor_free(bytes: *mut u8, length: usize) {
    if !bytes.is_null() {
        let slice = ptr::slice_from_raw_parts_mut(bytes, length);
        drop(unsafe { Box::from_raw(slice) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_a_profile_from_standard_control_transfers() {
        let device = vec![
            18, 1, 0x00, 0x02, 0, 0, 0, 64, 0x09, 0x12, 0x01, 0x00, 0x00, 0x01, 1, 2, 3, 1,
        ];
        let configuration = vec![
            9, 2, 25, 0, 1, 1, 0, 0x80, 25, 9, 4, 0, 0, 1, 3, 0, 0, 0, 7, 5, 0x81, 3, 64, 0, 1,
        ];
        let string = |text: &str| {
            let words = text.encode_utf16().collect::<Vec<_>>();
            let mut descriptor = vec![(2 + words.len() * 2) as u8, 3];
            for word in words {
                descriptor.extend_from_slice(&word.to_le_bytes());
            }
            descriptor
        };
        let personality = discover(UsbSpeed::FullSpeed, |setup, _| {
            let descriptor_type = (setup.value >> 8) as u8;
            let descriptor_index = setup.value as u8;
            let response = match (setup.request, descriptor_type, descriptor_index) {
                (USB_REQ_GET_DESCRIPTOR, USB_DT_DEVICE, 0) => Some(device.clone()),
                (USB_REQ_GET_DESCRIPTOR, USB_DT_CONFIGURATION, 0) => Some(configuration.clone()),
                (USB_REQ_GET_DESCRIPTOR, USB_DT_STRING, 0) => Some(vec![4, 3, 0x09, 0x04]),
                (USB_REQ_GET_DESCRIPTOR, USB_DT_STRING, 1) => Some(string("Example")),
                (USB_REQ_GET_DESCRIPTOR, USB_DT_STRING, 2) => Some(string("Device")),
                (USB_REQ_GET_DESCRIPTOR, USB_DT_STRING, 3) => Some(string("serial")),
                _ => None,
            };
            Ok(response.map(|bytes| bytes[..bytes.len().min(usize::from(setup.length))].to_vec()))
        })
        .unwrap();
        assert_eq!(personality.max_speed, UsbSpeed::FullSpeed);
        assert_eq!(personality.device_descriptor, device);
        assert_eq!(personality.configuration_descriptor, configuration);
        assert_eq!(personality.strings.len(), 4);
        assert!(personality.microsoft_os_1.is_none());
        assert!(personality.webusb.is_none());
    }
}
