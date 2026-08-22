//! Validation and Linux FunctionFS projection of a worker-owned USB personality.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use usb_gadget_worker::{
    MicrosoftOs10 as BundleMicrosoftOs10, UsbPersonality as PersonalityBundle,
    WebUsb as BundleWebUsb, PERSONALITY_SCHEMA,
};

const USB_DT_DEVICE: u8 = 0x01;
const USB_DT_CONFIGURATION: u8 = 0x02;
const USB_DT_STRING: u8 = 0x03;
const USB_DT_INTERFACE: u8 = 0x04;
const USB_DT_ENDPOINT: u8 = 0x05;

const FUNCTIONFS_DESCRIPTORS_MAGIC_V2: u32 = 3;
const FUNCTIONFS_STRINGS_MAGIC: u32 = 2;
const FUNCTIONFS_HAS_FS_DESC: u32 = 1;
const FUNCTIONFS_HAS_MS_OS_DESC: u32 = 8;
const FUNCTIONFS_ALL_CTRL_RECIP: u32 = 64;
const FUNCTIONFS_CONFIG0_SETUP: u32 = 128;
const MAX_STRING_DESCRIPTORS: usize = 256;

pub(crate) fn discover_bundle(bytes: &[u8]) -> io::Result<(PersonalityBundle, Personality)> {
    let bundle = PersonalityBundle::from_cbor(bytes)?;
    if bundle.schema != PERSONALITY_SCHEMA {
        return invalid(format!(
            "unsupported USB personality schema {}",
            bundle.schema
        ));
    }
    let max_speed = bundle.max_speed.configfs_name();
    if bundle.strings.len() > MAX_STRING_DESCRIPTORS {
        return invalid("USB personality contains too many string descriptors");
    }

    let mut strings = BTreeMap::new();
    for descriptor in &bundle.strings {
        if strings
            .insert(
                (descriptor.index, descriptor.language_id),
                descriptor.descriptor.to_vec(),
            )
            .is_some()
        {
            return invalid("USB personality contains a duplicate string descriptor");
        }
    }
    let mut personality = project(
        bundle.device_descriptor.as_ref(),
        bundle.configuration_descriptor.as_ref(),
        &strings,
        bundle.microsoft_os_1.as_ref(),
        bundle.webusb.as_ref(),
    )?;
    personality.max_speed = max_speed;
    Ok((bundle, personality))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DeviceIdentity {
    pub(crate) vendor_id: u16,
    pub(crate) product_id: u16,
    pub(crate) bcd_usb: u16,
    pub(crate) bcd_device: u16,
    pub(crate) device_class: u8,
    pub(crate) device_subclass: u8,
    pub(crate) device_protocol: u8,
    pub(crate) manufacturer: Option<String>,
    pub(crate) product: Option<String>,
    pub(crate) serial: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Endpoint {
    pub(crate) address: u8,
    pub(crate) transfer_type: u8,
    pub(crate) max_packet_size: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MicrosoftOs10 {
    pub(crate) vendor_code: u8,
    pub(crate) signature: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WebUsb {
    pub(crate) version: u16,
    pub(crate) vendor_code: u8,
    pub(crate) landing_page: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Personality {
    pub(crate) device: DeviceIdentity,
    pub(crate) max_speed: &'static str,
    pub(crate) configuration_attributes: u8,
    pub(crate) max_power_ma: u16,
    pub(crate) descriptors: Vec<u8>,
    pub(crate) strings: Vec<u8>,
    pub(crate) endpoints: Vec<Endpoint>,
    pub(crate) microsoft_os_1: Option<MicrosoftOs10>,
    pub(crate) webusb: Option<WebUsb>,
}

fn project(
    device: &[u8],
    config: &[u8],
    strings: &BTreeMap<(u8, u16), Vec<u8>>,
    microsoft: Option<&BundleMicrosoftOs10>,
    webusb: Option<&BundleWebUsb>,
) -> io::Result<Personality> {
    if device.len() != 18 || device[0] != 18 || device[1] != USB_DT_DEVICE {
        return invalid("USB personality contains an invalid device descriptor");
    }
    if device[17] != 1 {
        return invalid("Linux projection currently requires exactly one USB configuration");
    }
    let language = language_id(strings)?;
    let identity = DeviceIdentity {
        vendor_id: le_u16(device, 8)?,
        product_id: le_u16(device, 10)?,
        bcd_usb: le_u16(device, 2)?,
        bcd_device: le_u16(device, 12)?,
        device_class: device[4],
        device_subclass: device[5],
        device_protocol: device[6],
        manufacturer: optional_string(strings, device[14], language)?,
        product: optional_string(strings, device[15], language)?,
        serial: optional_string(strings, device[16], language)?,
    };

    if config.len() < 9 || config[0] != 9 || config[1] != USB_DT_CONFIGURATION {
        return invalid("USB personality contains an invalid configuration descriptor");
    }
    let total_length = le_u16(config, 2)? as usize;
    if !(9..=u16::MAX as usize).contains(&total_length) {
        return invalid("USB personality contains an invalid configuration length");
    }
    if config.len() != total_length || config[5] != 1 {
        return invalid("USB personality contains an unsupported USB configuration");
    }

    let mut body = config[9..].to_vec();
    let (descriptor_count, endpoints, interface_strings, interfaces) =
        inspect_and_rewrite_configuration(&mut body)?;
    let strings = functionfs_strings(strings, language, &interface_strings)?;
    let (microsoft_os_1, os_descriptors) = project_microsoft_os_1(microsoft, &interfaces)?;
    let webusb = project_webusb(webusb, identity.bcd_usb)?;

    let mut flags = FUNCTIONFS_HAS_FS_DESC | FUNCTIONFS_ALL_CTRL_RECIP | FUNCTIONFS_CONFIG0_SETUP;
    if !os_descriptors.is_empty() {
        flags |= FUNCTIONFS_HAS_MS_OS_DESC;
    }
    let header_words = 3 + 1 + usize::from(!os_descriptors.is_empty());
    let mut descriptors = Vec::with_capacity(header_words * 4 + body.len() + os_descriptors.len());
    descriptors.extend_from_slice(&FUNCTIONFS_DESCRIPTORS_MAGIC_V2.to_le_bytes());
    descriptors.extend_from_slice(&0_u32.to_le_bytes());
    descriptors.extend_from_slice(&flags.to_le_bytes());
    descriptors.extend_from_slice(&(descriptor_count as u32).to_le_bytes());
    if !os_descriptors.is_empty() {
        let os_count = count_functionfs_os_descriptors(&os_descriptors)?;
        descriptors.extend_from_slice(&(os_count as u32).to_le_bytes());
    }
    descriptors.extend_from_slice(&body);
    descriptors.extend_from_slice(&os_descriptors);
    let length = u32::try_from(descriptors.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "FunctionFS descriptor blob is too large",
        )
    })?;
    descriptors[4..8].copy_from_slice(&length.to_le_bytes());

    Ok(Personality {
        device: identity,
        max_speed: "full-speed",
        configuration_attributes: config[7],
        max_power_ma: u16::from(config[8]) * 2,
        descriptors,
        strings,
        endpoints,
        microsoft_os_1,
        webusb,
    })
}

fn language_id(strings: &BTreeMap<(u8, u16), Vec<u8>>) -> io::Result<u16> {
    let descriptor = strings
        .get(&(0, 0))
        .ok_or_else(|| data_error("USB personality has no language descriptor"))?;
    if descriptor.len() < 4
        || descriptor[1] != USB_DT_STRING
        || descriptor[0] as usize > descriptor.len()
    {
        return invalid("USB personality contains an invalid language descriptor");
    }
    le_u16(descriptor, 2)
}

fn optional_string(
    strings: &BTreeMap<(u8, u16), Vec<u8>>,
    index: u8,
    language: u16,
) -> io::Result<Option<String>> {
    if index == 0 {
        return Ok(None);
    }
    usb_string(strings, index, language).map(Some)
}

fn usb_string(
    strings: &BTreeMap<(u8, u16), Vec<u8>>,
    index: u8,
    language: u16,
) -> io::Result<String> {
    let descriptor = strings
        .get(&(index, language))
        .ok_or_else(|| data_error(format!("USB personality has no string descriptor {index}")))?;
    if descriptor.len() < 2
        || descriptor[1] != USB_DT_STRING
        || descriptor[0] as usize > descriptor.len()
        || descriptor[0] % 2 != 0
    {
        return invalid(format!(
            "USB personality contains invalid USB string descriptor {index}"
        ));
    }
    let words = descriptor[2..descriptor[0] as usize]
        .chunks_exact(2)
        .map(|word| u16::from_le_bytes([word[0], word[1]]))
        .collect::<Vec<_>>();
    String::from_utf16(&words).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "USB string contains invalid UTF-16",
        )
    })
}

