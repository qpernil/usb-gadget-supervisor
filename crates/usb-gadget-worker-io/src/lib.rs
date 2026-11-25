//! Compatible worker APIs selected at build time.
#![cfg(unix)]

mod wire;
pub use wire::{
    HandleKind, MAX_TRANSFER_LENGTH, event, initialize, receive_transfer, send_transfer,
};

/// The actual Linux APIs; production worker builds use these directly.
#[cfg(target_os = "linux")]
pub mod linux {
    pub use display_backends as display;
    pub use gpiocdev_uapi as gpio;
    pub use std::fs::File;
}

#[cfg(any(not(target_os = "linux"), feature = "simulated", test))]
pub mod simulated;

#[cfg(all(target_os = "linux", not(feature = "simulated")))]
pub use linux::{File, display, gpio};
#[cfg(any(not(target_os = "linux"), feature = "simulated"))]
pub use simulated::{File, display, gpio};

#[cfg(test)]
mod tests;
