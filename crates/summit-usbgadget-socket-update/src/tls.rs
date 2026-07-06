//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

//! OpenSSL-backed TLS for the update socket listener.
//!
//! Everything TLS — the `[socket_source.tls]` config schema and all
//! OpenSSL / tokio-openssl usage — is confined to this module, which is only
//! compiled with the `tls` feature. Without the feature the module is absent,
//! the TLS config data is not compiled, and the `[socket_source.tls]` section
//! is simply ignored. It exposes a single entry point, [`accept`], which
//! performs the handshake and returns the established stream boxed as a
//! [`SocketIo`](super::SocketIo) so the rest of the crate never sees the
//! concrete OpenSSL type.

use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

#[cfg(ossl300)]
use openssl::provider::Provider;
use openssl::ssl::{Ssl, SslAcceptor, SslFiletype, SslMethod, SslVerifyMode};
use openssl::x509::X509VerifyResult;
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_openssl::SslStream;

use super::SocketIo;

/// The `[socket_source.tls]` section as read from the configuration file. Its
/// presence enables TLS on the listener, and it is used directly to build the
/// OpenSSL acceptor (defaults for the optional flags are applied by serde).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SocketTlsConfig {
    pub server_cert: PathBuf,
    pub server_key: PathBuf,
    #[serde(default)]
    pub request_client_cert: bool,
    #[serde(default)]
    pub ca_cert: Option<PathBuf>,
    #[serde(default = "default_true")]
    pub ignore_expiration: bool,
    #[serde(default)]
    pub fips: bool,
}

fn default_true() -> bool {
    true
}

/// Performs the TLS handshake on an accepted connection and returns the
/// established stream boxed as a [`SocketIo`], so callers never see the
/// concrete OpenSSL type.
pub(super) async fn accept(
    stream: TcpStream,
    config: &SocketTlsConfig,
    accept_timeout: Duration,
) -> io::Result<Box<dyn SocketIo>> {
    let acceptor = build_tls_acceptor(config)?;
    let ssl = Ssl::new(acceptor.context())
        .map_err(|err| io::Error::other(format!("OpenSSL TLS session setup failed: {err}")))?;
    let mut tls_stream = SslStream::new(ssl, stream)
        .map_err(|err| io::Error::other(format!("OpenSSL stream setup failed: {err}")))?;
    timeout(accept_timeout, Pin::new(&mut tls_stream).accept())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timed out"))?
        .map_err(|err| io::Error::other(format!("TLS handshake failed: {err}")))?;
    Ok(Box::new(tls_stream))
}

fn build_tls_acceptor(config: &SocketTlsConfig) -> io::Result<SslAcceptor> {
    enable_fips_if_requested(config)?;

    let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server())
        .map_err(|err| io::Error::other(format!("OpenSSL TLS builder creation failed: {err}")))?;

    builder
        .set_certificate_chain_file(&config.server_cert)
        .map_err(|err| io::Error::other(format!(
            "loading OpenSSL server certificate chain {} failed: {err}",
            config.server_cert.display()
        )))?;
    builder
        .set_private_key_file(&config.server_key, SslFiletype::PEM)
        .map_err(|err| io::Error::other(format!(
            "loading OpenSSL server private key {} failed: {err}",
            config.server_key.display()
        )))?;
    builder
        .check_private_key()
        .map_err(|err| io::Error::other(format!("OpenSSL server certificate/key mismatch: {err}")))?;

    if config.request_client_cert {
        let ca_cert = config.ca_cert.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "client certificate validation requires a ca_cert",
            )
        })?;
        builder
            .set_ca_file(ca_cert)
            .map_err(|err| io::Error::other(format!("loading OpenSSL CA file {} failed: {err}", ca_cert.display())))?;

        if config.ignore_expiration {
            builder.set_verify_callback(SslVerifyMode::PEER, |preverified, ctx| {
                preverified || is_ignorable_expiration_result(ctx.error())
            });
        } else {
            builder.set_verify(SslVerifyMode::PEER);
        }
    }

    Ok(builder.build())
}

fn enable_fips_if_requested(config: &SocketTlsConfig) -> io::Result<()> {
    if !config.fips {
        return Ok(());
    }

    #[cfg(ossl300)]
    {
        let _default_provider = Provider::try_load(None, "default", true)
            .map_err(|err| io::Error::other(format!("loading OpenSSL default provider failed: {err}")))?;
        let _fips_provider = Provider::try_load(None, "fips", true)
            .map_err(|err| io::Error::other(format!("loading OpenSSL FIPS provider failed: {err}")))?;

        let enabled = unsafe { openssl::ffi::EVP_default_properties_enable_fips(std::ptr::null_mut(), 1) };
        if enabled != 1 {
            return Err(io::Error::other("enabling OpenSSL 3 FIPS default properties failed"));
        }

        return Ok(());
    }

    #[cfg(not(ossl300))]
    {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "TLS FIPS mode requires an OpenSSL 3 build",
        ))
    }
}

fn is_ignorable_expiration_result(result: X509VerifyResult) -> bool {
    matches!(result.as_raw(), 9 | 10 | 13 | 14)
}
