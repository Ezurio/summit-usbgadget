//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Exercises `serve()` itself (bind / accept / drop-listener loop), not
/// just `TcpFastbootSession` in isolation — this is the code that changed
/// when the listener loop was rewritten to bind fresh per client.
#[tokio::test]
async fn serve_binds_and_accepts_a_real_client() {
    let config: &'static FastbootTcpConfig = Box::leak(Box::new(FastbootTcpConfig {
        address: "127.0.0.1:0".to_string(),
        serial: "abc123".to_string(),
        ..Default::default()
    }));
    // Bind once ourselves just to grab a free port, then hand that exact
    // address to `serve()` so it can rebind it.
    let probe = TcpListener::bind(&config.address).await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let config: &'static FastbootTcpConfig = Box::leak(Box::new(FastbootTcpConfig {
        address: addr.to_string(),
        ..config.clone()
    }));
    let params = config.to_swupdate_params().unwrap();

    let server = tokio::spawn(serve(config, params));

    // The listener may not be bound yet; retry the connect briefly.
    let mut client = loop {
        match TcpStream::connect(&config.address).await {
            Ok(stream) => break stream,
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    };

    client.write_all(b"FB01").await.unwrap();
    let mut buf = [0u8; 4];
    let _ = client.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"FB01");

    let cmd: &[u8] = b"getvar:version";
    client.write_all(&(cmd.len() as u64).to_be_bytes()).await.unwrap();
    client.write_all(cmd).await.unwrap();

    let mut header = [0u8; 8];
    let _ = client.read_exact(&mut header).await.unwrap();
    let len = u64::from_be_bytes(header) as usize;
    let mut reply = vec![0u8; len];
    let _ = client.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply, b"OKAY0.4");

    server.abort();
}
