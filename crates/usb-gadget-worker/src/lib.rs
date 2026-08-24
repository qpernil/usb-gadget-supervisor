//! Shared worker-side model for publishing USB personalities to the supervisor.

use serde::{Deserialize, Serialize};
use std::ffi::{c_char, CStr};
use std::io::{self, Cursor};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::slice;
use std::sync::{Condvar, Mutex};

mod discovery;
pub use discovery::{discover, SetupPacket};

pub const USB_BUS_EVENT_BODY_LENGTH: usize = 9;

/// Coordinates blocking FunctionFS endpoint helpers with USB activation and
/// worker quiescence.
///
/// FunctionFS cancels pending I/O when an endpoint is disabled, but a new
/// blocking operation issued while disabled waits for the endpoint to be
/// enabled again. Endpoint helpers therefore wait here after cancellation and
/// only retry for a strictly newer activation. Quiescence wakes every waiter.
#[derive(Debug, Default)]
pub struct EndpointLifecycle {
    state: Mutex<EndpointLifecycleState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct EndpointLifecycleState {
    activation: u64,
    stopping: bool,
}

impl EndpointLifecycle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn activate(&self, activation: u64) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.stopping && activation > state.activation {
            state.activation = activation;
            self.changed.notify_all();
        }
    }

    pub fn stop(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.stopping {
            state.stopping = true;
            self.changed.notify_all();
        }
    }

    /// Waits for an endpoint activation newer than `observed`.
    ///
    /// Returns `None` once the generation is stopping.
    pub fn wait_for_activation_after(&self, observed: u64) -> Option<u64> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        while !state.stopping && state.activation <= observed {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
        (!state.stopping).then_some(state.activation)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum UsbBusEvent {
    Bind = 0,
    Unbind = 1,
    Enable = 2,
    Disable = 3,
    Suspend = 5,
    Resume = 6,
}

impl UsbBusEvent {
    pub fn from_byte(value: u8) -> io::Result<Self> {
        match value {
            0 => Ok(Self::Bind),
            1 => Ok(Self::Unbind),
            2 => Ok(Self::Enable),
            3 => Ok(Self::Disable),
            5 => Ok(Self::Suspend),
            6 => Ok(Self::Resume),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown USB bus event {value}"),
            )),
        }
    }

    pub fn encode(self, activation: u64) -> [u8; USB_BUS_EVENT_BODY_LENGTH] {
        let mut body = [0_u8; USB_BUS_EVENT_BODY_LENGTH];
        body[0] = self as u8;
        body[1..].copy_from_slice(&activation.to_be_bytes());
        body
    }

    pub fn decode(body: &[u8]) -> io::Result<(Self, u64)> {
        let body: &[u8; USB_BUS_EVENT_BODY_LENGTH] = body.try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid USB bus event body")
        })?;
        Ok((
            Self::from_byte(body[0])?,
            u64::from_be_bytes(body[1..].try_into().unwrap()),
        ))
    }
}

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

/// The single construction path for both firmware discovery and native
/// workers. Discovery supplies the descriptor bytes returned by firmware;
/// native adapters may first construct those bytes from their platform USB
/// configuration calls.
#[derive(Clone, Debug)]
pub struct UsbPersonalityBuilder {
    max_speed: UsbSpeed,
    device_descriptor: Option<Vec<u8>>,
    configuration_descriptor: Option<Vec<u8>>,
    strings: Vec<StringDescriptor>,
    microsoft_os_1: Option<MicrosoftOs10>,
    webusb: Option<WebUsb>,
}

impl UsbPersonalityBuilder {
    pub fn new(max_speed: UsbSpeed) -> Self {
        Self {
            max_speed,
            device_descriptor: None,
            configuration_descriptor: None,
            strings: Vec::new(),
            microsoft_os_1: None,
            webusb: None,
        }
    }

    pub fn device_descriptor(&mut self, descriptor: impl Into<Vec<u8>>) -> &mut Self {
        self.device_descriptor = Some(descriptor.into());
        self
    }

    pub fn configuration_descriptor(&mut self, descriptor: impl Into<Vec<u8>>) -> &mut Self {
        self.configuration_descriptor = Some(descriptor.into());
        self
    }

    pub fn string_descriptor(&mut self, descriptor: StringDescriptor) -> &mut Self {
        self.strings.push(descriptor);
        self
    }

    pub fn microsoft_os_1(&mut self, capability: MicrosoftOs10) -> &mut Self {
        self.microsoft_os_1 = Some(capability);
        self
    }

