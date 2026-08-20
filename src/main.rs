//! Generic privileged lifecycle supervisor for Linux USB gadget workers.

mod cli;
mod functionfs;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod profile;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod protocol;
#[cfg(target_os = "linux")]
mod runtime;

use std::env;
use std::io;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "linux")]
pub(crate) static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

fn main() {
    if let Err(error) = run() {
        eprintln!("usb-gadget-supervisor: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let options = cli::parse(env::args().skip(1))?;
    let profile = profile::Profile::load(&options.profile)?;
    if options.check_profile {
        println!("profile {} is valid", profile.name);
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        install_signal_handlers()?;
        let mut runtime =
            runtime::Runtime::setup(options.profile, profile, options.udc.as_deref())?;
        let serve_result = runtime.serve();
        let cleanup_result = runtime.cleanup();
        serve_result.and(cleanup_result)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (options, profile);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "USB gadget supervision is Linux-only",
        ))
    }
}

#[cfg(target_os = "linux")]
fn install_signal_handlers() -> io::Result<()> {
    unsafe extern "C" fn stop(_signal: i32) {
        STOP_REQUESTED.store(true, Ordering::Relaxed);
    }

    // SAFETY: `stop` has the C signal-handler ABI and only stores to an atomic.
    if unsafe { libc::signal(libc::SIGINT, stop as *const () as usize) } == libc::SIG_ERR
        || unsafe { libc::signal(libc::SIGTERM, stop as *const () as usize) } == libc::SIG_ERR
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
