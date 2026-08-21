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
#[cfg(target_os = "linux")]
use std::fs::File;
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

#[cfg(target_os = "linux")]
pub(crate) static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "linux")]
pub(crate) static RESTART_REQUESTED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "linux")]
static SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

#[cfg(target_os = "linux")]
struct SignalWakeup {
    read: File,
    _write: File,
}

#[cfg(target_os = "linux")]
impl SignalWakeup {
    fn read_fd(&self) -> i32 {
        self.read.as_raw_fd()
    }
}

#[cfg(target_os = "linux")]
impl Drop for SignalWakeup {
    fn drop(&mut self) {
        SIGNAL_WRITE_FD.store(-1, Ordering::Relaxed);
    }
}

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
        let signals = install_signal_handlers()?;
        let mut runtime =
            runtime::Runtime::setup(options.profile, profile, options.udc.as_deref())?;
        let serve_result = runtime.serve(signals.read_fd());
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
fn install_signal_handlers() -> io::Result<SignalWakeup> {
    unsafe fn wake_supervisor() {
        let descriptor = SIGNAL_WRITE_FD.load(Ordering::Relaxed);
        if descriptor >= 0 {
            let byte = 1_u8;
            let _ = unsafe { libc::write(descriptor, (&byte as *const u8).cast(), 1) };
        }
    }
    unsafe extern "C" fn stop(_signal: i32) {
        STOP_REQUESTED.store(true, Ordering::Relaxed);
        unsafe { wake_supervisor() };
    }
    unsafe extern "C" fn restart(_signal: i32) {
        RESTART_REQUESTED.store(true, Ordering::Relaxed);
        unsafe { wake_supervisor() };
    }
    unsafe extern "C" fn child_changed(_signal: i32) {
        unsafe { wake_supervisor() };
    }

    let mut descriptors = [-1; 2];
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 returned two new owned descriptors.
    let read = unsafe { File::from_raw_fd(descriptors[0]) };
    // SAFETY: pipe2 returned two new owned descriptors.
    let write = unsafe { File::from_raw_fd(descriptors[1]) };
    SIGNAL_WRITE_FD.store(write.as_raw_fd(), Ordering::Relaxed);

    // SAFETY: all handlers have the C signal-handler ABI. `stop` and `restart`
    // only store to atomics, and each handler writes one byte to a nonblocking
    // pipe using the async-signal-safe write(2) system call.
    if unsafe { libc::signal(libc::SIGINT, stop as *const () as usize) } == libc::SIG_ERR
        || unsafe { libc::signal(libc::SIGTERM, stop as *const () as usize) } == libc::SIG_ERR
        || unsafe { libc::signal(libc::SIGHUP, restart as *const () as usize) } == libc::SIG_ERR
        || unsafe { libc::signal(libc::SIGCHLD, child_changed as *const () as usize) }
            == libc::SIG_ERR
    {
        SIGNAL_WRITE_FD.store(-1, Ordering::Relaxed);
        return Err(io::Error::last_os_error());
    }
    Ok(SignalWakeup {
        read,
        _write: write,
    })
}