    pub fn webusb(&mut self, capability: WebUsb) -> &mut Self {
        self.webusb = Some(capability);
        self
    }

    pub fn finish(self) -> io::Result<UsbPersonality> {
        let device_descriptor = self.device_descriptor.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "missing USB device descriptor")
        })?;
        let configuration_descriptor = self.configuration_descriptor.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "missing USB configuration descriptor",
            )
        })?;
        if device_descriptor.len() != 18 || device_descriptor[0] != 18 || device_descriptor[1] != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid USB device descriptor",
            ));
        }
        if configuration_descriptor.len() < 9
            || configuration_descriptor[0] != 9
            || configuration_descriptor[1] != 2
            || usize::from(u16::from_le_bytes([
                configuration_descriptor[2],
                configuration_descriptor[3],
            ])) != configuration_descriptor.len()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid USB configuration descriptor",
            ));
        }
        Ok(UsbPersonality {
            schema: PERSONALITY_SCHEMA,
            max_speed: self.max_speed,
            device_descriptor,
            configuration_descriptor,
            strings: self.strings,
            microsoft_os_1: self.microsoft_os_1,
            webusb: self.webusb,
        })
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

#[repr(C)]
pub struct UgspBytes {
    pub data: *const u8,
    pub length: usize,
}

#[repr(C)]
pub struct UgspUsbDevice {
    pub usb_version: u16,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_version: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub max_packet_size_0: u8,
    pub manufacturer: *const c_char,
    pub product: *const c_char,
    pub serial_number: *const c_char,
    pub interface_name: *const c_char,
}

#[repr(C)]
pub struct UgspUsbInterface {
    pub number: u8,
    pub class_code: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub string_index: u8,
    pub endpoint_in: u8,
    pub endpoint_out: u8,
    pub transfer_type: u8,
    pub max_packet_size: u16,
    pub interval: u8,
    pub class_descriptors: UgspBytes,
}

#[derive(Clone, Debug)]
struct NativeDevice {
    usb_version: u16,
    vendor_id: u16,
    product_id: u16,
    device_version: u16,
    device_class: u8,
    device_subclass: u8,
    device_protocol: u8,
    max_packet_size_0: u8,
    manufacturer: String,
    product: String,
    serial_number: String,
    interface_name: String,
}

#[derive(Clone, Debug)]
struct NativeInterface {
    number: u8,
    class_code: u8,
    subclass: u8,
    protocol: u8,
    string_index: u8,
    endpoint_in: u8,
    endpoint_out: u8,
    transfer_type: u8,
    max_packet_size: u16,
    interval: u8,
    class_descriptors: Vec<u8>,
}

/// Opaque C handle which feeds native platform configuration into the same
/// UsbPersonalityBuilder used by descriptor discovery.
pub struct UgspPersonalityBuilder {
    max_speed: UsbSpeed,
    device: NativeDevice,
    interfaces: Vec<NativeInterface>,
    microsoft_os_1: Option<MicrosoftOs10>,
    webusb: Option<WebUsb>,
}

unsafe fn ffi_bytes(value: &UgspBytes) -> io::Result<Vec<u8>> {
    if value.length != 0 && value.data.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "null byte slice",
        ));
    }
    Ok(if value.length == 0 {
        Vec::new()
    } else {
        unsafe { slice::from_raw_parts(value.data, value.length) }.to_vec()
    })
}

unsafe fn ffi_string(value: *const c_char) -> io::Result<String> {
    if value.is_null() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "null string"));
    }
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .map(str::to_owned)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 string"))
}

fn ffi_speed(speed: u8) -> io::Result<UsbSpeed> {
    match speed {
        0 => Ok(UsbSpeed::LowSpeed),
        1 => Ok(UsbSpeed::FullSpeed),
        2 => Ok(UsbSpeed::HighSpeed),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid USB speed",
        )),
    }
}

fn string_descriptor(index: u8, language_id: u16, value: &str) -> io::Result<StringDescriptor> {
    let mut descriptor = Vec::with_capacity(2 + value.len() * 2);
    descriptor.extend_from_slice(&[0, 3]);
    for word in value.encode_utf16() {
        descriptor.extend_from_slice(&word.to_le_bytes());
    }
    if descriptor.len() > u8::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "USB string descriptor is too long",
        ));
    }
    descriptor[0] = descriptor.len() as u8;
    Ok(StringDescriptor::new(index, language_id, descriptor))
}

