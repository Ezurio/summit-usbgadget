//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! fastboot-usb command/data state machine.
//!
//! Drives one bound fastboot-usb function: it services control events, keeps the bulk
//! OUT receive queue fed, and interprets the fastboot-usb / fastboot command stream,
//! forwarding uploaded firmware into the shared download session.

use std::io;

use bytes::BytesMut;
use summit_usbgadget_swupdate::{Feed, SwupdateParams, SwupdateSession};
use tokio::time::timeout;
use usb_gadget::function::custom::{Custom, EndpointReceiver, EndpointSender, Event};

use crate::functionfs::EventHandler;

use super::commands;
use super::functionfs::send_static;
use super::{
    EndpointAction, FastbootUsbState, DOWNLOAD_STALL_TIMEOUT, FAIL_BADSIZE, FAIL_CLOSE,
    FAIL_EPIPE, FAIL_NOTOPEN, OKAY, RECV_BUFFER_SIZE,
};

impl EventHandler for FastbootUsbState {
    async fn handle_event(&mut self, udc_name: &str, event: Event<'_>) -> io::Result<()> {
        match event {
            Event::Enable => {
                log::info!("[{udc_name}] fastboot-usb function enabled");
            }
            Event::Disable => {
                log::info!("[{udc_name}] fastboot-usb function disabled");
                self.reset(udc_name, EndpointAction::Cancel, None).await;
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

impl FastbootUsbState {
    pub(super) fn new(
        mut rx: EndpointReceiver,
        tx: EndpointSender,
        download: SwupdateParams,
        serial: String,
    ) -> Self {
        Self {
            rx_max_packet_size: rx.max_packet_size().unwrap_or(512),
            rx,
            tx,
            download_params: download.clone(),
            download: Some(SwupdateSession::new(download, RECV_BUFFER_SIZE)),
            fastboot_usb_session_open: false,
            serial,
            download_size: 0,
            downloaded_size: 0,
            fastboot_pending_flash: false,
            finish_pending: false,
        }
    }

    fn reset_state(&mut self) {
        self.download = Some(SwupdateSession::new(self.download_params.clone(), RECV_BUFFER_SIZE));
        self.fastboot_usb_session_open = false;
        self.download_size = 0;
        self.downloaded_size = 0;
        self.fastboot_pending_flash = false;
        self.finish_pending = false;
    }

    pub(super) async fn reset(
        &mut self,
        udc_name: &str,
        endpoint_action: EndpointAction,
        reply: Option<&'static [u8]>,
    ) {
        if let Some(download) = self.download.as_mut() {
            download.abort().await;
        }
        self.reset_state();
        match endpoint_action {
            EndpointAction::Cancel => {
                if let Err(err) = self.rx.cancel() {
                    if !crate::functionfs::is_closed_transport_error(&err) {
                        log::debug!("[{udc_name}] fastboot-usb receive request cancel failed: {err}");
                    }
                }
            }
            EndpointAction::Halt => {
                if let Err(err) = self.rx.cancel() {
                    if !crate::functionfs::is_closed_transport_error(&err) {
                        log::debug!("[{udc_name}] fastboot-usb receive request cancel failed: {err}");
                    }
                }
                match self.rx.control() {
                    Ok(ctrl) => {
                        if let Err(err) = ctrl.halt() {
                            log::debug!("[{udc_name}] fastboot-usb receive endpoint halt failed: {err}");
                        }
                    }
                    Err(err) => {
                        log::debug!("[{udc_name}] fastboot-usb receive endpoint control unavailable for halt: {err}");
                    }
                }
            }
        }
        if let Some(reply) = reply {
            let _ = send_static(&mut self.tx, reply).await;
        }
    }

    fn remaining_download_bytes(&self) -> usize {
        self.download_size.saturating_sub(self.downloaded_size)
    }

    pub(super) fn download_active(&self) -> bool {
        self.downloaded_size < self.download_size
    }

    /// Borrows a buffer for the next bulk-OUT read.
    ///
    /// Normally a full `RECV_BUFFER_SIZE` buffer, recycled from the download
    /// pool. During a download the tail is usually shorter than that, so the
    /// read is clamped to the remaining bytes (rounded up to the max packet
    /// size). The submitted read length equals the buffer capacity, so this
    /// makes the final read fill exactly at the download boundary — completing
    /// even when the last block is an exact multiple of the packet size and the
    /// host sends no terminating short packet. The short tail buffer is a small
    /// fresh allocation (a recycled buffer would be oversized and defeat this).
    fn recv_buffer(&mut self) -> BytesMut {
        let capacity = self.recv_capacity();
        if capacity >= RECV_BUFFER_SIZE {
            self
                .download
                .as_mut()
                .map(|download| download.buffer(RECV_BUFFER_SIZE))
                .unwrap_or_else(|| BytesMut::with_capacity(RECV_BUFFER_SIZE))
        } else {
            BytesMut::with_capacity(capacity)
        }
    }

    /// Length of the next bulk-OUT read, clamped to the remaining download and
    /// rounded up to the max packet size (a hardware requirement for OUT reads).
    fn recv_capacity(&self) -> usize {
        if !self.download_active() {
            return RECV_BUFFER_SIZE;
        }
        let remaining = self.remaining_download_bytes().clamp(1, RECV_BUFFER_SIZE);
        let mps = self.rx_max_packet_size.max(1);
        remaining.div_ceil(mps) * mps
    }

    /// Non-blocking observation of the finish signal. `None` = still installing.
    fn finish_ready(&mut self) -> Option<io::Result<()>> {
        match self.download.as_mut() {
            Some(download) => download.try_finished(),
            None => Some(Ok(())),
        }
    }

    /// Run the command/data loop until the endpoint is torn down.
    ///
    /// `custom` is driven alongside the bulk receive queue: control events
    /// (enable/disable/setup) are serviced from `custom`, while firmware and
    /// commands arrive as bulk OUT completions on the receive queue.
    pub(super) async fn run(&mut self, udc_name: &str, custom: &mut Custom) {
        loop {
            if self.finish_pending {
                match self.finish_ready() {
                    Some(Ok(())) => {
                        self.reset_state();
                        log::warn!("[{udc_name}] finish reply tx: OKAY");
                        let _ = send_static(&mut self.tx, OKAY).await;
                        continue;
                    }
                    None => {}
                    Some(Err(err)) => {
                        log::error!("[{udc_name}] SWUpdate finish wait failed: {err}");
                        self.reset_state();
                        let _ = send_static(&mut self.tx, FAIL_CLOSE).await;
                        continue;
                    }
                }
            } else if self.download_active() {
                // A swupdate failure is a failure: report it to fastboot
                // immediately, even mid-download. Early success is cached and
                // only acted on once the whole download has been consumed.
                let failed = match self.download.as_mut() {
                    Some(download) if download.is_open() => download.try_finished().and_then(Result::err),
                    _ => None,
                };
                if let Some(err) = failed {
                    log::error!("[{udc_name}] swupdate failed during download: {err}");
                    self.reset(udc_name, EndpointAction::Halt, Some(FAIL_EPIPE)).await;
                    continue;
                }
            }

            // Keep exactly one bulk-OUT buffer primed. We never prime a second
            // one, so once handle_rx_chunk blocks on a full download queue the
            // host is naturally NAKed until a block drains.
            if self.rx.is_empty() {
                let buf = self.recv_buffer();
                if let Err(err) = self.rx.try_recv(buf) {
                    if !crate::functionfs::is_closed_transport_error(&err) {
                        log::debug!("[{udc_name}] fastboot-usb reprime error: {err}");
                    }
                }
            }

            let download_active = self.download_active();
            let wait_swupdate = self.finish_pending;

            tokio::select! {
                biased;

                control_res = async {
                    custom.wait_event().await?;
                    custom.event()
                } => {
                    let event = match control_res {
                        Ok(event) => event,
                        Err(err) => {
                            if crate::functionfs::is_closed_transport_error(&err) {
                                if self.download_active() || self.fastboot_pending_flash || self.finish_pending {
                                    log::warn!(
                                        "[{udc_name}] aborting in-flight fastboot-usb/fastboot download: control transport closed while waiting for event: {err}"
                                    );
                                    self.reset(udc_name, EndpointAction::Halt, None).await;
                                }
                            } else {
                                log::debug!("[{udc_name}] fastboot-usb wait_event error: {err}");
                            }
                            continue;
                        }
                    };

                    if let Err(err) = self.handle_event(udc_name, event).await {
                        if !crate::functionfs::is_closed_transport_error(&err) {
                            log::error!("[{udc_name}] fastboot-usb handle_event error: {err}");
                        }
                    }
                }

                data_res = async {
                    if download_active {
                        timeout(DOWNLOAD_STALL_TIMEOUT, self.rx.fetch_async())
                            .await
                            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "timed out waiting for fastboot-usb download data"))?
                    } else {
                        self.rx.fetch_async().await
                    }
                }, if !wait_swupdate => {
                    match data_res {
                        Ok(Some(chunk)) => {
                            let empty_completion = chunk.is_empty();

                            // Handle at most one bulk completion per iteration so
                            // control events and SWUpdate status are not starved.
                            self.handle_rx_chunk(udc_name, chunk).await;

                            if empty_completion {
                                tokio::task::yield_now().await;
                            }
                        }
                        Ok(None) => {}
                        Err(err) => {
                            if err.kind() == io::ErrorKind::TimedOut {
                                if self.download_active() || self.fastboot_pending_flash || self.finish_pending {
                                    log::warn!(
                                        "[{udc_name}] failing in-flight fastboot-usb/fastboot download: timed out waiting for remaining fastboot-usb/fastboot download bytes: {err}"
                                    );
                                    self.reset(udc_name, EndpointAction::Cancel, Some(FAIL_BADSIZE)).await;
                                }
                                continue;
                            }

                            if crate::functionfs::is_closed_transport_error(&err) {
                                if self.download_active() || self.fastboot_pending_flash || self.finish_pending {
                                    log::warn!(
                                        "[{udc_name}] aborting in-flight fastboot-usb/fastboot download: bulk OUT transport closed while waiting for receive completion: {err}"
                                    );
                                    self.reset(udc_name, EndpointAction::Halt, None).await;
                                }
                            } else {
                                log::debug!("[{udc_name}] fastboot-usb bulk recv error: {err}");
                            }
                        }
                    }
                }

                status = async {
                    match self.download.as_mut() {
                        Some(download) => download.finished().await,
                        None => Ok(()),
                    }
                }, if wait_swupdate => {
                    match status {
                        Ok(()) if self.finish_pending => {
                            self.reset_state();
                            log::warn!("[{udc_name}] finish reply tx: OKAY");
                            let _ = send_static(&mut self.tx, OKAY).await;
                        }
                        Ok(()) => {}
                        Err(err) => {
                            if self.finish_pending {
                            log::error!("[{udc_name}] SWUpdate finish wait failed: {err}");
                            self.reset_state();
                            let _ = send_static(&mut self.tx, FAIL_CLOSE).await;
                            } else {
                                log::error!("[{udc_name}] SWUpdate wait failed: {err}");
                                self.reset(udc_name, EndpointAction::Halt, Some(FAIL_EPIPE)).await;
                            }
                        }
                    }
                }
            }
        }
    }

    async fn handle_rx_chunk(&mut self, udc_name: &str, mut chunk: BytesMut) {
        if !self.download_active() {
            let handled = commands::handle_command_chunk(self, udc_name, &mut chunk).await;
            if handled && self.download_active() && !chunk.is_empty() {
                let _ = self.handle_download_chunk(udc_name, chunk).await;
            }
            return;
        }

        let remaining = self.remaining_download_bytes();
        if chunk.len() > remaining {
            let trailing = chunk.split_off(remaining);
            let boundary_complete = self.handle_download_chunk(udc_name, chunk).await;
            if boundary_complete {
                chunk = trailing;
            } else {
                return;
            }
        } else {
            let _ = self.handle_download_chunk(udc_name, chunk).await;
            return;
        }

        let _ = self.handle_download_chunk(udc_name, chunk).await;
    }

    async fn handle_download_chunk(&mut self, udc_name: &str, chunk: BytesMut) -> bool {
        if chunk.is_empty() {
            return false;
        }

        // Bytes consumed off the USB endpoint. Whether swupdate accepts them or
        // has already finished, the host is a dumb file-transfer tool that will
        // send the whole download; we must consume every byte and only ack at
        // the real boundary. Once swupdate is done (`Sent::Done`) we simply keep
        // draining and discarding.
        let chunk_len = chunk.len();

        let Some(download) = self.download.as_mut() else {
            self.reset(udc_name, EndpointAction::Halt, Some(FAIL_NOTOPEN)).await;
            return false;
        };

        let outcome = match download.try_send(chunk).await {
            Ok(Feed::Ok(_) | Feed::Closed) => Ok(()),
            Ok(Feed::Full(chunk)) => match download.send(chunk).await {
                Ok(Feed::Ok(_) | Feed::Closed) => Ok(()),
                Ok(Feed::Full(_)) => unreachable!("send awaits capacity"),
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        };

        match outcome {
            Ok(()) => {
                self.downloaded_size = self.downloaded_size.saturating_add(chunk_len);

                if self.downloaded_size >= self.download_size {
                    log::warn!("[{udc_name}] fastboot-usb download data phase complete");
                    let _ = send_static(&mut self.tx, OKAY).await;
                    true
                } else {
                    false
                }
            }
            Err(err) => {
                if err.kind() == io::ErrorKind::NotConnected {
                    let _ = send_static(&mut self.tx, FAIL_NOTOPEN).await;
                } else {
                    log::error!("[{udc_name}] fastboot-usb write to SWUpdate failed: {err}");
                    let _ = send_static(&mut self.tx, FAIL_EPIPE).await;
                }
                self.reset(udc_name, EndpointAction::Halt, None).await;
                false
            }
        }
    }
}
