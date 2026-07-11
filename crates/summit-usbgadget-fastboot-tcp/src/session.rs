//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! Fastboot command / data state machine over a TCP frame transport.
//!
//! Reuses the transport-agnostic command vocabulary from
//! [`summit_usbgadget_fastboot_proto`] and the shared [`SwupdateSession`] sink,
//! so the getvar / fetch / download / flash / Close semantics match the USB fastboot-usb
//! function exactly. The only fastboot-specific differences here are the TCP
//! framing (each command and reply is one frame) and error handling (a fatal
//! error drops the connection rather than halting a USB endpoint).

use std::io::{self, ErrorKind};

use bytes::BytesMut;
use summit_usbgadget_fastboot_proto::{
    data_header, fastboot_getvar_reply, parse_command, DownloadKind, FetchTarget, FlashTarget,
    ParsedCommand, FAIL_CLOSE, FAIL_CMD, FAIL_EPIPE, FAIL_FLASH, FAIL_OPEN, FAIL_UNKNOWN_PART,
    INFO_WAIT_SWUPDATE, OKAY,
};
use summit_usbgadget_swupdate::sysinfo::SystemInfo;
use summit_usbgadget_swupdate::{pump_to_swupdate, NextSwupdateBlock, PumpToSwupdateEnd, SwupdateParams, SwupdatePumpSource, SwupdateSession};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::transport::FastbootFraming;
use crate::RECV_BUFFER_SIZE;

