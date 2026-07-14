//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! Fastboot-over-TCP transport for `summit-usbgadget`.
//!
//! Mirrors the USB fastboot-usb / fastboot function ([`summit_usbgadget_fastboot_usb`]) but over a
//! TCP socket using the AOSP "TCP Protocol v1" framing: the device listens as a
//! server, both peers exchange a `FB01` handshake, and every fastboot packet is
//! then wrapped as an 8-byte big-endian length prefix followed by the packet
//! bytes. The command vocabulary (getvar / fetch / download / flash / Close) and
//! the SWUpdate feeding are identical to the USB path — only the framing differs.

use std::error::Error;
use std::io;
use std::time::Duration;

use serde::Deserialize;
use summit_usbgadget_config::{PluginConfig, Shutdown};
use summit_usbgadget_swupdate::{SwupdateConfig, SwupdateConfigError, SwupdateParams};
use tokio::net::TcpListener;
use tokio::time::timeout;

mod session;
mod transport;

use session::TcpFastbootSession;
use transport::FastbootFraming;

/// Configuration section read from the shared document.
pub const FASTBOOT_TCP_SECTION: &str = "fastboot_tcp";

/// Fixed capacity used for every download read buffer; also sizes the SWUpdate
/// queue so buffered memory stays bounded regardless of the host's frame sizes.
const RECV_BUFFER_SIZE: usize = 128 * 1024;

/// Serial reported by `getvar:serialno` / sysinfo when none is configured.
const DEFAULT_SERIAL: &str = "unknown";

/// `[fastboot_tcp]` configuration.
///
/// The SWUpdate parameters are flattened in, matching the USB fastboot-usb function so a
/// device can offer the same update over both transports with one schema.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FastbootTcpConfig {
    /// Listen address, e.g. `0.0.0.0:5554` (the fastboot TCP default port).
    pub address: String,
    /// Optional accept timeout; when unset the listener blocks indefinitely.
    pub accept_timeout_secs: Option<u64>,
    /// Idle timeout (seconds) for an established connection; `0` disables it.
    pub inactivity_timeout_secs: u64,
    /// Grace period (seconds) allowed for the connection shutdown handshake.
    pub shutdown_timeout_secs: u64,
    /// Serial number advertised to fastboot hosts.
    pub serial: String,
    /// SWUpdate download parameters shared with the fastboot-usb schema.
    #[serde(flatten)]
    pub swupdate: SwupdateConfig,
}

impl Default for FastbootTcpConfig {
    fn default() -> Self {
        Self {
            address: "0.0.0.0:5554".to_string(),
            accept_timeout_secs: None,
            inactivity_timeout_secs: 30,
            shutdown_timeout_secs: 15,
            serial: DEFAULT_SERIAL.to_string(),
            swupdate: SwupdateConfig::default(),
        }
    }
}

impl PluginConfig for FastbootTcpConfig {
    const SECTION: Option<&'static str> = Some(FASTBOOT_TCP_SECTION);
}

impl FastbootTcpConfig {
    /// Resolves the SWUpdate parameters, validating the download target.
    pub fn to_swupdate_params(&self) -> Result<SwupdateParams, SwupdateConfigError> {
        self.swupdate.to_params()
    }

    fn accept_timeout(&self) -> Option<Duration> {
        self.accept_timeout_secs.map(Duration::from_secs)
    }

    fn inactivity_timeout(&self) -> Option<Duration> {
        match self.inactivity_timeout_secs {
            0 => None,
            secs => Some(Duration::from_secs(secs)),
        }
    }

    fn shutdown_timeout(&self) -> Duration {
        Duration::from_secs(self.shutdown_timeout_secs)
    }
}

/// Runs the fastboot-over-TCP service: loads `[fastboot_tcp]` from `config_path`
/// and serves fastboot clients into SWUpdate until `shutdown` is requested.
pub async fn run(
    config_path: impl AsRef<std::path::Path>,
    shutdown: Shutdown,
) -> Result<(), Box<dyn Error>> {
    let config = FastbootTcpConfig::load(config_path)?;
    let params = config.to_swupdate_params()?;
    tokio::select! {
        result = serve(&config, params) => result?,
        _ = shutdown.wait() => {}
    }
    Ok(())
}

// Register fastboot-over-TCP as a top-level startup service.
summit_usbgadget_config::declare_service!("fastboot-tcp" => run);

async fn serve(config: &FastbootTcpConfig, params: SwupdateParams) -> io::Result<()> {
    let accept_timeout = config.accept_timeout();
    let inactivity_timeout = config.inactivity_timeout();

    loop {
        // Listen, accept exactly one client, then drop the listener so the port
        // is closed while we serve. Any other client is refused by the kernel
        // (connection refused) — one update at a time, no bookkeeping. Re-bind
        // for the next client once the session ends. Matches the socket-update
        // transport's own listener loop.
        let listener = TcpListener::bind(&config.address).await?;
        log::info!("fastboot-tcp listening on {}", config.address);

        let (stream, peer) = match accept_timeout {
            Some(t) => match timeout(t, listener.accept()).await {
                Ok(res) => res?,
                Err(_) => {
                    log::debug!("fastboot-tcp accept timed out waiting for a client");
                    continue;
                }
            },
            None => listener.accept().await?,
        };
        drop(listener);

        let _ = stream.set_nodelay(true);
        let peer = peer.to_string();
        log::info!("fastboot-tcp connection from {peer}");

        // Fastboot is host-driven and single-session; serve one client at a time
        // so at most one SWUpdate transfer is ever in flight.
        let session = TcpFastbootSession::new(FastbootFraming::new(stream, inactivity_timeout), config, params.clone(), peer.clone());
        if let Err(err) = session.run().await {
            log::error!("fastboot-tcp session {peer} failed: {err}");
        }
    }
}

#[cfg(test)]
mod tests;
