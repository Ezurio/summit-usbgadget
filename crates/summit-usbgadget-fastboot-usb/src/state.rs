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
use usb_gadget::function::custom::{EndpointReceiver, EndpointSender, Event};

use summit_usbgadget_usb::functionfs::{is_closed_transport_error, EventHandler};
use tokio::sync::watch;

use super::commands;
use crate::send_static;
use super::{
    FastbootUsbState, DOWNLOAD_STALL_TIMEOUT, FAIL_BADSIZE, FAIL_CLOSE,
    FAIL_EPIPE, FAIL_NOTOPEN, OKAY, RECV_BUFFER_SIZE,
};

/// Endpoint-zero callback for the fastboot-usb control plane, plugged into the
/// generic [`functionfs::serve`] loop shared with DFU.
///
/// It owns no USB endpoint of its own; it just translates each control event
/// into the `enabled` signal the data loop observes (`Enable` → `true`,
/// `Disable` → `false`) and services setup requests inline. The data loop reacts
/// to the signal in its own iteration, so control processing never blocks on
/// bulk I/O and vice versa.
///
/// [`functionfs::serve`]: summit_usbgadget_usb::functionfs::serve
pub(super) struct FastbootControl {
    enabled: watch::Sender<bool>,
}

impl FastbootControl {
    pub(super) fn new(enabled: watch::Sender<bool>) -> Self {
        Self { enabled }
    }
}

