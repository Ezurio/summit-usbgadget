use std::error::Error;
use std::io;
use std::time::Duration;

use bytes::BytesMut;
use serde::Deserialize;
use summit_usbgadget_config::{PluginConfig, Shutdown};
use summit_usbgadget_swupdate::{Feed, SwupdateConfig, SwupdateConfigError, SwupdateParams, SwupdateSession};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;

#[cfg(feature = "tls")]
mod tls;

pub const SOCKET_SOURCE_SECTION: &str = "socket_source";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SocketSourceConfig {
    pub address: String,
    /// Grace period (seconds) allowed for the connection shutdown handshake.
    pub shutdown_timeout_secs: u64,
    /// Idle timeout (seconds) for an established connection, so a stalled upload
    /// cannot hold the listener.
    pub inactivity_timeout_secs: u64,
    // The `[socket_source.tls]` section is only read when the `tls` feature is
    // enabled; without it the section is ignored. Its presence enables TLS.
    #[cfg(feature = "tls")]
    pub tls: Option<tls::SocketTlsConfig>,
    /// SWUpdate download parameters, shared with the fastboot schema so the same
    /// keys configure the update regardless of transport. These are orthogonal
    /// to the transport fields above — they only affect what SWUpdate does with
    /// the received bytes, not how the bytes arrive.
    #[serde(flatten)]
    pub swupdate: SwupdateConfig,
}

impl Default for SocketSourceConfig {
    fn default() -> Self {
        Self {
            address: "0.0.0.0:9000".to_string(),
            shutdown_timeout_secs: 15,
            inactivity_timeout_secs: 30,
            #[cfg(feature = "tls")]
            tls: None,
            swupdate: SwupdateConfig::default(),
        }
    }
}

impl summit_usbgadget_config::PluginConfig for SocketSourceConfig {
    const SECTION: Option<&'static str> = Some(SOCKET_SOURCE_SECTION);
}

impl SocketSourceConfig {
    /// Resolves the SWUpdate parameters, validating the download target.
    pub fn to_swupdate_params(&self) -> Result<SwupdateParams, SwupdateConfigError> {
        self.swupdate.to_params()
    }
}

/// Runs the socket update anchor: loads the `socket_source` configuration from
/// `config_path` and serves streamed firmware into SWUpdate until `shutdown` is
/// requested.
pub async fn run(
    config_path: impl AsRef<std::path::Path>,
    shutdown: Shutdown,
) -> Result<(), Box<dyn Error>> {
    let config = SocketSourceConfig::load(config_path)?;
    let params = config.to_swupdate_params()?;
    tokio::select! {
        result = serve_swupdate(&config, params) => result?,
        _ = shutdown.wait() => {}
    }
    Ok(())
}

// Register the socket source as a top-level startup service.
summit_usbgadget_config::declare_service!("socket" => run);

/// Logs the outcome of one client session. A failed session is reported but
/// never propagated, so the listener loops on to serve the next client.
fn log_session_end(result: io::Result<()>) {
    if let Err(err) = result {
        log::error!("socket update session failed: {err}");
    }
}

const SOCKET_READ_BUFFER_SIZE: usize = 128 * 1024;

