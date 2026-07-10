use std::error::Error;
use std::io;
use std::time::Duration;

use bytes::BytesMut;
use serde::Deserialize;
use summit_usbgadget_config::{PluginConfig, Shutdown};
use summit_usbgadget_swupdate::{Feed, SwupdateParams, SwupdateSession};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;

#[cfg(feature = "tls")]
mod tls;
#[cfg(feature = "tls")]
use tls::SocketTlsConfig;

pub const SOCKET_SOURCE_SECTION: &str = "socket_source";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SocketSourceConfig {
    #[serde(default = "default_socket_source_address")]
    pub address: String,
    pub accept_timeout_secs: Option<u64>,
    pub shutdown_timeout_secs: Option<u64>,
    /// Idle timeout (seconds) for an established connection; `0` disables it.
    /// Defaults to 30 seconds so a stalled upload cannot hold the listener.
    pub inactivity_timeout_secs: Option<u64>,
    // The `[socket_source.tls]` section is only read when the `tls` feature is
    // enabled; without it the section is ignored. Its presence enables TLS.
    #[cfg(feature = "tls")]
    pub tls: Option<SocketTlsConfig>,
    pub software_set: Option<String>,
    pub image_mode: Option<String>,
    pub dry_run: Option<bool>,
    pub disable_store_swu: Option<bool>,
    pub timeout_secs: Option<u64>,
}

impl summit_usbgadget_config::PluginConfig for SocketSourceConfig {
    const SECTION: Option<&'static str> = Some(SOCKET_SOURCE_SECTION);
}

fn default_socket_source_address() -> String {
    "0.0.0.0:9000".to_string()
}

/// Default idle timeout (seconds) applied to an established connection.
const DEFAULT_INACTIVITY_TIMEOUT_SECS: u64 = 30;

pub struct UpdateSocketListener {
    listener: TcpListener,
    config: SocketSourceConfig,
}

/// Any async byte stream that can carry an update payload: a plain [`TcpStream`]
/// or, when the `tls` feature is enabled, an OpenSSL stream. Combining the two
/// non-auto I/O traits into one object-safe trait lets a single boxed trait
/// object stand in for either.
pub trait SocketIo: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> SocketIo for T {}

/// A boxed update stream (plain TCP or, with the `tls` feature, OpenSSL).
///
/// `Box<dyn SocketIo>` already implements `AsyncRead`/`AsyncWrite` through
/// tokio's blanket `Box` impls, so no wrapper is needed.
pub type UpdateSocketStream = Box<dyn SocketIo>;

impl SocketSourceConfig {
    pub fn swupdate_params(&self) -> SwupdateParams {
        SwupdateParams {
            software_set: self.software_set.clone(),
            image_mode: self.image_mode.clone(),
            dry_run: self.dry_run.unwrap_or(false),
            disable_store_swu: self.disable_store_swu.unwrap_or(true),
            timeout: Duration::from_secs(self.timeout_secs.unwrap_or(120)),
        }
    }

    fn inactivity_timeout(&self) -> Option<Duration> {
        match self.inactivity_timeout_secs {
            Some(0) => None,
            Some(secs) => Some(Duration::from_secs(secs)),
            None => Some(Duration::from_secs(DEFAULT_INACTIVITY_TIMEOUT_SECS)),
        }
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
    tokio::select! {
        result = serve_swupdate(&config) => result?,
        _ = shutdown.wait() => {}
    }
    Ok(())
}

// Register the socket source as a top-level startup service.
summit_usbgadget_config::declare_service!("socket" => run);

impl UpdateSocketListener {
    pub async fn bind(config: &SocketSourceConfig) -> io::Result<Self> {
        let listener = TcpListener::bind(&config.address).await?;
        Ok(Self { listener, config: config.clone() })
    }

    pub async fn accept(&self) -> io::Result<UpdateSocketStream> {
        let (stream, _) = timeout(self.accept_timeout(), self.listener.accept())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "socket accept timed out"))??;
        let _ = stream.set_nodelay(true);