type ConfigurationInspection = (usize, Vec<Endpoint>, BTreeMap<u8, u8>, BTreeSet<u8>);

fn inspect_and_rewrite_configuration(bytes: &mut [u8]) -> io::Result<ConfigurationInspection> {
    let mut offset = 0;
    let mut count = 0;
    let mut endpoints = Vec::new();
    let mut string_map = BTreeMap::new();
    let mut interfaces = BTreeSet::new();
    while offset < bytes.len() {
        if bytes.len() - offset < 2 {
            return invalid("truncated descriptor in USB configuration");
        }
        let length = bytes[offset] as usize;
        if length < 2 || offset + length > bytes.len() {
            return invalid("invalid descriptor length in USB configuration");
        }
        match bytes[offset + 1] {
            USB_DT_INTERFACE => {
                if length < 9 {
                    return invalid("short interface descriptor in USB configuration");
                }
                interfaces.insert(bytes[offset + 2]);
                let original = bytes[offset + 8];
                if original != 0 {
                    let next = u8::try_from(string_map.len() + 1).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "too many FunctionFS strings")
                    })?;
                    let replacement = *string_map.entry(original).or_insert(next);
                    bytes[offset + 8] = replacement;
                }
            }
            USB_DT_ENDPOINT => {
                if length < 7 {
                    return invalid("short endpoint descriptor in USB configuration");
                }
                endpoints.push(Endpoint {
                    address: bytes[offset + 2],
                    transfer_type: bytes[offset + 3] & 0x03,
                    max_packet_size: le_u16(bytes, offset + 4)?,
                });
            }
            _ => {}
        }
        count += 1;
        offset += length;
    }
    Ok((count, endpoints, string_map, interfaces))
}

