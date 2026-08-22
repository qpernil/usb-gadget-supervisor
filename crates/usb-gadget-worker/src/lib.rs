//! Shared worker-side model for publishing USB personalities to the supervisor.

use serde::{Deserialize, Serialize};
use std::io::{self, Cursor};

mod discovery;
pub use discovery::{discover, SetupPacket};

pub const PERSONALITY_SCHEMA: u16 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UsbSpeed {
    LowSpeed,
    FullSpeed,
    HighSpeed,
}

impl UsbSpeed {
    pub fn configfs_name(self) -> &'static str {
        match self {
            Self::LowSpeed => "low-speed",
            Self::FullSpeed => "full-speed",
            Self::HighSpeed => "high-speed",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsbPersonality {
    pub schema: u16,
    pub max_speed: UsbSpeed,
    #[serde(with = "serde_bytes")]
    pub device_descriptor: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub configuration_descriptor: Vec<u8>,
    pub strings: Vec<StringDescriptor>,
    pub microsoft_os_1: Option<MicrosoftOs10>,
    pub webusb: Option<WebUsb>,
}

impl UsbPersonality {
    pub fn new(
        max_speed: UsbSpeed,
        device_descriptor: impl Into<Vec<u8>>,
        configuration_descriptor: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            schema: PERSONALITY_SCHEMA,
            max_speed,
            device_descriptor: device_descriptor.into(),
            configuration_descriptor: configuration_descriptor.into(),
            strings: Vec::new(),
            microsoft_os_1: None,
            webusb: None,
        }
    }

    pub fn with_string(mut self, descriptor: StringDescriptor) -> Self {
        self.strings.push(descriptor);
        self
    }

    pub fn with_microsoft_os_1(mut self, capability: MicrosoftOs10) -> Self {
        self.microsoft_os_1 = Some(capability);
        self
    }

    pub fn with_webusb(mut self, capability: WebUsb) -> Self {
        self.webusb = Some(capability);
        self
    }

    pub fn to_cbor(&self) -> io::Result<Vec<u8>> {
        let mut output = Vec::new();
        ciborium::into_writer(self, &mut output)
            .map_err(|error| io::Error::other(format!("encode USB personality CBOR: {error}")))?;
        Ok(output)
    }

    pub fn from_cbor(bytes: &[u8]) -> io::Result<Self> {
        let mut input = Cursor::new(bytes);
        let personality = ciborium::from_reader(&mut input)
            .map_err(|error| io::Error::other(format!("decode USB personality CBOR: {error}")))?;
        if input.position() != bytes.len() as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "USB personality CBOR contains trailing data",
            ));
        }
        Ok(personality)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StringDescriptor {
    pub index: u8,
    pub language_id: u16,
    #[serde(with = "serde_bytes")]
    pub descriptor: Vec<u8>,
}

impl StringDescriptor {
    pub fn new(index: u8, language_id: u16, descriptor: impl Into<Vec<u8>>) -> Self {
        Self {
            index,
            language_id,
            descriptor: descriptor.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MicrosoftOs10 {
    pub vendor_code: u8,
    pub compatible_ids: Vec<MicrosoftCompatibleId>,
    pub registry_properties: Vec<MicrosoftRegistryProperty>,
}

impl MicrosoftOs10 {
    pub fn new(vendor_code: u8) -> Self {
        Self {
            vendor_code,
            compatible_ids: Vec::new(),
            registry_properties: Vec::new(),
        }
    }

    pub fn with_compatible_id(mut self, compatible_id: MicrosoftCompatibleId) -> Self {
        self.compatible_ids.push(compatible_id);
        self
    }

    pub fn with_registry_property(mut self, property: MicrosoftRegistryProperty) -> Self {
        self.registry_properties.push(property);
        self
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MicrosoftCompatibleId {
    pub interface: u8,
    pub compatible_id: String,
    pub sub_compatible_id: String,
}

impl MicrosoftCompatibleId {
    pub fn new(
        interface: u8,
        compatible_id: impl Into<String>,
        sub_compatible_id: impl Into<String>,
    ) -> Self {
        Self {
            interface,
            compatible_id: compatible_id.into(),
            sub_compatible_id: sub_compatible_id.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MicrosoftRegistryProperty {
    pub interface: u8,
    pub data_type: u32,
    pub name: String,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

impl MicrosoftRegistryProperty {
    pub fn new(
        interface: u8,
        data_type: u32,
        name: impl Into<String>,
        data: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            interface,
            data_type,
            name: name.into(),
            data: data.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebUsb {
    pub version: u16,
    pub vendor_code: u8,
    pub landing_page: String,
}

impl WebUsb {
    pub fn new(version: u16, vendor_code: u8, landing_page: impl Into<String>) -> Self {
        Self {
            version,
            vendor_code,
            landing_page: landing_page.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_round_trip_as_cbor_byte_strings() {
        let personality =
            UsbPersonality::new(UsbSpeed::FullSpeed, vec![18, 1, 0, 2], vec![9, 2, 9, 0])
                .with_string(StringDescriptor::new(1, 0x0409, vec![4, 3, b'A', 0]))
                .with_webusb(WebUsb::new(0x0100, 1, "https://example.test"));
        let encoded = personality.to_cbor().unwrap();
        assert!(encoded.windows(4).any(|bytes| bytes == [0x44, 18, 1, 0]));
        let decoded = UsbPersonality::from_cbor(&encoded).unwrap();
        assert_eq!(decoded.max_speed, UsbSpeed::FullSpeed);
        assert_eq!(decoded.strings[0].descriptor, [4, 3, b'A', 0]);
    }
}
