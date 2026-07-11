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
//! performs the handshake and returns the established [`SslStream`] so the rest
//! of the crate feeds it through the same generic session pipeline as a plain
//! TCP stream.

use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use openssl::provider::Provider;
use openssl::ssl::{Ssl, SslAcceptor, SslFiletype, SslMethod, SslVerifyMode};
use openssl::x509::X509VerifyResult;
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_openssl::SslStream;

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
/// established [`SslStream`], which the caller feeds through the same generic
/// session pipeline as a plain TCP stream.
pub(super) async fn accept(
    stream: TcpStream,
    config: &SocketTlsConfig,
    handshake_timeout: Duration,
) -> io::Result<SslStream<TcpStream>> {
    let acceptor = build_tls_acceptor(config)?;
    let ssl = Ssl::new(acceptor.context())
        .map_err(|err| io::Error::other(format!("OpenSSL TLS session setup failed: {err}")))?;
    let mut tls_stream = SslStream::new(ssl, stream)
        .map_err(|err| io::Error::other(format!("OpenSSL stream setup failed: {err}")))?;
    timeout(handshake_timeout, Pin::new(&mut tls_stream).accept())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timed out"))?
        .map_err(|err| io::Error::other(format!("TLS handshake failed: {err}")))?;
    Ok(tls_stream)
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

    let mut verify_mode = SslVerifyMode::NONE;
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
        verify_mode = SslVerifyMode::PEER;
    }

    // The device has no reliable time source, so certificate expiration cannot be
    // trusted as a rejection reason. When `ignore_expiration` is set, install a
    // verify callback that accepts certs whose only fault is a date error. This is
    // decoupled from `request_client_cert`: it adjusts whichever verification mode
    // was selected above rather than being nested under client-cert handling.
    if config.ignore_expiration {
        builder.set_verify_callback(verify_mode, |preverified, ctx| {
            preverified || is_ignorable_expiration_result(ctx.error())
        });
    } else {
        builder.set_verify(verify_mode);
    }

    Ok(builder.build())
}

fn enable_fips_if_requested(config: &SocketTlsConfig) -> io::Result<()> {
    if !config.fips {
        return Ok(());
    }

    let _default_provider = Provider::try_load(None, "default", true)
        .map_err(|err| io::Error::other(format!("loading OpenSSL default provider failed: {err}")))?;
    let _fips_provider = Provider::try_load(None, "fips", true)
        .map_err(|err| io::Error::other(format!("loading OpenSSL FIPS provider failed: {err}")))?;

    let enabled = unsafe { openssl_sys::EVP_default_properties_enable_fips(std::ptr::null_mut(), 1) };
    if enabled != 1 {
        return Err(io::Error::other("enabling OpenSSL FIPS default properties failed"));
    }

    Ok(())
}

fn is_ignorable_expiration_result(result: X509VerifyResult) -> bool {
    matches!(result.as_raw(), 9 | 10 | 13 | 14)
}