impl UgspPersonalityBuilder {
    fn personality(&self) -> io::Result<UsbPersonality> {
        if self.interfaces.is_empty() || self.interfaces.len() > u8::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "USB personality has no interfaces",
            ));
        }
        let mut device = vec![
            18,
            1,
            0,
            0,
            self.device.device_class,
            self.device.device_subclass,
            self.device.device_protocol,
            self.device.max_packet_size_0,
            0,
            0,
            0,
            0,
            0,
            0,
            1,
            2,
            3,
            1,
        ];
        device[2..4].copy_from_slice(&self.device.usb_version.to_le_bytes());
        device[8..10].copy_from_slice(&self.device.vendor_id.to_le_bytes());
        device[10..12].copy_from_slice(&self.device.product_id.to_le_bytes());
        device[12..14].copy_from_slice(&self.device.device_version.to_le_bytes());

        let mut interfaces = self.interfaces.clone();
        interfaces.sort_by_key(|interface| interface.number);
        for pair in interfaces.windows(2) {
            if pair[0].number == pair[1].number {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "duplicate USB interface number",
                ));
            }
        }
        let mut configuration = vec![9, 2, 0, 0, interfaces.len() as u8, 1, 0, 0x80, 0x32];
        for interface in &interfaces {
            if interface.endpoint_in & 0x80 == 0
                || interface.endpoint_out & 0x80 != 0
                || interface.max_packet_size == 0
                || interface.transfer_type > 3
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid native USB interface",
                ));
            }
            configuration.extend_from_slice(&[
                9,
                4,
                interface.number,
                0,
                2,
                interface.class_code,
                interface.subclass,
                interface.protocol,
                interface.string_index,
            ]);
            configuration.extend_from_slice(&interface.class_descriptors);
            for address in [interface.endpoint_in, interface.endpoint_out] {
                configuration.extend_from_slice(&[
                    7,
                    5,
                    address,
                    interface.transfer_type,
                    self::low_byte(interface.max_packet_size),
                    self::high_byte(interface.max_packet_size),
                    interface.interval,
                ]);
            }
        }
        let total_length = u16::try_from(configuration.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "USB configuration descriptor is too large",
            )
        })?;
        configuration[2..4].copy_from_slice(&total_length.to_le_bytes());

        let mut builder = UsbPersonalityBuilder::new(self.max_speed);
        builder
            .device_descriptor(device)
            .configuration_descriptor(configuration)
            .string_descriptor(StringDescriptor::new(0, 0, vec![4, 3, 0x09, 0x04]))
            .string_descriptor(string_descriptor(1, 0x0409, &self.device.manufacturer)?)
            .string_descriptor(string_descriptor(2, 0x0409, &self.device.product)?)
            .string_descriptor(string_descriptor(3, 0x0409, &self.device.serial_number)?)
            .string_descriptor(string_descriptor(4, 0x0409, &self.device.interface_name)?);
        if let Some(microsoft) = self.microsoft_os_1.clone() {
            builder.microsoft_os_1(microsoft);
        }
        if let Some(webusb) = self.webusb.clone() {
            builder.webusb(webusb);
        }
        builder.finish()
    }
}

const fn low_byte(value: u16) -> u8 {
    value as u8
}

const fn high_byte(value: u16) -> u8 {
    (value >> 8) as u8
}

#[no_mangle]
/// Creates a native USB personality builder.
///
/// # Safety
///
/// `device` must point to a readable `UgspUsbDevice`. Every string pointer in
/// that structure must name a valid NUL-terminated C string for the duration
/// of this call.
pub unsafe extern "C" fn ugsp_personality_builder_new(
    speed: u8,
    device: *const UgspUsbDevice,
) -> *mut UgspPersonalityBuilder {
    let result = catch_unwind(AssertUnwindSafe(
        || -> io::Result<UgspPersonalityBuilder> {
            let device = unsafe { device.as_ref() }
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "null USB device"))?;
            Ok(UgspPersonalityBuilder {
                max_speed: ffi_speed(speed)?,
                device: NativeDevice {
                    usb_version: device.usb_version,
                    vendor_id: device.vendor_id,
                    product_id: device.product_id,
                    device_version: device.device_version,
                    device_class: device.device_class,
                    device_subclass: device.device_subclass,
                    device_protocol: device.device_protocol,
                    max_packet_size_0: device.max_packet_size_0,
                    manufacturer: unsafe { ffi_string(device.manufacturer)? },
                    product: unsafe { ffi_string(device.product)? },
                    serial_number: unsafe { ffi_string(device.serial_number)? },
                    interface_name: unsafe { ffi_string(device.interface_name)? },
                },
                interfaces: Vec::new(),
                microsoft_os_1: None,
                webusb: None,
            })
        },
    ));
    match result {
        Ok(Ok(builder)) => Box::into_raw(Box::new(builder)),
        _ => ptr::null_mut(),
    }
}