fn functionfs_strings(
    strings: &BTreeMap<(u8, u16), Vec<u8>>,
    language: u16,
    mapping: &BTreeMap<u8, u8>,
) -> io::Result<Vec<u8>> {
    let mut ordered = mapping.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|(_, replacement)| **replacement);
    let mut body = Vec::new();
    if !ordered.is_empty() {
        body.extend_from_slice(&language.to_le_bytes());
        for (original, _) in ordered {
            let value = usb_string(strings, *original, language)?;
            if value.as_bytes().contains(&0) {
                return invalid("FunctionFS string contains an embedded NUL byte");
            }
            body.extend_from_slice(value.as_bytes());
            body.push(0);
        }
    }
    let length = 16 + body.len();
    let mut strings = Vec::with_capacity(length);
    strings.extend_from_slice(&FUNCTIONFS_STRINGS_MAGIC.to_le_bytes());
    strings.extend_from_slice(&(length as u32).to_le_bytes());
    strings.extend_from_slice(&(mapping.len() as u32).to_le_bytes());
    strings.extend_from_slice(&(u32::from(!mapping.is_empty())).to_le_bytes());
    strings.extend_from_slice(&body);
    Ok(strings)
}

fn project_microsoft_os_1(
    declaration: Option<&BundleMicrosoftOs10>,
    interfaces: &BTreeSet<u8>,
) -> io::Result<(Option<MicrosoftOs10>, Vec<u8>)> {
    let Some(declaration) = declaration else {
        return Ok((None, Vec::new()));
    };
    if declaration.vendor_code == 0 {
        return invalid("Microsoft OS 1.0 vendor code must not be zero");
    }
    let mut output = Vec::new();
    if !declaration.compatible_ids.is_empty() {
        let mut seen = BTreeSet::new();
        let count = u8::try_from(declaration.compatible_ids.len())
            .map_err(|_| data_error("too many Microsoft compatible IDs"))?;
        let length = 11 + usize::from(count) * 24;
        output.push(declaration.compatible_ids[0].interface);
        output.extend_from_slice(&(length as u32).to_le_bytes());
        output.extend_from_slice(&0x0100_u16.to_le_bytes());
        output.extend_from_slice(&4_u16.to_le_bytes());
        output.push(count);
        output.push(0);
        for compatible in &declaration.compatible_ids {
            if !interfaces.contains(&compatible.interface) || !seen.insert(compatible.interface) {
                return invalid("Microsoft compatible ID names an invalid or duplicate interface");
            }
            output.push(compatible.interface);
            output.push(1);
            output.extend_from_slice(&microsoft_identifier(&compatible.compatible_id)?);
            output.extend_from_slice(&microsoft_identifier(&compatible.sub_compatible_id)?);
            output.extend_from_slice(&[0; 6]);
        }
    }

    let mut properties_by_interface = BTreeMap::<u8, Vec<Vec<u8>>>::new();
    for property in &declaration.registry_properties {
        if !interfaces.contains(&property.interface) || property.name.contains('\0') {
            return invalid("Microsoft registry property names an invalid interface or name");
        }
        let mut name = property.name.encode_utf16().collect::<Vec<_>>();
        name.push(0);
        let name_length = u16::try_from(name.len() * 2)
            .map_err(|_| data_error("Microsoft registry property name is too long"))?;
        let feature_length = 14_usize
            .checked_add(usize::from(name_length))
            .and_then(|length| length.checked_add(property.data.len()))
            .ok_or_else(|| data_error("Microsoft registry property is too large"))?;
        let mut feature = Vec::with_capacity(feature_length);
        feature.extend_from_slice(&(feature_length as u32).to_le_bytes());
        feature.extend_from_slice(&property.data_type.to_le_bytes());
        feature.extend_from_slice(&name_length.to_le_bytes());
        for word in name {
            feature.extend_from_slice(&word.to_le_bytes());
        }
        feature.extend_from_slice(&(property.data.len() as u32).to_le_bytes());
        feature.extend_from_slice(&property.data);
        properties_by_interface
            .entry(property.interface)
            .or_default()
            .push(feature);
    }
    for (interface, features) in properties_by_interface {
        let raw_length = 11 + features.iter().map(Vec::len).sum::<usize>();
        output.push(interface);
        output.extend_from_slice(&(raw_length as u32).to_le_bytes());
        output.extend_from_slice(&0x0100_u16.to_le_bytes());
        output.extend_from_slice(&5_u16.to_le_bytes());
        output.extend_from_slice(
            &u16::try_from(features.len())
                .map_err(|_| data_error("too many Microsoft registry properties"))?
                .to_le_bytes(),
        );
        for feature in features {
            output.extend_from_slice(&feature);
        }
    }
    Ok((
        Some(MicrosoftOs10 {
            vendor_code: declaration.vendor_code,
            signature: "MSFT100".to_owned(),
        }),
        output,
    ))
}

