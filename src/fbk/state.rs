//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! FBK command/data state machine.
//!
//! Drives one bound FBK function: it services control events, keeps the bulk
//! OUT receive queue fed, and interprets the FBK / fastboot command stream,
//! forwarding uploaded firmware into the shared download session.

use std::io;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::time::timeout;
use usb_gadget::function::custom::{Custom, EndpointReceiver, EndpointSender, Event};

use crate::functionfs::EventHandler;
use crate::swupdate::SwupdateSink;

use super::protocol::{
    fastboot_getvar_reply, parse_command, send_data_header, send_static, DownloadKind,
    FetchTarget, FlashTarget, ParsedCommand, FAIL_BADSIZE, FAIL_CLOSE, FAIL_CMD,
    FAIL_EPIPE, FAIL_FLASH, FAIL_NOTOPEN, FAIL_OPEN, FAIL_UNKNOWN_PART,
    INFO_WAIT_SWUPDATE, OKAY, split_command,
};
use super::recv_queue::RecvQueue;
use super::RECV_BUFFER_SIZE;

// Host-side fastboot transfers can legitimately pause for long stretches while
// upstream tooling or the target pipeline catches up, so this guard must only
// catch true hangs rather than normal minute-scale stalls.
const DOWNLOAD_STALL_TIMEOUT: Duration = Duration::from_secs(180);
#[derive(Debug)]
pub(super) struct FbkState {
    recv: RecvQueue,
    tx: EndpointSender,
    download: SwupdateSink,
    serial: String,
    download_size: usize,
    downloaded_size: usize,
    fastboot_pending_flash: bool,
}

impl EventHandler for FbkState {
    async fn handle_event(&mut self, udc_name: &str, event: Event<'_>) -> io::Result<()> {
        match event {
            Event::Enable => {
                log::info!("[{udc_name}] FBK function enabled");
            }
            Event::Disable => {
                log::info!("[{udc_name}] FBK function disabled");
                self.download.abort().await;
                self.reset_download_tracking();
                self.recv.cancel(udc_name);
            }
            Event::SetupHostToDevice(req) => {
                let _ = req.recv_all_async().await;
            }
            Event::SetupDeviceToHost(req) => {
                let _ = req.send_async(&[]).await;
            }
            _ => {}
        }

        Ok(())
    }
}

impl FbkState {
    pub(super) fn new(
        rx: EndpointReceiver,
        tx: EndpointSender,
        download: SwupdateSink,
        serial: String,
    ) -> Self {
        Self {
            recv: RecvQueue::new(rx, RECV_BUFFER_SIZE),
            tx,
            download,
            serial,
            download_size: 0,
            downloaded_size: 0,
            fastboot_pending_flash: false,
        }
    }

    fn reset_download_tracking(&mut self) {
        self.download_size = 0;
        self.downloaded_size = 0;
        self.fastboot_pending_flash = false;
    }

    fn remaining_download_bytes(&self) -> usize {
        self.download_size.saturating_sub(self.downloaded_size)
    }

    fn download_active(&self) -> bool {
        self.downloaded_size < self.download_size
    }

    fn should_prime_recv(&self) -> bool {
        !self.download_active() || !self.download.should_throttle(RECV_BUFFER_SIZE)
    }

    fn prime_recv_if_room(&mut self, udc_name: &str) {
        if self.should_prime_recv() {
            let requested_capacity = if self.download_active() {
                self.remaining_download_bytes().min(RECV_BUFFER_SIZE)
            } else {
                RECV_BUFFER_SIZE
            };
            self.recv.prime_or_report(udc_name, requested_capacity);
        }
    }

