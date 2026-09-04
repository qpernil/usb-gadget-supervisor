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
    pub(crate) functionfs_mount: PathBuf,
    pub(crate) worker: WorkerProfile,
    #[serde(default)]
    pub(crate) resources: Vec<ResourceProfile>,
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
pub(crate) struct WorkerProfile {
    pub(crate) command: PathBuf,
    #[serde(default)]
    pub(crate) arguments: Vec<String>,
    pub(crate) run_as: String,
    pub(crate) readiness_timeout_ms: u64,
    pub(crate) state_directory: PathBuf,
    pub(crate) runtime_directory: PathBuf,
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
        validate_absolute("functionfs_mount", &self.functionfs_mount)?;
        let mount_name = self
            .functionfs_mount
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if self.functionfs_mount.parent() != Some(Path::new("/dev"))
            || !mount_name.starts_with("ffs-")
            || mount_name.len() == 4
        {
            return invalid("functionfs_mount must use the /dev/ffs-* namespace");
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

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
schema = 1
name = "test-device"
functionfs_mount = "/dev/ffs-test-device"

[worker]
command = "/usr/libexec/test-worker"
arguments = ["--serial", "1"]
run_as = "device-worker"
readiness_timeout_ms = 10000
state_directory = "/var/lib/test-device"
runtime_directory = "/run/test-device"
"#;

    #[test]
    fn parses_a_descriptor_free_profile() {
        let profile: Profile = toml::from_str(VALID).unwrap();
        profile.validate().unwrap();
        assert_eq!(profile.functionfs_mount, Path::new("/dev/ffs-test-device"));
    }

    #[test]
    fn rejects_unknown_and_legacy_usb_fields() {
        assert!(toml::from_str::<Profile>(&format!("{VALID}\n[usb]\nvendor_id = 1\n")).is_err());
    }

    #[test]
    fn rejects_a_root_worker() {
        let profile: Profile = toml::from_str(&VALID.replace("device-worker", "root")).unwrap();
        assert!(profile.validate().is_err());
    }

    #[test]
    fn validates_gpio_ownership_and_initial_values() {
        let valid = format!(
            "{VALID}\n[[resources]]\ntype = \"gpio-lines\"\nname = \"buttons\"\npath = \"/dev/gpiochip0\"\noffsets = [5, 26, 13]\ndirection = \"input\"\nactive_low = true\nbias = \"pull-up\"\nedge = \"both\"\n"
        );
        toml::from_str::<Profile>(&valid)
            .unwrap()
            .validate()
            .unwrap();

        let invalid = format!(
            "{VALID}\n[[resources]]\ntype = \"gpio-lines\"\nname = \"display\"\npath = \"/dev/gpiochip0\"\noffsets = [24, 25]\ndirection = \"output\"\ninitial_values = [false]\n"
        );
        assert!(
            toml::from_str::<Profile>(&invalid)
                .unwrap()
                .validate()
                .is_err()
        );
    }
}