fn microsoft_identifier(value: &str) -> io::Result<[u8; 8]> {
    if value.len() > 8 || !value.is_ascii() || value.as_bytes().contains(&0) {
        return invalid("Microsoft compatible IDs must be ASCII strings up to eight bytes");
    }
    let mut output = [0_u8; 8];
    output[..value.len()].copy_from_slice(value.as_bytes());
    Ok(output)
}

fn project_webusb(declaration: Option<&BundleWebUsb>, bcd_usb: u16) -> io::Result<Option<WebUsb>> {
    let Some(declaration) = declaration else {
        return Ok(None);
    };
    if bcd_usb < 0x0201 || declaration.version != 0x0100 || declaration.vendor_code == 0 {
        return invalid("invalid WebUSB capability declaration");
    }
    if declaration.landing_page.len() > 252
        || !declaration.landing_page.is_ascii()
        || (!declaration.landing_page.is_empty()
            && !declaration.landing_page.starts_with("https://")
            && !declaration.landing_page.starts_with("http://"))
    {
        return invalid("WebUSB landing page must be empty or an ASCII HTTP(S) URL");
    }
    Ok(Some(WebUsb {
        version: declaration.version,
        vendor_code: declaration.vendor_code,
        landing_page: declaration.landing_page.clone(),
    }))
}

fn count_functionfs_os_descriptors(bytes: &[u8]) -> io::Result<usize> {
    let mut offset = 0;
    let mut count = 0;
    while offset < bytes.len() {
        if bytes.len() - offset < 5 {
            return invalid("truncated FunctionFS OS descriptor");
        }
        let length = le_u32(bytes, offset + 1)? as usize;
        if length < 11 || offset + length > bytes.len() {
            return invalid("invalid FunctionFS OS descriptor length");
        }
        offset += length;
        count += 1;
    }
    Ok(count)
}