    /// Run the command/data loop until the endpoint is torn down.
    ///
    /// `custom` is driven alongside the bulk receive queue: control events
    /// (enable/disable/setup) are serviced from `custom`, while firmware and
    /// commands arrive as bulk OUT completions on the receive queue.
    pub(super) async fn run(&mut self, udc_name: &str, custom: &mut Custom) {
        if let Err(err) = self.recv.prime(RECV_BUFFER_SIZE) {
            if !crate::functionfs::is_closed_transport_error(&err) {
                log::error!("[{udc_name}] FBK initial receive queue priming failed: {err}");
            }
        }

        loop {
            let download_active = self.download_active();
            let recv_inflight = self.recv.inflight();
            let wait_writable = download_active
                && recv_inflight == 0
                && self.download.should_throttle(RECV_BUFFER_SIZE);

            tokio::select! {
                biased;

                event_wait = custom.wait_event() => {
                    match event_wait {
                        Ok(()) => {}
                        Err(err) => {
                            if !crate::functionfs::is_closed_transport_error(&err) {
                                log::debug!("[{udc_name}] FBK wait_event error: {err}");
                            }
                            continue;
                        }
                    }

                    let event = match custom.event() {
                        Ok(event) => event,
                        Err(err) => {
                            if !crate::functionfs::is_closed_transport_error(&err) {
                                log::error!("[{udc_name}] FBK event error: {err}");
                            }
                            continue;
                        }
                    };

                    if let Err(err) = self.handle_event(udc_name, event).await {
                        if !crate::functionfs::is_closed_transport_error(&err) {
                            log::error!("[{udc_name}] FBK handle_event error: {err}");
                        }
                    }

                    // A freshly enabled function needs its receive queue primed so the
                    // kernel starts accepting host data immediately.
                    self.prime_recv_if_room(udc_name);
                }

                data_res = async {
                    if download_active {
                        timeout(DOWNLOAD_STALL_TIMEOUT, self.recv.fetch())
                            .await
                            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "timed out waiting for FBK download data"))?
                    } else {
                        self.recv.fetch().await
                    }
                }, if recv_inflight > 0 => {
                    match data_res {
                        Ok(Some(chunk)) => {
                            self.recv.on_completed();

                            let empty_completion = chunk.is_empty();
                            self.handle_rx_chunk(udc_name, chunk).await;

                            // A capacity switch (command -> data phase) or buffer recycle
                            // may have freed queue slots; refill them only if
                            // the SWUpdate queue still has room for another full
                            // receive buffer.
                            self.prime_recv_if_room(udc_name);

                            if empty_completion {
                                if let Err(err) = self.download.poll_progress().await {
                                    if crate::functionfs::is_closed_transport_error(&err) {
                                        self.abort_and_halt_inflight_download(udc_name, &format!("SWUpdate progress polling failed during zero-length completion: {err}"))
                                            .await;
                                    } else {
                                        log::debug!("[{udc_name}] FBK poll_progress after zero-length completion failed: {err}");
                                    }
                                }
                                tokio::task::yield_now().await;
                            }
                        }
                        Ok(None) => {
                            // Nothing was actually queued; resync the in-flight count and
                            // try to prime again.
                            self.recv.reset_inflight();
                            self.prime_recv_if_room(udc_name);
                        }
                        Err(err) => {
                            if err.kind() == io::ErrorKind::TimedOut {
                                self.fail_and_recover_inflight_download(
                                    udc_name,
                                    &format!("timed out waiting for remaining FBK/fastboot download bytes: {err}"),
                                    FAIL_BADSIZE,
                                ).await;
                                continue;
                            }

                            self.recv.on_completed();
                            if crate::functionfs::is_closed_transport_error(&err) {
                                self.abort_and_halt_inflight_download(udc_name, &format!("bulk OUT transport closed while waiting for receive completion: {err}"))
                                    .await;
                            } else {
                                log::debug!("[{udc_name}] FBK bulk recv error: {err}");
                                self.prime_recv_if_room(udc_name);
                            }
                        }
                    }
                }

                writable_res = self.download.wait_writable(), if wait_writable => {
                    match writable_res {
                        Ok(()) => {
                            self.prime_recv_if_room(udc_name);
                        }
                        Err(err) => {
                            if crate::functionfs::is_closed_transport_error(&err) {
                                self.abort_and_halt_inflight_download(
                                    udc_name,
                                    &format!("SWUpdate writable wait failed after FBK backpressure pause: {err}")
                                ).await;
                            } else {
                                log::debug!("[{udc_name}] FBK writable wait failed: {err}");
                            }
                        }
                    }
                }
            }
        }
    }

    async fn abort_and_halt_inflight_download(&mut self, udc_name: &str, reason: &str) {
        if !self.download_active() && !self.fastboot_pending_flash && !self.download.is_active() {
            return;
        }

        log::warn!("[{udc_name}] aborting in-flight FBK/fastboot download: {reason}");
        self.download.abort().await;
        self.reset_download_tracking();
        self.recv.cancel_and_halt(udc_name);
    }

    async fn fail_and_recover_inflight_download(&mut self, udc_name: &str, reason: &str, reply: &'static [u8]) {
        if !self.download_active() && !self.fastboot_pending_flash && !self.download.is_active() {
            return;
        }

        log::warn!("[{udc_name}] failing in-flight FBK/fastboot download: {reason}");
        self.download.abort().await;
        self.reset_download_tracking();

        // A stall timeout means the host stopped feeding the declared payload;
        // abort the current transfer and re-prime the OUT queue so we can send
        // a protocol failure and accept a fresh command instead of wedging the
        // client behind a halted endpoint.
        self.recv.cancel(udc_name);
        self.prime_recv_if_room(udc_name);
        let _ = send_static(&mut self.tx, reply).await;
    }

    async fn handle_rx_chunk(&mut self, udc_name: &str, mut chunk: BytesMut) {
        loop {
            if self.download_active() {
                self.handle_download_chunk(udc_name, chunk).await;
                return;
            }

            if !self.handle_command_chunk(udc_name, &mut chunk).await {
                self.recv.recycle(chunk);
                return;
            }
        }
    }

    async fn handle_download_chunk(&mut self, udc_name: &str, chunk: BytesMut) {
        if chunk.is_empty() {
            self.recv.recycle(chunk);
            return;
        }

        match self.download.write_block_if_active(chunk).await {
            Ok(queued) => {
                self.downloaded_size = self.downloaded_size.saturating_add(queued);

                if self.downloaded_size >= self.download_size {
                    log::warn!("[{udc_name}] FBK download data phase complete");
                    let _ = send_static(&mut self.tx, OKAY).await;
                }
            }
            Err(err) => {
                if err.kind() == io::ErrorKind::NotConnected {
                    let _ = send_static(&mut self.tx, FAIL_NOTOPEN).await;
                } else {
                    log::error!("[{udc_name}] FBK write to SWUpdate failed: {err}");
                    let _ = send_static(&mut self.tx, FAIL_EPIPE).await;
                }
                self.download.abort().await;
                self.reset_download_tracking();
                self.recv.cancel_and_halt(udc_name);
            }
        }
    }

    async fn begin_download_command(&mut self, udc_name: &str, label: &str) -> bool {
        if let Err(err) = self.download.begin().await {
            log::error!("[{udc_name}] {label} begin failed: {err}");
            let _ = send_static(&mut self.tx, FAIL_OPEN).await;
            return false;
        }
        true
    }

    async fn finish_download_command(&mut self, udc_name: &str, label: &str) -> bool {
        if let Err(err) = self.download.finish().await {
            log::error!("[{udc_name}] {label} failed: {err}");
            let _ = send_static(&mut self.tx, FAIL_CLOSE).await;
            return false;
        }
        true
    }

    async fn handle_wopen_command(&mut self, udc_name: &str) -> bool {
        if !self.begin_download_command(udc_name, "FBK WOpen").await {
            return false;
        }

        self.fastboot_pending_flash = false;
        log::warn!("[{udc_name}] FBK reply tx: OKAY (WOpen)");
        let _ = send_static(&mut self.tx, OKAY).await;
        true
    }

    async fn handle_fetch_command(&mut self, udc_name: &str, target: FetchTarget) -> bool {
        let payload = crate::sysinfo::SystemInfo::collect(Some(self.serial.clone())).to_json_bytes();
        let len = payload.len();
        let target = match target {
            FetchTarget::Sysinfo => "sysinfo",
            FetchTarget::SysinfoJson => "sysinfo.json",
        };
        log::warn!("[{udc_name}] fastboot reply tx: DATA{:08X} (fetch {target})", len);
        if send_data_header(&mut self.tx, len).await.is_err() {
            return false;
        }
        if self.tx.send_async(Bytes::from(payload)).await.is_err() {
            return false;
        }
        log::warn!("[{udc_name}] fastboot reply tx: OKAY (fetch)");
        let _ = send_static(&mut self.tx, OKAY).await;
        true
    }

    async fn start_download_transfer(&mut self, udc_name: &str, len: usize, kind: DownloadKind) -> bool {
        if !self.download.is_active() && !self.begin_download_command(udc_name, "FBK/fastboot").await {
            return false;
        }
        if send_data_header(&mut self.tx, len).await.is_err() {
            self.download.abort().await;
            self.reset_download_tracking();
            return false;
        }

        self.download_size = len;
        self.downloaded_size = 0;
        self.fastboot_pending_flash = matches!(kind, DownloadKind::Fastboot);
        log::warn!(
            "[{udc_name}] {} reply tx: DATA{:08X} (expecting {} bytes)",
            if self.fastboot_pending_flash { "fastboot" } else { "FBK" },
            len,
            len
        );
        if len == 0 {
            self.download_size = 0;
            self.downloaded_size = 0;
            log::warn!("[{udc_name}] reply tx: OKAY (zero-length download)");
            let _ = send_static(&mut self.tx, OKAY).await;
        }
        true
    }

    async fn finish_flash_download(&mut self, udc_name: &str, target: FlashTarget) -> bool {
        if !self.fastboot_pending_flash {
            let _ = send_static(&mut self.tx, FAIL_FLASH).await;
            return false;
        }
        if self.download_active() {
            let _ = send_static(&mut self.tx, FAIL_BADSIZE).await;
            return false;
        }

        let target = match target {
            FlashTarget::Update => "update",
            FlashTarget::Swu => "swu",
        };
        log::warn!("[{udc_name}] fastboot flash command received: {target}");
        let _ = send_static(&mut self.tx, INFO_WAIT_SWUPDATE).await;
        if !self.finish_download_command(udc_name, "fastboot flash/finish").await {
            self.fastboot_pending_flash = false;
            return false;
        }

        self.fastboot_pending_flash = false;
        log::warn!("[{udc_name}] fastboot reply tx: OKAY (flash)");
        let _ = send_static(&mut self.tx, OKAY).await;
        true
    }

    async fn handle_close_command(&mut self, udc_name: &str) -> bool {
        if !self.finish_download_command(udc_name, "FBK close/finish").await {
            return false;
        }

        self.reset_download_tracking();
        log::warn!("[{udc_name}] FBK reply tx: OKAY (Close)");
        let _ = send_static(&mut self.tx, OKAY).await;
        true
    }

    async fn handle_command_chunk(&mut self, udc_name: &str, chunk: &mut BytesMut) -> bool {
        let Some((cmd, consumed)) = split_command(chunk.as_ref()) else {
            return false;
        };
        let cmd = cmd.to_vec();
        let _ = chunk.split_to(consumed);

        log::warn!("[{udc_name}] FBK command rx: {}", String::from_utf8_lossy(&cmd));

        match parse_command(&cmd) {
            ParsedCommand::WOpen => self.handle_wopen_command(udc_name).await,
            ParsedCommand::GetVar => {
                if let Some(reply) = fastboot_getvar_reply(&cmd, &self.serial) {
                    log::warn!("[{udc_name}] fastboot reply tx: {}", String::from_utf8_lossy(&reply));
                    let _ = self.tx.send_async(Bytes::from(reply)).await;
                    true
                } else {
                    log::warn!("[{udc_name}] unsupported FBK command: {}", String::from_utf8_lossy(&cmd));
                    let _ = send_static(&mut self.tx, FAIL_CMD).await;
                    false
                }
            }
            ParsedCommand::Fetch(target) => self.handle_fetch_command(udc_name, target).await,
            ParsedCommand::FetchUnknownPart => {
                let _ = send_static(&mut self.tx, FAIL_UNKNOWN_PART).await;
                false
            }
            ParsedCommand::Download { len, kind } => {
                self.start_download_transfer(udc_name, len, kind).await
            }
            ParsedCommand::Flash(target) => self.finish_flash_download(udc_name, target).await,
            ParsedCommand::FlashUnknownPart => {
                let _ = send_static(&mut self.tx, FAIL_UNKNOWN_PART).await;
                false
            }
            ParsedCommand::Close => self.handle_close_command(udc_name).await,
            ParsedCommand::Unsupported => {
                log::warn!("[{udc_name}] unsupported FBK command: {}", String::from_utf8_lossy(&cmd));
                let _ = send_static(&mut self.tx, FAIL_CMD).await;
                false
            }
        }
    }
}
