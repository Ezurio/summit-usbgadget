//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

use super::*;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn test_config() -> FastbootTcpConfig {
    FastbootTcpConfig {
        serial: "abc123".to_string(),
        shutdown_timeout_secs: 5,
        ..Default::default()
    }
}

/// Binds an ephemeral loopback listener and returns one connected pair
/// (server side, client side) — a real TCP socket, matching what `serve()`
/// hands to `TcpFastbootSession` in production.
async fn tcp_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (server, (client, _)) = tokio::join!(TcpStream::connect(addr), async {
        listener.accept().await.unwrap()
    });
    (server.unwrap(), client)
}

async fn client_handshake(client: &mut TcpStream) {
    client.write_all(b"FB01").await.unwrap();
    let mut buf = [0u8; 4];
    let _ = client.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"FB01");
}

async fn send_frame(client: &mut TcpStream, data: &[u8]) {
    client.write_all(&(data.len() as u64).to_be_bytes()).await.unwrap();
    client.write_all(data).await.unwrap();
}

async fn recv_frame(client: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 8];
    let _ = client.read_exact(&mut header).await.unwrap();
    let len = u64::from_be_bytes(header) as usize;
    let mut buf = vec![0u8; len];
    let _ = client.read_exact(&mut buf).await.unwrap();
    buf
}

#[tokio::test]
async fn handshake_and_getvar_over_tcp() {
    let (server, mut client) = tcp_pair().await;
    let config = test_config();
    let session = TcpFastbootSession::new(FastbootFraming::new(server, None), &config, SwupdateParams::default(), "test".to_string());

    // The session borrows `config`, so it can't be `tokio::spawn`ed; drive it
    // alongside the client on this same task instead.
    let (result, ()) = tokio::join!(session.run(), async {
        client_handshake(&mut client).await;

        send_frame(&mut client, b"getvar:version").await;
        assert_eq!(recv_frame(&mut client).await, b"OKAY0.4");

        send_frame(&mut client, b"getvar:serialno").await;
        assert_eq!(recv_frame(&mut client).await, b"OKAYabc123");

        send_frame(&mut client, b"getvar:nonexistent").await;
        assert_eq!(recv_frame(&mut client).await, b"FAILunknown command");

        send_frame(&mut client, b"powerdown").await;
        assert_eq!(recv_frame(&mut client).await, b"FAILunknown command");

        drop(client);
    });
    result.unwrap();
}

#[tokio::test]
async fn rejects_malformed_handshake() {
    let (server, mut client) = tcp_pair().await;
    let config = test_config();
    let session = TcpFastbootSession::new(FastbootFraming::new(server, None), &config, SwupdateParams::default(), "test".to_string());

    let (result, ()) = tokio::join!(session.run(), async {
        // Read (and ignore) the device handshake, then send a bad one.
        let mut buf = [0u8; 4];
        let _ = client.read_exact(&mut buf).await.unwrap();
        client.write_all(b"XX01").await.unwrap();
        drop(client);
    });
    assert!(result.is_err(), "malformed handshake should fail the session");
}

#[tokio::test]
async fn idle_connection_times_out() {
    let (server, mut client) = tcp_pair().await;
    let config = test_config();
    let session = TcpFastbootSession::new(
        FastbootFraming::new(server, Some(Duration::from_millis(50))),
        &config,
        SwupdateParams::default(),
        "test".to_string(),
    );

    let (result, ()) = tokio::join!(session.run(), async {
        // Complete the handshake, then stay idle without sending a command.
        client_handshake(&mut client).await;
    });
    let err = result.expect_err("an idle connection should time out");
    assert_eq!(err.kind(), ErrorKind::TimedOut);
}
