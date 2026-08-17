use std::io;
use std::path::PathBuf;

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Options {
    pub(crate) profile: PathBuf,
    pub(crate) udc: Option<String>,
    pub(crate) check_profile: bool,
}

pub(crate) fn parse<I>(arguments: I) -> io::Result<Options>
where
    I: IntoIterator<Item = String>,
{
    let mut profile = None;
    let mut udc = None;
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
            "--udc" => {
                let value = arguments.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--udc needs a name")
                })?;
                if value.is_empty()
                    || !value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid UDC name: {value}"),
                    ));
                }
                udc = Some(value);
            }
            "--check-profile" => check_profile = true,
            "--help" | "-h" => {
                println!(
                    "Usage: usb-gadget-supervisor --profile PATH [--udc NAME] [--check-profile]\n\
                     \n\
                     Load one root-owned device profile, create its Linux USB gadget,\n\
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

    let profile = profile
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "--profile PATH is required"))?;
    if !profile.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the profile path must be absolute",
        ));
    }
    Ok(Options {
        profile,
        udc,
        check_profile,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_an_absolute_profile() {
        let error = parse(["--profile".into(), "relative.toml".into()]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn accepts_a_valid_udc_override() {
        assert_eq!(
            parse([
                "--profile".into(),
                "/opt/usb-gadget-supervisor/profiles/yubikey.toml".into(),
                "--udc".into(),
                "fe980000.usb".into(),
            ])
            .unwrap(),
            Options {
                profile: "/opt/usb-gadget-supervisor/profiles/yubikey.toml".into(),
                udc: Some("fe980000.usb".into()),
                check_profile: false,
            }
        );
    }
}