impl EventHandler for FastbootControl {
    async fn handle_event(&mut self, udc_name: &str, event: Event<'_>) -> io::Result<()> {
        match event {
            Event::Enable => {
                log::info!("[{udc_name}] fastboot-usb function enabled");
                let _ = self.enabled.send(true);
            }
            Event::Disable => {
                log::info!("[{udc_name}] fastboot-usb function disabled");
                let _ = self.enabled.send(false);
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
        rx: EndpointReceiver,
        tx: EndpointSender,
        download: SwupdateParams,
        serial: String,
        rx_max_packet_size_default: usize,
    ) -> Self {
        Self {
            // Not queried here: the endpoint must not be touched at all until
            // the function is actually enabled by a host (see `serve_connected`,
            // which refreshes this before driving the bulk path).
            rx_max_packet_size: rx_max_packet_size_default,
            rx_max_packet_size_default,
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
            gadget_torn_down: false,
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

    /// Quiesces the bulk receive path, following the IO teardown order
    /// required for any IO interface (sockets, files, DMA, USB endpoints
    /// alike): disable the producer side first — cancel the receive endpoint
    /// so no new and no already-queued data can be delivered — then discard
    /// the result and reset local session state. Never the other order
    /// (destroying local buffers/session state before the endpoint is
    /// quiesced), which is a use-after-free/race.
    ///
    /// Every hard-failure caller (stall timeout, unrecoverable recv error,
    /// mid-download swupdate failure, transport close) pairs this with exiting
    /// `serve_connected` entirely instead of looping back — see
    /// [`serve_connected`](Self::serve_connected).
    pub(super) async fn reset(&mut self, udc_name: &str, reply: Option<&'static [u8]>) {
        if let Err(err) = self.rx.cancel() {
            log::debug!("[{udc_name}] fastboot-usb receive endpoint cancel failed: {err}");
        }

        if let Some(download) = self.download.as_mut() {
            download.abort().await;
        }
        self.reset_state();

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

    /// Whether a download/flash/finish is mid-flight, so a transport error must
    /// tear it down rather than be ignored.
    fn in_flight(&self) -> bool {
        self.download_active() || self.fastboot_pending_flash || self.finish_pending
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

    /// Bulk data-plane loop, run independently of the control loop.
    ///
    /// Two separate loops, not one loop with a branch: [`wait_for_enabled`]
    /// is a free function with no access to `self` (and therefore no way to
    /// touch a USB endpoint even by accident) that does nothing but wait for
    /// the control loop to report the function enabled. Only once it returns
    /// `true` does this hand off to [`serve_connected`], which owns all bulk
    /// USB I/O for as long as the function stays enabled. When
    /// `serve_connected` returns (disabled, or the control loop ended), this
    /// drops straight back to `wait_for_enabled` — the bulk path is never
    /// touched in between.
    ///
    /// [`serve_connected`]: FastbootUsbState::serve_connected
    pub(super) async fn data_loop(&mut self, udc_name: &str, mut enabled: watch::Receiver<bool>) {
        loop {
            if !wait_for_enabled(&mut enabled).await {
                return; // control loop ended
            }
            self.serve_connected(udc_name, &mut enabled).await;
            if self.gadget_torn_down {
                // The endpoint is permanently gone (UDC unbind in progress),
                // not merely disabled pending a reconnect: re-checking `enabled`
                // would just observe a stale `true` left over from before
                // shutdown and immediately re-enter `serve_connected`, which
                // would submit yet another read into a driver that is actively
                // disabling the endpoint -- see `gadget_torn_down`'s doc comment
                // for why that hangs `RunningGadget::shutdown`. Stop this task
                // outright instead.
                return;
            }
        }
    }

    /// Connected phase: drives the bulk receive queue and the transfer state
    /// machine, watching the control loop's `enabled` signal. Returns as soon as
    /// the function is disabled, the control loop ends, *or* any hard failure
    /// occurs where retrying in place is not expected (stall timeout, transport
    /// close, mid-download SWUpdate failure, an unrecoverable bulk recv error).
    /// Every one of those paths already ran `reset()` (which quiesces the bulk
    /// path and best-effort replies FAIL) before returning; this function never
    /// loops back to re-arm the endpoint after a hard failure — the caller
    /// (`data_loop`) simply drops back to `wait_for_enabled`, i.e. the same
    /// as a real Disable, until the function is genuinely re-enabled.
    async fn serve_connected(&mut self, udc_name: &str, enabled: &mut watch::Receiver<bool>) {
        // First real touch of the bulk endpoint for this connection: only
        // reached once the control loop has reported the function enabled, so
        // querying the negotiated max packet size here (instead of eagerly in
        // `new`) can never run before a host is actually attached.
        self.rx_max_packet_size = self.rx.max_packet_size().unwrap_or(self.rx_max_packet_size_default);

        loop {
            // Fail fast on a mid-download swupdate error before priming a read,
            // so the reset never has to cancel a freshly-submitted (maybe
            // actively DMA-ing) buffer.
            if !self.finish_pending && self.download_active() {
                let failed = match self.download.as_mut() {
                    Some(download) if download.is_open() => download.try_finished().and_then(Result::err),
                    _ => None,
                };
                if let Some(err) = failed {
                    log::error!("[{udc_name}] swupdate failed during download: {err}");
                    self.reset(udc_name, Some(FAIL_EPIPE)).await;
                    return;
                }
            }

            // Prime one read before the select so `ready` reflects whether a
            // block (or an install verdict) is actually available to await;
            // otherwise the transfer arm would spin on an empty queue.
            self.submit_recv(udc_name);
            if self.gadget_torn_down {
                // submit_recv just saw the UDC being unbound: return immediately
                // rather than falling into the select, which would otherwise sit
                // parked on `enabled.changed()` (harmless, but pointless) until
                // the control loop separately notices the same teardown.
                return;
            }
            let ready = !self.rx.is_empty() || self.finish_pending;

            tokio::select! {
                biased;

                // The control loop signalled a state change. A transition to
                // disabled (or a dropped control loop) quiesces the bulk path and
                // leaves the connected loop immediately; a spurious wake with the
                // function still enabled just re-iterates.
                res = enabled.changed() => {
                    if res.is_err() || !*enabled.borrow() {
                        self.reset(udc_name, None).await;
                        return;
                    }
                }

                // Transfer step: await one block (or the install verdict) and
                // route it. Cancel-safe — if the control signal fires first this
                // is dropped with the read still queued, and the next iteration
                // re-awaits it (submit_recv is a no-op while one is in flight).
                // A `false` result means a hard failure already reset() and
                // tore the session down: close the endpoint and exit rather
                // than loop back and try to keep serving it.
                keep_serving = self.transfer_step(udc_name), if ready => {
                    if !keep_serving {
                        return;
                    }
                }
            }
        }
    }

    /// One step of the transfer state machine, driven as a select arm beside the
    /// control channel: await the install verdict while finishing, otherwise
    /// receive and route one block. Returns whether the connected session
    /// should keep serving: `false` means a hard failure already reset() and
    /// tore the session down, so the caller must close the bulk endpoint and
    /// exit `serve_connected` instead of looping back to re-arm it. A finish
    /// verdict (success or SWUpdate-reported failure) is a normal protocol
    /// end state, not a hard failure, and always returns `true`.
    async fn transfer_step(&mut self, udc_name: &str) -> bool {
        if self.finish_pending {
            let status = match self.download.as_mut() {
                Some(download) => download.finished().await,
                None => Ok(()),
            };
            self.reset_state();
            match status {
                Ok(()) => {
                    log::warn!("[{udc_name}] finish reply tx: OKAY");
                    let _ = send_static(&mut self.tx, OKAY).await;
                }
                Err(err) => {
                    log::error!("[{udc_name}] SWUpdate finish wait failed: {err}");
                    let _ = send_static(&mut self.tx, FAIL_CLOSE).await;
                }
            }
            return true;
        }

        let data_res = if self.download_active() {
            match timeout(DOWNLOAD_STALL_TIMEOUT, self.rx.fetch_async()).await {
                Ok(res) => res,
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for fastboot-usb download data",
                )),
            }
        } else {
            self.rx.fetch_async().await
        };
        self.on_recv(udc_name, data_res).await
    }

    /// Submits one bulk-OUT read into the AIO queue unless a read is already in
    /// flight. Only ever reached from the connected loop, so the endpoint is
    /// guaranteed enabled; exactly one read is queued at a time, so a full
    /// download queue naturally NAKs the host until a block drains.
    fn submit_recv(&mut self, udc_name: &str) {
        if !self.rx.is_empty() {
            return;
        }
        let buf = self.recv_buffer();
        match self.rx.try_recv(buf) {
            Ok(_) => {}
            // Any closed-transport condition (`ENOTCONN`/`ESHUTDOWN`/`BrokenPipe`,
            // or ci_hdrc's unbind-time `EINTR`) means the endpoint is gone, so
            // submitting a *new* read here is never safe: see `gadget_torn_down`'s
            // doc comment for why a fresh read submitted while the driver is
            // disabling the endpoint can hang `RunningGadget::shutdown`
            // indefinitely. Set the flag unconditionally rather than only for
            // `EINTR` -- there's no scenario on a *bulk* endpoint where
            // continuing to resubmit after any of these is useful.
            Err(err) if is_closed_transport_error(&err) => {
                log::debug!("[{udc_name}] fastboot-usb bulk endpoint closed, no longer submitting reads: {err}");
                self.gadget_torn_down = true;
            }
            Err(err) => log::debug!("[{udc_name}] fastboot-usb submit read error: {err}"),
        }
    }

    /// Routes one bulk-OUT receive completion: forward a chunk to the download,
    /// or tear down an in-flight transfer on stall/close. Returns whether the
    /// connected session should keep serving — `false` means a hard failure
    /// already reset() and tore the session down; the caller must close the
    /// bulk endpoint and exit `serve_connected` instead of looping back to
    /// re-arm the read (there is no retry once a packet-level failure like
    /// this — timeout, closed transport, or an otherwise unrecoverable recv
    /// error such as a bad CRC — has occurred).
    async fn on_recv(&mut self, udc_name: &str, data_res: io::Result<Option<BytesMut>>) -> bool {
        match data_res {
            // Handle this completion, then let the loop return to the select and
            // wait for the next packet. The single in-flight read parks on the
            // AIO eventfd, so no manual yield is needed to stay cooperative.
            Ok(Some(chunk)) => self.handle_rx_chunk(udc_name, chunk).await,
            Ok(None) => true,
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                if self.in_flight() {
                    log::warn!(
                        "[{udc_name}] failing in-flight fastboot-usb/fastboot download: timed out waiting for remaining fastboot-usb/fastboot download bytes: {err}"
                    );
                    // A stalled read waits on the host, which by definition is
                    // not sending more data, so it will never complete on its
                    // own: cancel it directly via `EndpointReceiver::cancel()`
                    // instead of waiting for it forever.
                    self.reset(udc_name, Some(FAIL_BADSIZE)).await;
                    false
                } else {
                    true
                }
            }
            // Any closed-transport condition (`ENOTCONN`/`ESHUTDOWN`/`BrokenPipe`,
            // ci_hdrc's unbind-time `EINTR`, or an ep0-style superseded-setup
            // `EIDRM` even though that shouldn't occur on a bulk completion in
            // practice) means this endpoint is gone for good: mark
            // `gadget_torn_down` unconditionally (see its doc comment for why
            // letting the loop submit further reads here can hang
            // `RunningGadget::shutdown` indefinitely) and reset/stop
            // regardless of `in_flight()` -- continuing to "keep serving" when
            // idle would just let the next `submit_recv` resubmit a fresh read
            // into a dead/dying endpoint.
            Err(err) if is_closed_transport_error(&err) => {
                if self.in_flight() {
                    log::warn!(
                        "[{udc_name}] aborting in-flight fastboot-usb/fastboot download: bulk OUT transport closed while waiting for receive completion: {err}"
                    );
                } else {
                    log::debug!("[{udc_name}] fastboot-usb bulk endpoint closed: {err}");
                }
                self.gadget_torn_down = true;
                self.reset(udc_name, None).await;
                false
            }
            Err(err) => {
                // Not a recognized retryable condition (e.g. a bad-CRC style
                // transfer error from the kernel): no retry is expected, so
                // close the endpoint instead of silently looping on a broken
                // stream.
                log::warn!("[{udc_name}] fastboot-usb bulk recv error, closing endpoint: {err}");
                self.reset(udc_name, Some(FAIL_BADSIZE)).await;
                false
            }
        }
    }

    /// Returns whether the connected session should keep serving (see
    /// [`handle_download_chunk`](Self::handle_download_chunk)).
    async fn handle_rx_chunk(&mut self, udc_name: &str, mut chunk: BytesMut) -> bool {
        if !self.download_active() {
            let handled = commands::handle_command_chunk(self, udc_name, &mut chunk).await;
            if handled && self.download_active() && !chunk.is_empty() {
                return self.handle_download_chunk(udc_name, chunk).await;
            }
            return true;
        }

        let remaining = self.remaining_download_bytes();
        if chunk.len() > remaining {
            let trailing = chunk.split_off(remaining);
            if !self.handle_download_chunk(udc_name, chunk).await {
                return false;
            }
            if self.download_active() {
                // Shouldn't happen given the exact split above, but stay
                // defensive: the boundary wasn't actually reached, so there is
                // no trailing command data to process yet.
                return true;
            }
            chunk = trailing;
        } else {
            return self.handle_download_chunk(udc_name, chunk).await;
        }

        self.handle_download_chunk(udc_name, chunk).await
    }

    /// Writes one chunk into the active download. Returns whether the
    /// connected session should keep serving: `false` means a hard failure
    /// occurred and `reset()` already tore the session down (and best-effort
    /// replied FAIL) — the caller must close the bulk endpoint and exit
    /// `serve_connected`, not loop back and try to keep receiving.
    async fn handle_download_chunk(&mut self, udc_name: &str, chunk: BytesMut) -> bool {
        if chunk.is_empty() {
            return true;
        }

        // Bytes consumed off the USB endpoint. Whether swupdate accepts them or
        // has already finished, the host is a dumb file-transfer tool that will
        // send the whole download; we must consume every byte and only ack at
        // the real boundary. Once swupdate is done (`Sent::Done`) we simply keep
        // draining and discarding.
        let chunk_len = chunk.len();

        let Some(download) = self.download.as_mut() else {
            self.reset(udc_name, Some(FAIL_NOTOPEN)).await;
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
                }
                true
            }
            Err(err) => {
                if err.kind() == io::ErrorKind::NotConnected {
                    let _ = send_static(&mut self.tx, FAIL_NOTOPEN).await;
                } else {
                    log::error!("[{udc_name}] fastboot-usb write to SWUpdate failed: {err}");
                    let _ = send_static(&mut self.tx, FAIL_EPIPE).await;
                }
                self.reset(udc_name, None).await;
                false
            }
        }
    }
}

/// Waits until the function is enabled by a host. Takes no `FastbootUsbState`
/// (and so has no way to touch a USB endpoint even by accident) — it does
/// nothing but watch the control loop's signal.
///
/// Returns `true` once enabled, or `false` if the control loop ended (gadget
/// unbound) while still disabled.
async fn wait_for_enabled(enabled: &mut watch::Receiver<bool>) -> bool {
    loop {
        if *enabled.borrow_and_update() {
            return true;
        }
        if enabled.changed().await.is_err() {
            return false;
        }
    }
}