#[no_mangle]
/// Adds an interface to a native USB personality builder.
///
/// # Safety
///
/// `builder` must be a live pointer returned by
/// `ugsp_personality_builder_new`. `interface` must point to a readable
/// `UgspUsbInterface`; its class-descriptor pointer must be readable for its
/// declared length for the duration of this call.
pub unsafe extern "C" fn ugsp_personality_builder_add_interface(
    builder: *mut UgspPersonalityBuilder,
    interface: *const UgspUsbInterface,
) -> bool {
    let result = catch_unwind(AssertUnwindSafe(|| -> io::Result<()> {
        let builder = unsafe { builder.as_mut() }
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "null builder"))?;
        let interface = unsafe { interface.as_ref() }
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "null interface"))?;
        if builder
            .interfaces
            .iter()
            .any(|existing| existing.number == interface.number)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "duplicate USB interface",
            ));
        }
        builder.interfaces.push(NativeInterface {
            number: interface.number,
            class_code: interface.class_code,
            subclass: interface.subclass,
            protocol: interface.protocol,
            string_index: interface.string_index,
            endpoint_in: interface.endpoint_in,
            endpoint_out: interface.endpoint_out,
            transfer_type: interface.transfer_type,
            max_packet_size: interface.max_packet_size,
            interval: interface.interval,
            class_descriptors: unsafe { ffi_bytes(&interface.class_descriptors)? },
        });
        Ok(())
    }));
    matches!(result, Ok(Ok(())))
}

#[no_mangle]
/// Replaces the builder's serial number.
///
/// # Safety
///
/// `builder` must be a live pointer returned by
/// `ugsp_personality_builder_new`, and `serial_number` must name a valid
/// NUL-terminated C string for the duration of this call.
pub unsafe extern "C" fn ugsp_personality_builder_set_serial_number(
    builder: *mut UgspPersonalityBuilder,
    serial_number: *const c_char,
) -> bool {
    let result = catch_unwind(AssertUnwindSafe(|| -> io::Result<()> {
        let builder = unsafe { builder.as_mut() }
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "null builder"))?;
        builder.device.serial_number = unsafe { ffi_string(serial_number)? };
        Ok(())
    }));
    matches!(result, Ok(Ok(())))
}

#[no_mangle]
/// Adds a Microsoft OS 1.0 compatible-ID declaration.
///
/// # Safety
///
/// `builder` must be a live pointer returned by
/// `ugsp_personality_builder_new`. Both string pointers must name valid
/// NUL-terminated C strings for the duration of this call.
pub unsafe extern "C" fn ugsp_personality_builder_add_microsoft_compatible_id(
    builder: *mut UgspPersonalityBuilder,
    vendor_code: u8,
    interface: u8,
    compatible_id: *const c_char,
    sub_compatible_id: *const c_char,
) -> bool {
    let result = catch_unwind(AssertUnwindSafe(|| -> io::Result<()> {
        let builder = unsafe { builder.as_mut() }
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "null builder"))?;
        if vendor_code == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero Microsoft OS vendor code",
            ));
        }
        let microsoft = builder
            .microsoft_os_1
            .get_or_insert_with(|| MicrosoftOs10::new(vendor_code));
        if microsoft.vendor_code != vendor_code {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "conflicting Microsoft OS vendor codes",
            ));
        }
        microsoft.compatible_ids.push(MicrosoftCompatibleId::new(
            interface,
            unsafe { ffi_string(compatible_id)? },
            unsafe { ffi_string(sub_compatible_id)? },
        ));
        Ok(())
    }));
    matches!(result, Ok(Ok(())))
}

#[no_mangle]
/// Sets or clears the builder's WebUSB platform capability.
///
/// # Safety
///
/// `builder` must be a live pointer returned by
/// `ugsp_personality_builder_new`, and `landing_page` must name a valid
/// NUL-terminated C string for the duration of this call.
pub unsafe extern "C" fn ugsp_personality_builder_set_webusb(
    builder: *mut UgspPersonalityBuilder,
    enabled: u8,
    version: u16,
    vendor_code: u8,
    landing_page: *const c_char,
) -> bool {
    let result = catch_unwind(AssertUnwindSafe(|| -> io::Result<()> {
        let builder = unsafe { builder.as_mut() }
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "null builder"))?;
        builder.webusb = if enabled == 0 {
            None
        } else {
            Some(WebUsb::new(version, vendor_code, unsafe {
                ffi_string(landing_page)?
            }))
        };
        Ok(())
    }));
    matches!(result, Ok(Ok(())))
}