pub async fn serve_swupdate(config: &SocketSourceConfig, params: SwupdateParams) -> io::Result<()> {
    let inactivity_timeout = Duration::from_secs(config.inactivity_timeout_secs);
    let shutdown_timeout = Duration::from_secs(config.shutdown_timeout_secs);

    loop {
        // Listen, accept exactly one client, then drop the listener so the port
        // is closed while we serve. Any other client is refused by the kernel
        // (connection refused) — one update at a time, no bookkeeping. Re-bind
        // for the next client once the session ends.
        let listener = TcpListener::bind(&config.address).await?;
        log::info!("socket update listening on {}", config.address);
        let (sock, _) = listener.accept().await?;
        drop(listener);
        log::info!("socket update client connected from {}", sock.peer_addr()?);

        // The plain TCP and TLS streams are distinct concrete types, so under
        // static dispatch the TLS upgrade needs its own monomorphized session
        // call. The plain call below then serves both the no-TLS build and the
        // case where TLS is compiled in but not configured. The idle timeout also
        // bounds the handshake; there is no separate accept/handshake timeout.
        #[cfg(feature = "tls")]
        if let Some(tls) = config.tls.as_ref() {
            let stream: tokio_openssl::SslStream<tokio::net::TcpStream> = tls::accept(sock, tls, inactivity_timeout).await?;
            log_session_end(
                stream_to_swupdate(stream, &params, inactivity_timeout, shutdown_timeout).await,
            );
            continue;
        }
        log_session_end(
            stream_to_swupdate(sock, &params, inactivity_timeout, shutdown_timeout).await,
        );
    }
}

/// How the pump loop ended.
enum PumpEnd {
    /// The source stopped sending: a clean EOF (read returned 0), a read
    /// failure, or an idle timeout. Flush EOF into the queue and let swupdate
    /// drain what arrived and render the verdict — the queue is never discarded.
    SourceClosed,
    /// swupdate reached its verdict while the source was still connected.
    SwupdateDone,
}

async fn stream_to_swupdate<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    params: &SwupdateParams,
    inactivity_timeout: Duration,
    shutdown_timeout: Duration,
) -> io::Result<()> {
    let mut sink = SwupdateSession::new(params.clone(), SOCKET_READ_BUFFER_SIZE);

    let result = match pump(&mut stream, &mut sink, inactivity_timeout).await {
        // Source stopped sending (clean EOF, read error, or inactivity timeout):
        // flush EOF into swupdate and let it render the verdict on what arrived.
        Ok(PumpEnd::SourceClosed) if sink.is_open() => {
            sink.eof();
            sink.finished().await
        }
        // Source closed before sending any payload: nothing to do, go idle.
        Ok(PumpEnd::SourceClosed) => {
            log::warn!("socket update connection closed without payload");
            Ok(())
        }
        // swupdate finished while the source was still connected: drop it.
        Ok(PumpEnd::SwupdateDone) => Ok(()),
        Err(err) => Err(err),
    };

    if result.is_err() {
        sink.abort().await;
    }

    // Closing the connection means sending EOF (FIN) on the stream.
    timeout(shutdown_timeout, stream.shutdown())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "socket shutdown timed out"))??;

    result
}

/// Pumps bytes from the single client stream into the single swupdate sink until
/// one side finishes. Each iteration reads one block and forwards it — there is
/// only one stream and one sink, so the read is inlined here.
async fn pump<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    sink: &mut SwupdateSession,
    inactivity_timeout: Duration,
) -> io::Result<PumpEnd> {
    loop {
        // One queue element, filled straight from the socket. `read_buf` appends
        // only the bytes that arrive (no zero-fill, no truncate).
        let mut block: BytesMut = sink.buffer(SOCKET_READ_BUFFER_SIZE);
        block.clear();

        // Read one block, bounding the wait by the idle timeout. EOF, a read
        // error, or a timeout all end the source: stop reading and let swupdate
        // drain what it already received.
        let read = match timeout(inactivity_timeout, stream.read_buf(&mut block)).await {
            Ok(read) => read,
            Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "socket update connection inactive")),
        };
        match read {
            Ok(0) => return Ok(PumpEnd::SourceClosed),
            Ok(_) => {}
            Err(err) => {
                log::warn!("socket update read stopped, draining to swupdate: {err}");
                return Ok(PumpEnd::SourceClosed);
            }
        }

        // swupdate may fail early on a bad image: stop and report the verdict.
        if sink.is_open()
            && let Some(result) = sink.try_finished()
        {
            return result.map(|()| PumpEnd::SwupdateDone);
        }

        // Hand the filled block to the queue.
        if matches!(sink.send(block).await?, Feed::Closed) {
            return Ok(PumpEnd::SwupdateDone);
        }
    }
}
