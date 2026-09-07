use std::io;
use std::path::PathBuf;

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Options {
    pub(crate) profile: PathBuf,
    pub(crate) check_profile: bool,
}

pub(crate) fn parse<I>(arguments: I) -> io::Result<Options>
where
    I: IntoIterator<Item = String>,
{
    let mut profile = None;
    let mut check_profile = false;
    let mut arguments = arguments.into_iter();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--profile" => {
                let value = arguments.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--profile needs a path")
                })?;
                profile = Some(PathBuf::from(value));
            }
            "--check-profile" => check_profile = true,
            "--help" | "-h" => {
                println!(
                    "Usage: usb-gadget-supervisor --profile NAME_OR_PATH [--check-profile]\n\
                     \n\
                     Load one root-owned profile, select its USB or inherited-device mode,\n\
                     and run the configured worker as an unprivileged account. Use\n\
                     --check-profile to validate the schema without touching hardware."
                );
                std::process::exit(0);
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument: {argument}"),
                ));
            }
        }
    }

    let profile = profile.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "--profile NAME_OR_PATH is required",
        )
    })?;
    let profile = if profile.is_absolute() {
        profile
    } else {
        let name = profile.to_str().unwrap_or_default();
        if name.is_empty()
            || name == "."
            || name == ".."
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--profile needs an installed profile name or an absolute path",
            ));
        }
        PathBuf::from("/opt/usb-gadget-supervisor/profiles").join(format!("{name}.toml"))
    };
    Ok(Options {
        profile,
        check_profile,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_relative_profile_paths() {
        let error = parse(["--profile".into(), "../relative.toml".into()]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn resolves_an_installed_profile_name() {
        let options = parse(["--profile".into(), "virtual-yubihsm-i2c".into()]).unwrap();
        assert_eq!(
            options.profile,
            PathBuf::from("/opt/usb-gadget-supervisor/profiles/virtual-yubihsm-i2c.toml")
        );
    }

    #[test]
    fn rejects_launch_overrides() {
        let error = parse([
            "--profile".into(),
            "virtual-yubikey".into(),
            "--udc".into(),
            "fe980000.usb".into(),
        ])
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