fn le_u16(bytes: &[u8], offset: usize) -> io::Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated USB 16-bit field"))?;
    Ok(u16::from_le_bytes(value.try_into().unwrap()))
}

fn le_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated USB 32-bit field"))?;
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
    use usb_gadget_worker::{
        MicrosoftCompatibleId, MicrosoftOs10 as BundleMicrosoftOs10, MicrosoftRegistryProperty,
        StringDescriptor, UsbPersonality, UsbSpeed, WebUsb as BundleWebUsb,
    };

    fn string_descriptor(value: &str) -> Vec<u8> {
        let words = value.encode_utf16().collect::<Vec<_>>();
        let mut descriptor = vec![(2 + words.len() * 2) as u8, USB_DT_STRING];
        for word in words {
            descriptor.extend_from_slice(&word.to_le_bytes());
        }
        descriptor
    }

    #[test]
    fn projects_typed_winusb_and_webusb_capabilities() {
        let device = vec![
            18, 1, 0x10, 0x02, 0, 0, 0, 64, 0x09, 0x12, 0xc1, 0x53, 0x01, 0x01, 1, 2, 3, 1,
        ];
        let configuration = vec![
            9, 2, 32, 0, 1, 1, 0, 0x80, 50, 9, 4, 0, 0, 2, 0xff, 0, 0, 4, 7, 5, 0x01, 3, 64, 0, 1,
            7, 5, 0x81, 3, 64, 0, 1,
        ];
        let guid = "{0263b512-88cb-4136-9613-5c8e109d8ef5}"
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let personality = UsbPersonality::new(UsbSpeed::FullSpeed, device, configuration)
            .with_string(StringDescriptor::new(0, 0, vec![4, 3, 0x09, 0x04]))
            .with_string(StringDescriptor::new(
                1,
                0x0409,
                string_descriptor("Virtual Trezor"),
            ))
            .with_string(StringDescriptor::new(
                2,
                0x0409,
                string_descriptor("Virtual Trezor"),
            ))
            .with_string(StringDescriptor::new(
                3,
                0x0409,
                string_descriptor("serial"),
            ))
            .with_string(StringDescriptor::new(
                4,
                0x0409,
                string_descriptor("Trezor Interface"),
            ))
            .with_microsoft_os_1(
                BundleMicrosoftOs10::new(0x21)
                    .with_compatible_id(MicrosoftCompatibleId::new(0, "WINUSB", ""))
                    .with_registry_property(MicrosoftRegistryProperty::new(
                        0,
                        7,
                        "DeviceInterfaceGUIDs",
                        guid,
                    )),
            )
            .with_webusb(BundleWebUsb::new(0x0100, 1, ""));

        let (_, projected) = discover_bundle(&personality.to_cbor().unwrap()).unwrap();
        assert_eq!(projected.device.vendor_id, 0x1209);
        assert_eq!(projected.device.product_id, 0x53c1);
        assert_eq!(projected.endpoints.len(), 2);
        assert_eq!(projected.endpoints[0].address, 0x01);
        assert_eq!(projected.endpoints[1].address, 0x81);
        assert_eq!(projected.microsoft_os_1.as_ref().unwrap().vendor_code, 0x21);
        assert_eq!(projected.webusb.as_ref().unwrap().vendor_code, 1);
        assert_ne!(
            u32::from_le_bytes(projected.descriptors[8..12].try_into().unwrap())
                & FUNCTIONFS_HAS_MS_OS_DESC,
            0
        );
    }

    #[test]
    fn rejects_missing_referenced_strings() {
        let personality = UsbPersonality::new(
            UsbSpeed::FullSpeed,
            vec![18, 1, 0, 2, 0, 0, 0, 64, 0x09, 0x12, 1, 0, 0, 1, 1, 0, 0, 1],
            vec![9, 2, 9, 0, 0, 1, 0, 0x80, 25],
        )
        .with_string(StringDescriptor::new(0, 0, vec![4, 3, 0x09, 0x04]));
        assert!(discover_bundle(&personality.to_cbor().unwrap()).is_err());
    }
}
