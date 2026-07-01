//
// SPDX-License-Identifier: MIT OR Apache-2.0
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

use summit_usbgadget::config::GadgetConfig;
use summit_usbgadget::gadget;

/// Default configuration file path when none is given on the command line.
const DEFAULT_CONFIG_PATH: &str = "/etc/summit-usbgadget.toml";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    simple_logger::SimpleLogger::new()
        .with_level(log::LevelFilter::Warn)
        .with_module_level("summit_usbgadget", log::LevelFilter::Info)
        .env()
        .init()
        .unwrap();

    #[cfg(feature = "dfu")]
    summit_usbgadget::sysinfo::init().await;

    let config_path = config_path_from_args();
    log::info!("loading gadget configuration from {}", config_path.display());
    let config = match GadgetConfig::load(&config_path) {
        Ok(config) => config,
        Err(err) => {
            if let Err(reset_err) = gadget::reset() {
                log::warn!("failed to remove gadgets after configuration load error: {reset_err}");
            }
            return Err(err.into());
        }
    };

    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

        gadget::serve_until(config, async {
            tokio::select! {
                _ = sigterm.recv() => {
                    log::info!("received SIGTERM, shutting down gadget service");
                }
                _ = sigint.recv() => {
                    log::info!("received SIGINT, shutting down gadget service");
                }
            }
        }).await
    }

    #[cfg(not(unix))]
    {
        gadget::serve(config).await
    }
}

/// Determines the configuration file path from the command line or environment.
fn config_path_from_args() -> PathBuf {
    if let Some(path) = std::env::args_os().nth(1) {
        return PathBuf::from(path);
    }
    PathBuf::from(DEFAULT_CONFIG_PATH)
}
