use crate::functionfs;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Profile {
    pub(crate) schema: u32,
    pub(crate) name: String,
    pub(crate) usb: UsbProfile,
    pub(crate) worker: WorkerProfile,
    #[serde(default)]
    pub(crate) resources: Vec<ResourceProfile>,
    pub(crate) functions: Vec<FunctionProfile>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum ResourceProfile {
    CharacterDevice(CharacterDeviceResource),
    GpioLines(GpioLinesResource),
}

impl ResourceProfile {
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::CharacterDevice(resource) => &resource.name,
            Self::GpioLines(resource) => &resource.name,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::CharacterDevice(resource) => &resource.path,
            Self::GpioLines(resource) => &resource.path,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CharacterDeviceResource {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) access: ResourceAccess,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GpioLinesResource {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) offsets: Vec<u32>,
    pub(crate) direction: GpioDirection,
    #[serde(default)]
    pub(crate) active_low: bool,
    pub(crate) bias: Option<GpioBias>,
    pub(crate) edge: Option<GpioEdge>,
    pub(crate) initial_values: Option<Vec<bool>>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ResourceAccess {
    Read,
    Write,
    ReadWrite,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum GpioDirection {
    Input,
    Output,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum GpioBias {
    PullUp,
    PullDown,
    Disabled,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum GpioEdge {
    Rising,
    Falling,
    Both,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UsbProfile {
    pub(crate) vendor_id: u16,
    pub(crate) product_id: u16,
    pub(crate) bcd_usb: u16,
    pub(crate) bcd_device: u16,
    pub(crate) max_speed: String,
    pub(crate) device_class: u8,
    pub(crate) device_subclass: u8,
    pub(crate) device_protocol: u8,
    pub(crate) manufacturer: String,
    pub(crate) product: String,
    pub(crate) serial: Option<String>,
    pub(crate) max_power_ma: u16,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerProfile {
    pub(crate) command: PathBuf,
    #[serde(default)]
    pub(crate) arguments: Vec<String>,
    pub(crate) run_as: String,
    pub(crate) readiness_timeout_ms: u64,
    pub(crate) state_directory: PathBuf,
    pub(crate) runtime_directory: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum FunctionProfile {
    Hid(HidFunction),
    Functionfs(FunctionFsFunction),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HidFunction {
    pub(crate) name: String,
    pub(crate) protocol: u8,
    pub(crate) subclass: u8,
    pub(crate) report_length: u16,
    pub(crate) report_descriptor: Option<PathBuf>,
    pub(crate) report_descriptor_hex: Option<String>,
    pub(crate) device: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FunctionFsFunction {
    pub(crate) name: String,
    pub(crate) mount: PathBuf,
    pub(crate) descriptors_hex: String,
    pub(crate) strings_hex: String,
}

impl Profile {
    pub(crate) fn load(path: &Path) -> io::Result<Self> {
        let source = fs::read_to_string(path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("read profile {}: {error}", path.display()),
            )
        })?;
        let profile: Self = toml::from_str(&source).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("parse profile {}: {error}", path.display()),
            )
        })?;
        profile.validate()?;
        Ok(profile)
    }

    fn validate(&self) -> io::Result<()> {
        if self.schema != 1 {
            return invalid(format!("unsupported profile schema {}", self.schema));
        }
        validate_name("profile", &self.name)?;
        if !matches!(
            self.usb.max_speed.as_str(),
            "low-speed" | "full-speed" | "high-speed"
        ) {
            return invalid("usb.max_speed must be low-speed, full-speed, or high-speed");
        }
        if self.usb.manufacturer.is_empty() || self.usb.product.is_empty() {
            return invalid("USB manufacturer and product strings must not be empty");
        }
        if self.usb.max_power_ma == 0 || self.usb.max_power_ma > 500 {
            return invalid("usb.max_power_ma must be between 1 and 500");
        }
        if let Some(serial) = &self.usb.serial {
            if serial.is_empty() || serial.len() > 126 {
                return invalid("usb.serial must contain 1 to 126 bytes when present");
            }
        }
        validate_absolute("worker.command", &self.worker.command)?;
        validate_directory_under(
            "worker.state_directory",
            &self.worker.state_directory,
            Path::new("/var/lib"),
        )?;
        validate_directory_under(
            "worker.runtime_directory",
            &self.worker.runtime_directory,
            Path::new("/run"),
        )?;
        validate_name("worker.run_as", &self.worker.run_as)?;
        if self.worker.run_as == "root" {
            return invalid("worker.run_as must not be root");
        }
        if self.worker.readiness_timeout_ms == 0 || self.worker.readiness_timeout_ms > 120_000 {
            return invalid("worker.readiness_timeout_ms must be between 1 and 120000");
        }
        if self.functions.is_empty() {
            return invalid("the profile must declare at least one function");
        }

        let mut resource_names = HashSet::new();
        let mut character_device_paths = HashSet::new();
        let mut gpio_lines = HashSet::new();
        for resource in &self.resources {
            validate_name("resource", resource.name())?;
            if !resource_names.insert(resource.name()) {
                return invalid(format!("duplicate resource name {:?}", resource.name()));
            }
            validate_absolute("resource path", resource.path())?;
            if resource.path() == Path::new("/dev") || !resource.path().starts_with("/dev") {
                return invalid("resource paths must be strict children of /dev");
            }
            match resource {
                ResourceProfile::CharacterDevice(resource) => {
                    if is_gpio_chip_path(&resource.path) {
                        return invalid(format!(
                            "GPIO chip {} must be declared as a gpio-lines resource",
                            resource.path.display()
                        ));
                    }
                    if !character_device_paths.insert(resource.path.as_path()) {
                        return invalid(format!(
                            "duplicate character-device resource path {}",
                            resource.path.display()
                        ));
                    }
                }
                ResourceProfile::GpioLines(resource) => {
                    if !is_gpio_chip_path(&resource.path) {
                        return invalid(
                            "GPIO line resources must use the /dev/gpiochipN namespace",
                        );
                    }
                    if resource.offsets.is_empty() || resource.offsets.len() > 64 {
                        return invalid("GPIO line groups must contain 1 to 64 offsets");
                    }
                    let mut offsets = HashSet::new();
                    for offset in &resource.offsets {
                        if !offsets.insert(*offset) {
                            return invalid(format!(
                                "GPIO resource {:?} contains duplicate offset {offset}",
                                resource.name
                            ));
                        }
                        if !gpio_lines.insert((resource.path.as_path(), *offset)) {
                            return invalid(format!(
                                "GPIO line {}:{} is claimed by more than one resource",
                                resource.path.display(),
                                offset
                            ));
                        }
                    }
                    match resource.direction {
                        GpioDirection::Input => {
                            if resource.initial_values.is_some() {
                                return invalid("input GPIO line groups cannot set initial_values");
                            }
                        }
                        GpioDirection::Output => {
                            if resource.bias.is_some() || resource.edge.is_some() {
                                return invalid("output GPIO line groups cannot set bias or edge");
                            }
                            match &resource.initial_values {
                                Some(values) if values.len() == resource.offsets.len() => {}
                                Some(_) => {
                                    return invalid(
                                        "GPIO output initial_values must match offsets in length",
                                    );
                                }
                                None => {
                                    return invalid(
                                        "GPIO output line groups must set initial_values",
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        let mut names = HashSet::new();
        let mut mounts = HashSet::new();
        let mut devices = HashSet::new();
        for function in &self.functions {
            match function {
                FunctionProfile::Hid(hid) => {
                    validate_name("HID function", &hid.name)?;
                    if !names.insert(hid.name.as_str()) {
                        return invalid(format!("duplicate function name {:?}", hid.name));
                    }
                    if hid.report_length == 0 || hid.report_length > 4096 {
                        return invalid("HID report_length must be between 1 and 4096");
                    }
                    match (&hid.report_descriptor, &hid.report_descriptor_hex) {
                        (Some(path), None) => {
                            validate_absolute("HID report_descriptor", path)?;
                        }
                        (None, Some(descriptor)) => {
                            decode_hex_descriptor(descriptor, "inline HID report descriptor")?;
                        }
                        (None, None) => {
                            return invalid(
                                "HID functions need report_descriptor or report_descriptor_hex",
                            );
                        }
                        (Some(_), Some(_)) => {
                            return invalid(
                                "HID functions must not set both report_descriptor and report_descriptor_hex",
                            );
                        }
                    }
                    validate_absolute("HID device", &hid.device)?;
                    let device_name = hid
                        .device
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_default();
                    if hid.device.parent() != Some(Path::new("/dev"))
                        || !device_name.starts_with("hidg")
                        || device_name[4..].is_empty()
                        || !device_name[4..].bytes().all(|byte| byte.is_ascii_digit())
                    {
                        return invalid("HID device paths must use the /dev/hidg* namespace");
                    }
                    if !devices.insert(hid.device.as_path()) {
                        return invalid(format!("duplicate HID device {}", hid.device.display()));
                    }
                }
                FunctionProfile::Functionfs(ffs) => {
                    validate_name("FunctionFS function", &ffs.name)?;
                    if !names.insert(ffs.name.as_str()) {
                        return invalid(format!("duplicate function name {:?}", ffs.name));
                    }
                    validate_absolute("FunctionFS mount", &ffs.mount)?;
                    let mount_name = ffs
                        .mount
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_default();
                    if ffs.mount.parent() != Some(Path::new("/dev"))
                        || !mount_name.starts_with("ffs-")
                        || mount_name.len() == 4
                    {
                        return invalid("FunctionFS mounts must use the /dev/ffs-* namespace");
                    }
                    if !mounts.insert(ffs.mount.as_path()) {
                        return invalid(format!(
                            "duplicate FunctionFS mount {}",
                            ffs.mount.display()
                        ));
                    }
                    let descriptors = decode_hex_blob(
                        &ffs.descriptors_hex,
                        &format!("FunctionFS {} descriptors", ffs.name),
                    )?;
                    let strings = decode_hex_blob(
                        &ffs.strings_hex,
                        &format!("FunctionFS {} strings", ffs.name),
                    )?;
                    functionfs::inspect(&descriptors, &strings)?;
                }
            }
        }
        Ok(())
    }
}

fn validate_name(label: &str, value: &str) -> io::Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
    {
        return invalid(format!("invalid {label} name {value:?}"));
    }
    Ok(())
}

fn is_gpio_chip_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    path.parent() == Some(Path::new("/dev"))
        && name.strip_prefix("gpiochip").is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn validate_absolute(label: &str, path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return invalid(format!(
            "{label} must be an absolute path without traversal"
        ));
    }
    Ok(())
}

fn validate_directory_under(label: &str, path: &Path, root: &Path) -> io::Result<()> {
    validate_absolute(label, path)?;
    if path == root || !path.starts_with(root) {
        return invalid(format!(
            "{label} must be a strict child of {}",
            root.display()
        ));
    }
    Ok(())
}

fn invalid<T>(message: impl Into<String>) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidInput, message.into()))
}

pub(crate) fn decode_hex_blob(source: &str, label: &str) -> io::Result<Vec<u8>> {
    let mut descriptor = Vec::new();
    for token in source.split_whitespace() {
        descriptor.push(u8::from_str_radix(token, 16).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid hexadecimal byte {token:?} in {label}"),
            )
        })?);
    }
    if descriptor.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} is empty"),
        ));
    }
    Ok(descriptor)
}

pub(crate) fn decode_hex_descriptor(source: &str, label: &str) -> io::Result<Vec<u8>> {
    decode_hex_blob(source, label)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
schema = 1
name = "test-device"

[usb]
vendor_id = 0x1209
product_id = 0x0001
bcd_usb = 0x0200
bcd_device = 0x0100
max_speed = "full-speed"
device_class = 0
device_subclass = 0
device_protocol = 0
manufacturer = "Example"
product = "Test Device"
max_power_ma = 50

[worker]
command = "/usr/libexec/test-worker"
arguments = ["--serial", "1"]
run_as = "device-worker"
readiness_timeout_ms = 10000
state_directory = "/var/lib/test-device"
runtime_directory = "/run/test-device"

[[functions]]
type = "functionfs"
name = "main"
mount = "/dev/ffs-test-device"
descriptors_hex = "03 00 00 00 27 00 00 00 01 00 00 00 03 00 00 00 09 04 00 00 02 ff 00 00 00 07 05 01 02 40 00 00 07 05 81 02 40 00 00"
strings_hex = "02 00 00 00 10 00 00 00 00 00 00 00 00 00 00 00"
"#;

    #[test]
    fn strictly_parses_a_valid_profile() {
        let profile: Profile = toml::from_str(VALID).unwrap();
        profile.validate().unwrap();
        assert_eq!(profile.usb.vendor_id, 0x1209);
        assert_eq!(profile.functions.len(), 1);
    }

    #[test]
    fn rejects_unknown_fields() {
        let error = toml::from_str::<Profile>(&format!("{VALID}\nsecret = true\n")).unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn rejects_a_root_worker() {
        let profile: Profile = toml::from_str(&VALID.replace("device-worker", "root")).unwrap();
        assert_eq!(
            profile.validate().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn rejects_broad_worker_directories() {
        let profile: Profile = toml::from_str(&VALID.replace(
            "state_directory = \"/var/lib/test-device\"",
            "state_directory = \"/var/lib\"",
        ))
        .unwrap();
        assert!(profile.validate().is_err());
    }

    #[test]
    fn accepts_distinct_function_names_without_environment_normalization() {
        let source = format!(
            "{VALID}\n[[functions]]\ntype = \"functionfs\"\nname = \"ma-in\"\nmount = \"/dev/ffs-second\"\ndescriptors_hex = \"03 00 00 00 27 00 00 00 01 00 00 00 03 00 00 00 09 04 00 00 02 ff 00 00 00 07 05 01 02 40 00 00 07 05 81 02 40 00 00\"\nstrings_hex = \"02 00 00 00 10 00 00 00 00 00 00 00 00 00 00 00\"\n\n[[functions]]\ntype = \"hid\"\nname = \"ma_in\"\nprotocol = 0\nsubclass = 0\nreport_length = 64\nreport_descriptor = \"/usr/share/test.hex\"\ndevice = \"/dev/hidg0\"\n"
        );
        let profile: Profile = toml::from_str(&source).unwrap();
        profile.validate().unwrap();
    }

    #[test]
    fn accepts_an_inline_hid_report_descriptor() {
        let source = format!(
            "{VALID}\n[[functions]]\ntype = \"hid\"\nname = \"fido\"\nprotocol = 0\nsubclass = 0\nreport_length = 64\nreport_descriptor_hex = \"06 d0 f1 09 01 c0\"\ndevice = \"/dev/hidg0\"\n"
        );
        let profile: Profile = toml::from_str(&source).unwrap();
        profile.validate().unwrap();
    }

    #[test]
    fn rejects_missing_or_ambiguous_hid_report_descriptors() {
        let missing = format!(
            "{VALID}\n[[functions]]\ntype = \"hid\"\nname = \"fido\"\nprotocol = 0\nsubclass = 0\nreport_length = 64\ndevice = \"/dev/hidg0\"\n"
        );
        assert!(toml::from_str::<Profile>(&missing)
            .unwrap()
            .validate()
            .is_err());

        let both = format!(
            "{VALID}\n[[functions]]\ntype = \"hid\"\nname = \"fido\"\nprotocol = 0\nsubclass = 0\nreport_length = 64\nreport_descriptor = \"/usr/share/test.hex\"\nreport_descriptor_hex = \"06 d0 f1 09 01 c0\"\ndevice = \"/dev/hidg0\"\n"
        );
        assert!(toml::from_str::<Profile>(&both)
            .unwrap()
            .validate()
            .is_err());

        let invalid_hex = format!(
            "{VALID}\n[[functions]]\ntype = \"hid\"\nname = \"fido\"\nprotocol = 0\nsubclass = 0\nreport_length = 64\nreport_descriptor_hex = \"not-hex\"\ndevice = \"/dev/hidg0\"\n"
        );
        assert!(toml::from_str::<Profile>(&invalid_hex)
            .unwrap()
            .validate()
            .is_err());
    }

    #[test]
    fn parses_required_device_resources() {
        let source = format!(
            "{VALID}\n[[resources]]\ntype = \"character-device\"\nname = \"display-i2c\"\npath = \"/dev/i2c-1\"\naccess = \"read-write\"\n"
        );
        let profile: Profile = toml::from_str(&source).unwrap();
        profile.validate().unwrap();
        assert_eq!(profile.resources.len(), 1);
    }

    #[test]
    fn rejects_optional_resource_slots() {
        let source = format!(
            "{VALID}\n[[resources]]\ntype = \"character-device\"\nname = \"display-i2c\"\npath = \"/dev/i2c-1\"\naccess = \"read-write\"\noptional = true\n"
        );
        assert!(toml::from_str::<Profile>(&source).is_err());
    }

    #[test]
    fn accepts_disjoint_gpio_groups_on_one_chip() {
        let source = format!(
            "{VALID}\n[[resources]]\ntype = \"gpio-lines\"\nname = \"display-control\"\npath = \"/dev/gpiochip0\"\noffsets = [25, 27, 24]\ndirection = \"output\"\ninitial_values = [false, false, false]\n\n[[resources]]\ntype = \"gpio-lines\"\nname = \"buttons\"\npath = \"/dev/gpiochip0\"\noffsets = [5, 26, 13]\ndirection = \"input\"\nactive_low = true\nbias = \"pull-up\"\nedge = \"both\"\n"
        );
        let profile: Profile = toml::from_str(&source).unwrap();
        profile.validate().unwrap();
    }

    #[test]
    fn rejects_overlapping_gpio_groups() {
        let source = format!(
            "{VALID}\n[[resources]]\ntype = \"gpio-lines\"\nname = \"first\"\npath = \"/dev/gpiochip0\"\noffsets = [5, 26]\ndirection = \"input\"\n\n[[resources]]\ntype = \"gpio-lines\"\nname = \"second\"\npath = \"/dev/gpiochip0\"\noffsets = [26, 13]\ndirection = \"input\"\n"
        );
        assert!(toml::from_str::<Profile>(&source)
            .unwrap()
            .validate()
            .is_err());
    }

    #[test]
    fn requires_one_initial_value_per_output_line() {
        let missing = format!(
            "{VALID}\n[[resources]]\ntype = \"gpio-lines\"\nname = \"display-control\"\npath = \"/dev/gpiochip0\"\noffsets = [24, 25]\ndirection = \"output\"\n"
        );
        assert!(toml::from_str::<Profile>(&missing)
            .unwrap()
            .validate()
            .is_err());

        let wrong_count = format!(
            "{VALID}\n[[resources]]\ntype = \"gpio-lines\"\nname = \"display-control\"\npath = \"/dev/gpiochip0\"\noffsets = [24, 25]\ndirection = \"output\"\ninitial_values = [false]\n"
        );
        assert!(toml::from_str::<Profile>(&wrong_count)
            .unwrap()
            .validate()
            .is_err());
    }

    #[test]
    fn rejects_a_raw_gpio_chip_character_device() {
        let source = format!(
            "{VALID}\n[[resources]]\ntype = \"character-device\"\nname = \"gpio\"\npath = \"/dev/gpiochip0\"\naccess = \"read-write\"\n"
        );
        assert!(toml::from_str::<Profile>(&source)
            .unwrap()
            .validate()
            .is_err());
    }
}