#[no_mangle]
/// Validates and serializes a native USB personality builder.
///
/// # Safety
///
/// `builder` must be a live pointer returned by
/// `ugsp_personality_builder_new`. `output` and `output_length` must be valid,
/// writable pointers. On success, release the returned buffer with
/// `ugsp_personality_cbor_free`.
pub unsafe extern "C" fn ugsp_personality_builder_finish(
    builder: *const UgspPersonalityBuilder,
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
    let result = catch_unwind(AssertUnwindSafe(|| -> io::Result<Vec<u8>> {
        let builder = unsafe { builder.as_ref() }
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "null builder"))?;
        builder.personality()?.to_cbor()
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
/// Releases a native USB personality builder.
///
/// # Safety
///
/// `builder` must be null or a live pointer returned by
/// `ugsp_personality_builder_new` that has not previously been freed.
pub unsafe extern "C" fn ugsp_personality_builder_free(builder: *mut UgspPersonalityBuilder) {
    if !builder.is_null() {
        drop(unsafe { Box::from_raw(builder) });
    }
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
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn endpoint_lifecycle_waits_for_new_activation_and_stops() {
        let lifecycle = Arc::new(EndpointLifecycle::new());
        let (sender, receiver) = mpsc::channel();
        let waiter = {
            let lifecycle = Arc::clone(&lifecycle);
            thread::spawn(move || {
                sender.send(lifecycle.wait_for_activation_after(0)).unwrap();
                sender.send(lifecycle.wait_for_activation_after(1)).unwrap();
            })
        };

        assert!(receiver.recv_timeout(Duration::from_millis(20)).is_err());
        lifecycle.activate(1);
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            Some(1)
        );
        lifecycle.stop();
        assert_eq!(receiver.recv_timeout(Duration::from_secs(1)).unwrap(), None);
        waiter.join().unwrap();
    }

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

    #[test]
    fn bus_events_carry_activation() {
        let event = UsbBusEvent::Enable.encode(0x0102_0304_0506_0708);
        assert_eq!(
            UsbBusEvent::decode(&event).unwrap(),
            (UsbBusEvent::Enable, 0x0102_0304_0506_0708)
        );
    }

    #[test]
    fn native_builder_ffi_encodes_typed_personality() {
        use std::ffi::CString;

        let manufacturer = CString::new("Trezor Company").unwrap();
        let product = CString::new("Trezor Safe 3").unwrap();
        let serial = CString::new("serial").unwrap();
        let interface_name = CString::new("TREZOR Interface").unwrap();
        let device = UgspUsbDevice {
            usb_version: 0x0210,
            vendor_id: 0x1209,
            product_id: 0x53c1,
            device_version: 0x0200,
            device_class: 0,
            device_subclass: 0,
            device_protocol: 0,
            max_packet_size_0: 64,
            manufacturer: manufacturer.as_ptr(),
            product: product.as_ptr(),
            serial_number: serial.as_ptr(),
            interface_name: interface_name.as_ptr(),
        };
        let interface = UgspUsbInterface {
            number: 0,
            class_code: 0xff,
            subclass: 0,
            protocol: 0,
            string_index: 4,
            endpoint_in: 0x81,
            endpoint_out: 0x01,
            transfer_type: 3,
            max_packet_size: 64,
            interval: 1,
            class_descriptors: UgspBytes {
                data: ptr::null(),
                length: 0,
            },
        };
        let builder = unsafe { ugsp_personality_builder_new(1, &device) };
        assert!(!builder.is_null());
        assert!(unsafe { ugsp_personality_builder_add_interface(builder, &interface) });
        let mut output = ptr::null_mut();
        let mut output_length = 0;
        assert!(unsafe {
            ugsp_personality_builder_finish(builder, &mut output, &mut output_length)
        });
        let encoded = unsafe { slice::from_raw_parts(output, output_length) };
        let decoded = UsbPersonality::from_cbor(encoded).unwrap();
        assert_eq!(&decoded.device_descriptor[8..12], &[0x09, 0x12, 0xc1, 0x53]);
        assert_eq!(decoded.configuration_descriptor[4], 1);
        assert_eq!(decoded.strings[2].descriptor[2], b'T');
        unsafe { crate::discovery::ugsp_personality_cbor_free(output, output_length) };
        unsafe { ugsp_personality_builder_free(builder) };
    }
}