/// One fastboot-over-TCP client session.
pub(crate) struct TcpFastbootSession<S> {
    transport: FastbootFraming<S>,
    params: SwupdateParams,
    sink: SwupdateSession,
    serial: String,
    peer: String,
    fastboot_usb_session_open: bool,
    fastboot_pending_flash: bool,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> TcpFastbootSession<S> {
    pub(crate) fn new(
        transport: FastbootFraming<S>,
        params: SwupdateParams,
        serial: String,
        peer: String,
    ) -> Self {
        let sink = SwupdateSession::new(params.clone(), RECV_BUFFER_SIZE);
        Self {
            transport,
            params,
            sink,
            serial,
            peer,
            fastboot_usb_session_open: false,
            fastboot_pending_flash: false,
        }
    }

    /// Handshakes and services commands until the client disconnects or a fatal
    /// transport error occurs. On error the SWUpdate sink is torn down.
    pub(crate) async fn run(mut self) -> io::Result<()> {
        self.transport.handshake().await?;
        log::info!("fastboot-tcp {} handshake complete", self.peer);

        let result = self.command_loop().await;
        if result.is_err() {
            self.sink.abort().await;
        }
        result
    }

    async fn command_loop(&mut self) -> io::Result<()> {
        loop {
            let Some(frame) = self.transport.recv_command().await? else {
                log::info!("fastboot-tcp {} disconnected", self.peer);
                return Ok(());
            };
            self.dispatch(frame.as_ref()).await?;
        }
    }

    /// Dispatches one command. `Err` signals a fatal transport failure (the
    /// connection is dropped); command-level failures reply `FAIL...` and return
    /// `Ok` so the session continues.
    async fn dispatch(&mut self, cmd: &[u8]) -> io::Result<()> {
        log::debug!("fastboot-tcp {} command: {}", self.peer, String::from_utf8_lossy(cmd));

        match parse_command(cmd) {
            ParsedCommand::WOpen => self.handle_wopen().await,
            ParsedCommand::GetVar => self.handle_getvar(cmd).await,
            ParsedCommand::Fetch(target) => self.handle_fetch(target).await,
            ParsedCommand::FetchUnknownPart => self.transport.send_packet(FAIL_UNKNOWN_PART).await,
            ParsedCommand::Download { len, kind } => self.handle_download(len, kind).await,
            ParsedCommand::Flash(target) => self.handle_flash(target).await,
            ParsedCommand::FlashUnknownPart => self.transport.send_packet(FAIL_UNKNOWN_PART).await,
            ParsedCommand::Close => self.finish_download().await,
            ParsedCommand::Unsupported => {
                log::warn!("fastboot-tcp {} unsupported command: {}", self.peer, String::from_utf8_lossy(cmd));
                self.transport.send_packet(FAIL_CMD).await
            }
        }
    }

    async fn handle_wopen(&mut self) -> io::Result<()> {
        if let Err(err) = self.sink.open().await {
            log::error!("fastboot-tcp {}: WOpen begin failed: {err}", self.peer);
            return self.transport.send_packet(FAIL_OPEN).await;
        }
        self.fastboot_usb_session_open = true;
        self.fastboot_pending_flash = false;
        self.transport.send_packet(OKAY).await
    }

    async fn handle_getvar(&mut self, cmd: &[u8]) -> io::Result<()> {
        match fastboot_getvar_reply(cmd, &self.serial) {
            Some(reply) => self.transport.send_packet(&reply).await,
            None => self.transport.send_packet(FAIL_CMD).await,
        }
    }

    async fn handle_fetch(&mut self, target: FetchTarget) -> io::Result<()> {
        let label = match target {
            FetchTarget::Sysinfo => "sysinfo",
            FetchTarget::SysinfoJson => "sysinfo.json",
        };
        // Serialize the JSON straight into its data-phase frame, after an 8-byte
        // length-prefix placeholder, then backfill the prefix and send the whole
        // frame in one write — no intermediate payload buffer, no split writes.
        let mut frame = vec![0u8; 8];
        SystemInfo::collect(Some(self.serial.clone())).write_json(&mut frame);
        let len = frame.len() - 8;
        frame[..8].copy_from_slice(&(len as u64).to_be_bytes());
        log::info!("fastboot-tcp {}: fetch {label} ({len} bytes)", self.peer);
        self.transport.send_packet(data_header(len).as_bytes()).await?;
        self.transport.send_prebuilt(&frame).await?;
        self.transport.send_packet(OKAY).await
    }

    async fn handle_download(&mut self, len: usize, kind: DownloadKind) -> io::Result<()> {
        if !self.sink.is_open()
            && let Err(err) = self.sink.open().await
        {
            log::error!("fastboot-tcp {}: download begin failed: {err}", self.peer);
            return self.transport.send_packet(FAIL_OPEN).await;
        }

        self.transport.send_packet(data_header(len).as_bytes()).await?;

        self.fastboot_pending_flash = match kind {
            DownloadKind::Fastboot => true,
            DownloadKind::Plain => !self.fastboot_usb_session_open,
        };

        if len == 0 {
            return self.transport.send_packet(OKAY).await;
        }

        self.stream_download(len).await
    }

    /// Reads exactly `total` payload bytes across one or more data-phase frames,
    /// feeding them into SWUpdate, then acknowledges the completed data phase.
    async fn stream_download(&mut self, total: usize) -> io::Result<()> {
        let mut source = FastbootDownloadSource::new(&mut self.transport, total);
        let mut forwarding = true;

        while source.remaining > 0 {
            if !forwarding {
                let _ = source.read_block(&mut self.sink).await?;
                continue;
            }

            let pump_end = pump_to_swupdate(&mut self.sink, &mut source).await?;

            match pump_end {
                PumpToSwupdateEnd::InputClosed => break,
                PumpToSwupdateEnd::SwupdateFinished(Ok(())) => {
                    // swupdate already finished; keep draining the client
                    // (uploaders send the whole image) but discard the bytes.
                    forwarding = false;
                }
                PumpToSwupdateEnd::SwupdateFinished(Err(err)) => {
                    log::error!("fastboot-tcp {}: swupdate failed during download: {err}", self.peer);
                    let _ = self.transport.send_packet(FAIL_EPIPE).await;
                    self.sink.abort().await;
                    return Err(err);
                }
            }
        }

        self.transport.send_packet(OKAY).await
    }

    async fn handle_flash(&mut self, target: FlashTarget) -> io::Result<()> {
        if !self.fastboot_pending_flash {
            return self.transport.send_packet(FAIL_FLASH).await;
        }
        let label = match target {
            FlashTarget::Update => "update",
            FlashTarget::Swu => "swu",
        };
        log::info!("fastboot-tcp {}: flash {label}", self.peer);
        self.transport.send_packet(INFO_WAIT_SWUPDATE).await?;
        self.finish_download().await
    }

    /// Closes the SWUpdate feed (EOF), waits for the install verdict, resets the
    /// session for any subsequent transfer, and replies OKAY / FAIL.
    async fn finish_download(&mut self) -> io::Result<()> {
        if !self.sink.is_open() {
            self.fastboot_pending_flash = false;
            return self.transport.send_packet(FAIL_CLOSE).await;
        }

        self.sink.eof();
        let verdict = self.sink.finished().await;
        self.reset_after_finish();

        match verdict {
            Ok(()) => self.transport.send_packet(OKAY).await,
            Err(err) => {
                log::error!("fastboot-tcp {}: swupdate finish failed: {err}", self.peer);
                self.transport.send_packet(FAIL_CLOSE).await
            }
        }
    }

    fn reset_after_finish(&mut self) {
        self.sink = SwupdateSession::new(self.params.clone(), RECV_BUFFER_SIZE);
        self.fastboot_usb_session_open = false;
        self.fastboot_pending_flash = false;
    }
}

struct FastbootDownloadSource<'a, S> {
    transport: &'a mut FastbootFraming<S>,
    remaining: usize,
    frame_remaining: usize,
}

