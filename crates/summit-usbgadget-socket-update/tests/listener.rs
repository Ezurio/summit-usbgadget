//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

//! Integration tests for the socket update listener, exercising only its
//! public API.

use summit_usbgadget_socket_update::{SocketSourceConfig, UpdateSocketListener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[test]
fn swupdate_params_follow_socket_source_settings() {
    let config = SocketSourceConfig {
        address: "127.0.0.1:9000".to_string(),
        accept_timeout_secs: Some(3),
        shutdown_timeout_secs: Some(7),
        inactivity_timeout_secs: Some(20),
        #[cfg(feature = "tls")]
        tls: None,
        software_set: Some("beta".to_string()),
        image_mode: Some("delta".to_string()),
        dry_run: Some(true),
        disable_store_swu: Some(false),
        timeout_secs: Some(55),
    };

    let params = config.swupdate_params();
    assert_eq!(params.software_set.as_deref(), Some("beta"));
    assert_eq!(params.image_mode.as_deref(), Some("delta"));
    assert!(params.dry_run);
    assert!(!params.disable_store_swu);
    assert_eq!(params.timeout.as_secs(), 55);
}

#[tokio::test(flavor = "current_thread")]
async fn listener_accepts_plain_tcp_streams() {
    let config = SocketSourceConfig {
        address: "127.0.0.1:0".to_string(),
        accept_timeout_secs: Some(2),
        shutdown_timeout_secs: Some(2),
        inactivity_timeout_secs: None,
        #[cfg(feature = "tls")]
        tls: None,
        software_set: None,
        image_mode: None,
        dry_run: None,
        disable_store_swu: None,
        timeout_secs: None,
    };

    let listener = UpdateSocketListener::bind(&config)
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener address");

    let accept_task = tokio::spawn(async move { listener.accept().await });
    let mut client = TcpStream::connect(addr).await.expect("client should connect");
    client.write_all(b"ping").await.expect("client write should succeed");

    let mut accepted = accept_task
        .await
        .expect("accept task should complete")
        .expect("accept should succeed");

    let mut buf = [0u8; 4];
    accepted.read_exact(&mut buf).await.expect("accepted stream should read payload");
    assert_eq!(&buf, b"ping");
}

#[tokio::test(flavor = "current_thread")]
async fn listener_times_out_without_client() {
    let config = SocketSourceConfig {
        address: "127.0.0.1:0".to_string(),
        accept_timeout_secs: Some(0),
        shutdown_timeout_secs: Some(2),
        inactivity_timeout_secs: None,
        #[cfg(feature = "tls")]
        tls: None,
        software_set: None,
        image_mode: None,
        dry_run: None,
        disable_store_swu: None,
        timeout_secs: None,
    };

    let listener = UpdateSocketListener::bind(&config)
        .await
        .expect("listener should bind");
    let err = listener
        .accept()
        .await
        .err()
        .expect("accept should time out without a client");
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
}
