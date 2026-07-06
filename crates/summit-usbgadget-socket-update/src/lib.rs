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

const SOCKET_READ_BUFFER_SIZE: usize = 64 * 1024;

fn socket_read_buffer(sink: &mut SwupdateSession) -> BytesMut {
    let mut buf = sink.buffer(SOCKET_READ_BUFFER_SIZE);
    buf.resize(SOCKET_READ_BUFFER_SIZE, 0);
    buf
}

pub async fn serve_swupdate(config: &SocketSourceConfig) -> io::Result<()> {
    let listener = UpdateSocketListener::bind(config).await?;
    let params = config.swupdate_params();

    loop {
        match listener.accept().await {
            Ok(stream) => {
                if let Err(err) = stream_to_swupdate(stream, &params, listener.shutdown_timeout()).await {
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

async fn stream_to_swupdate(
    mut stream: UpdateSocketStream,
    params: &SwupdateParams,
    shutdown_timeout: Duration,
) -> io::Result<()> {
    let mut sink = SwupdateSession::new(params.clone(), SOCKET_READ_BUFFER_SIZE);
    let mut received_payload = false;

    let result = async {
        let mut forwarding = true;
        loop {
            let mut buf = socket_read_buffer(&mut sink);
            let read = stream.read(&mut buf[..]).await?;
            if read == 0 {
                break;
            }

            received_payload = true;

            // A swupdate failure aborts immediately so the client sees the
            // connection error. Early success keeps draining to EOF below.
            if sink.is_open() {
                if let Some(Err(err)) = sink.try_finished() {
                    return Err(err);
                }
            }

            if forwarding {
                buf.truncate(read);
                if matches!(sink.send(buf).await?, Feed::Closed) {
                    // swupdate is done; keep draining the client to EOF but
                    // discard — uploaders send the whole file regardless.
                    forwarding = false;
                }
            }
        }

        if !received_payload {
            log::warn!("socket update connection closed without payload");
            return Ok(());
        }

        sink.eof();
        sink.finished().await?;

        Ok(())
    }
    .await;

    if result.is_err() {
        sink.abort().await;
    }

    timeout(shutdown_timeout, stream.shutdown())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "socket shutdown timed out"))??;

    result
}