impl<'a, S> FastbootDownloadSource<'a, S> {
    fn new(transport: &'a mut FastbootFraming<S>, total: usize) -> Self {
        Self {
            transport,
            remaining: total,
            frame_remaining: 0,
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> FastbootDownloadSource<'_, S> {
    async fn read_block(&mut self, sink: &mut SwupdateSession) -> io::Result<Option<BytesMut>> {
        if self.remaining == 0 {
            return Ok(None);
        }

        if self.frame_remaining == 0 {
            let Some(frame_len) = self.transport.read_header().await? else {
                return Err(io::Error::new(ErrorKind::UnexpectedEof, "connection closed mid-download"));
            };
            self.frame_remaining = frame_len as usize;
        }

        let want = self.frame_remaining.min(self.remaining).min(RECV_BUFFER_SIZE);
        let mut buf = sink.buffer(want);
        buf.resize(want, 0);
        let read = self.transport.read_into(&mut buf).await?;
        if read == 0 {
            return Err(io::Error::new(ErrorKind::UnexpectedEof, "connection closed mid-download"));
        }
        buf.truncate(read);
        self.frame_remaining -= read;
        self.remaining -= read;
        Ok(Some(buf))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> SwupdatePumpSource for FastbootDownloadSource<'_, S> {
    fn next_block<'a>(&'a mut self, sink: &'a mut SwupdateSession) -> NextSwupdateBlock<'a> {
        Box::pin(self.read_block(sink))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

    async fn client_handshake(client: &mut DuplexStream) {
        client.write_all(b"FB01").await.unwrap();
        let mut buf = [0u8; 4];
        let _ = client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"FB01");
    }

    async fn send_frame(client: &mut DuplexStream, data: &[u8]) {
        client.write_all(&(data.len() as u64).to_be_bytes()).await.unwrap();
        client.write_all(data).await.unwrap();
    }

    async fn recv_frame(client: &mut DuplexStream) -> Vec<u8> {
        let mut header = [0u8; 8];
        let _ = client.read_exact(&mut header).await.unwrap();
        let len = u64::from_be_bytes(header) as usize;
        let mut buf = vec![0u8; len];
        let _ = client.read_exact(&mut buf).await.unwrap();
        buf
    }

    #[tokio::test]
    async fn handshake_and_getvar_over_tcp() {
        let (server, mut client) = tokio::io::duplex(4096);
        let session = TcpFastbootSession::new(
            FastbootFraming::new(server, None),
            SwupdateParams::default(),
            "abc123".to_string(),
            "test".to_string(),
        );
        let server_task = tokio::spawn(session.run());

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
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn rejects_malformed_handshake() {
        let (server, mut client) = tokio::io::duplex(64);
        let session = TcpFastbootSession::new(
            FastbootFraming::new(server, None),
            SwupdateParams::default(),
            "abc123".to_string(),
            "test".to_string(),
        );
        let server_task = tokio::spawn(session.run());

        // Read (and ignore) the device handshake, then send a bad one.
        let mut buf = [0u8; 4];
        let _ = client.read_exact(&mut buf).await.unwrap();
        client.write_all(b"XX01").await.unwrap();
        drop(client);

        let result = server_task.await.unwrap();
        assert!(result.is_err(), "malformed handshake should fail the session");
    }

    #[tokio::test]
    async fn idle_connection_times_out() {
        let (server, mut client) = tokio::io::duplex(64);
        let session = TcpFastbootSession::new(
            FastbootFraming::new(server, Some(Duration::from_millis(50))),
            SwupdateParams::default(),
            "abc123".to_string(),
            "test".to_string(),
        );
        let server_task = tokio::spawn(session.run());

        // Complete the handshake, then stay idle without sending a command.
        client_handshake(&mut client).await;

        let err = server_task
            .await
            .unwrap()
            .expect_err("an idle connection should time out");
        assert_eq!(err.kind(), ErrorKind::TimedOut);
    }
}
