//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! Runtime-configurable composite USB gadget.
//!
//! The gadget composition is read from a configuration file (see
//! [`summit_usbgadget::config`]). One gadget is built and bound per selected USB
//! device controller; controllers are discovered through udev events (see
//! [`summit_usbgadget::udc`]), so the UDC driver may be loaded after this service
//! starts and multiple controllers can be served. When a gadget includes a DFU
//! function, a task services its endpoint-zero control requests and streams
//! downloads into SWUpdate (or a file).
//!
//! Root privileges, a mounted `configfs`, udev, and a USB device controller
//! (UDC) are required. See the kernel configuration notes in `README.md`.

use std::path::PathBuf;

/// Default configuration file path when none is given on the command line.
const DEFAULT_CONFIG_PATH: &str = "/etc/summit-usbgadget.toml";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `with_module_level` always wins over `.env()`'s RUST_LOG-derived default
    // for any module it names (see simple_logger::Log::enabled: the longest
    // matching module_levels entry is checked before falling back to the
    // default level). Since "summit_usbgadget" prefix-matches every crate in
    // this workspace, hard-coding its level here would make RUST_LOG unable to
    // raise verbosity for our own code at all (third-party crates like
    // usb_gadget would respect RUST_LOG, ours silently would not). Read
    // RUST_LOG ourselves so it can override the default Info level instead.
    let summit_level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|level| level.parse::<log::LevelFilter>().ok())
        .unwrap_or(log::LevelFilter::Info);

    simple_logger::SimpleLogger::new()
        .with_level(log::LevelFilter::Warn)
        .with_module_level("summit_usbgadget", summit_level)
        .env()
        .init()
        .unwrap();

    // Populate the boot-info cache once, up front, while this is still the
    // only task running. The first call runs `boot-rootfs.sh` via a blocking
    // subprocess; every service below shares one single-threaded runtime, so
    // calling this lazily from within a service would stall all of them.
    #[cfg(any(feature = "dfu", feature = "fastboot-usb", feature = "fastboot-tcp", feature = "socket"))]
    let _ = summit_usbgadget_swupdate::sysinfo::boot_info();

    // Start every compiled-in plugin's registered service. Each plugin reads
    // its own section from the shared configuration file and runs itself.
    summit_usbgadget::config::run_services(config_path_from_args()).await
}

/// Determines the configuration file path from the command line or environment.
fn config_path_from_args() -> PathBuf {
    if let Some(path) = std::env::args_os().nth(1) {
        return PathBuf::from(path);
    }
    PathBuf::from(DEFAULT_CONFIG_PATH)
}