        // TLS, when enabled and configured, is just an upgrade of the accepted
        // connection; otherwise the plain stream is used as-is.
        #[cfg(feature = "tls")]
        if let Some(tls) = self.config.tls.as_ref() {
            return Ok(tls::accept(stream, tls, self.accept_timeout()).await?);
        }

        Ok(Box::new(stream))
    }

    fn accept_timeout(&self) -> Duration {
        Duration::from_secs(self.config.accept_timeout_secs.unwrap_or(15))
    }

    pub fn shutdown_timeout(&self) -> Duration {
        Duration::from_secs(self.config.shutdown_timeout_secs.unwrap_or(15))
    }

    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }
}

const SOCKET_READ_BUFFER_SIZE: usize = 128 * 1024;

fn socket_read_buffer(sink: &mut SwupdateSession) -> BytesMut {
    let mut buf = sink.buffer(SOCKET_READ_BUFFER_SIZE);
    buf.resize(SOCKET_READ_BUFFER_SIZE, 0);
    buf
}

pub async fn serve_swupdate(config: &SocketSourceConfig) -> io::Result<()> {
    let listener = UpdateSocketListener::bind(config).await?;
    let params = config.swupdate_params();
    let inactivity_timeout = config.inactivity_timeout();

    loop {
        match listener.accept().await {
            Ok(stream) => {
                if let Err(err) =
                    stream_to_swupdate(stream, &params, inactivity_timeout, listener.shutdown_timeout()).await
                {
                    log::error!("socket update session failed: {err}");
                }
            }
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                log::debug!("socket update accept timed out waiting for a client");
            }
            Err(err) => return Err(err),
        }
    }
}

/// How the pump loop ended.
enum PumpEnd {
    /// The source stopped sending: either a clean EOF (read returned 0) or a
    /// read failure/inactivity timeout. Both are handled the same way — flush
    /// EOF into the queue and let swupdate render the verdict.
    SourceClosed,
    /// swupdate reached its verdict while the source was still connected.
    SwupdateDone,
}

async fn stream_to_swupdate(
    mut stream: UpdateSocketStream,
    params: &SwupdateParams,
    inactivity_timeout: Option<Duration>,
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

/// Pumps source bytes into swupdate until one side finishes.
async fn pump(
    stream: &mut UpdateSocketStream,
    sink: &mut SwupdateSession,
    inactivity_timeout: Option<Duration>,
) -> io::Result<PumpEnd> {
    loop {
        let mut buf = socket_read_buffer(sink);
        let read = match read_with_timeout(stream, &mut buf[..], inactivity_timeout).await {
            // A read error or inactivity timeout is treated like a clean EOF:
            // stop reading and let swupdate judge what was received.
            Err(err) => {
                log::warn!("socket update read failed, flushing to swupdate: {err}");
                return Ok(PumpEnd::SourceClosed);
            }
            Ok(0) => return Ok(PumpEnd::SourceClosed),
            Ok(read) => read,
        };

        // swupdate may reach its verdict mid-transfer: a failure surfaces as an
        // error, an early success lets us stop and drop the connection.
        if sink.is_open() {
            if let Some(result) = sink.try_finished() {
                return result.map(|()| PumpEnd::SwupdateDone);
            }
        }

        buf.truncate(read);
        if matches!(sink.send(buf).await?, Feed::Closed) {
            return Ok(PumpEnd::SwupdateDone);
        }
    }
}

/// Reads from `stream`, enforcing `inactivity_timeout` when set: if no bytes
/// arrive within the limit the read fails with a `TimedOut` error, which the
/// caller folds into a source close (flush EOF, let swupdate decide).
async fn read_with_timeout(
    stream: &mut UpdateSocketStream,
    buf: &mut [u8],
    inactivity_timeout: Option<Duration>,
) -> io::Result<usize> {
    match inactivity_timeout {
        Some(limit) => timeout(limit, stream.read(buf))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "socket update connection inactive"))?,
        None => stream.read(buf).await,
    }
}
