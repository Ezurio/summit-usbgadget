//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! Bounded bulk-OUT receive queue for the FBK function.
//!
//! Wraps the kernel AIO receive ring plus a small buffer pool. It keeps up to
//! [`RECV_QUEUE_DEPTH`] buffers submitted so the host is never flow-controlled
//! while a chunk is being serviced, and recycles drained buffers.
//!
//! The buffer capacity is fixed for the entire lifetime of the queue (set once
//! in [`RecvQueue::new`]) and is never changed mid-session: doing so requires
//! cancelling in-flight buffers, which discards any data the host already
//! streamed into them before it was fetched — silent data loss. See
//! [`cancel`](Self::cancel) for the one case where that trade-off is
//! acceptable (function disable / transport teardown).

use std::io;

use bytes::BytesMut;
use usb_gadget::function::custom::EndpointReceiver;

use super::RECV_QUEUE_DEPTH;

#[derive(Debug)]
pub(super) struct RecvQueue {
    rx: EndpointReceiver,
    capacity: usize,
    max_packet_size: usize,
    pool: Vec<BytesMut>,
    inflight: usize,
}

impl RecvQueue {
    pub(super) fn new(mut rx: EndpointReceiver, capacity: usize) -> Self {
        let max_packet_size = rx.max_packet_size().unwrap_or(512);
        Self { rx, capacity, max_packet_size, pool: Vec::new(), inflight: 0 }
    }

    /// Number of buffers currently submitted to the kernel and awaiting
    /// completion. The serve loop only awaits a completion while this is > 0,
    /// which is the queue's starvation guard.
    pub(super) fn inflight(&self) -> usize {
        self.inflight
    }

    /// Await the next completed receive buffer.
    pub(super) async fn fetch(&mut self) -> io::Result<Option<BytesMut>> {
        self.rx.fetch_async().await
    }

    /// Account for a completion that was just consumed (data or error).
    pub(super) fn on_completed(&mut self) {
        self.inflight = self.inflight.saturating_sub(1);
    }

    /// Resync the in-flight count to empty (e.g. after a spurious `Ok(None)`).
    pub(super) fn reset_inflight(&mut self) {
        self.inflight = 0;
    }

    fn take_buf(&mut self, capacity: usize) -> BytesMut {
        if capacity == self.capacity {
            self.pool.pop().unwrap_or_else(|| BytesMut::with_capacity(self.capacity))
        } else {
            BytesMut::with_capacity(capacity)
        }
    }

    fn round_recv_capacity(&self, requested: usize) -> usize {
        let mps = self.max_packet_size.max(1);
        requested.div_ceil(mps) * mps
    }

    /// Top the queue up to [`RECV_QUEUE_DEPTH`] submitted buffers while the AIO
    /// ring has space. Applies backpressure implicitly: once the ring is full,
    /// `is_ready` returns false and submission stops.
    pub(super) fn prime(&mut self, requested_capacity: usize) -> io::Result<()> {
        let recv_capacity = self.round_recv_capacity(requested_capacity.min(self.capacity));
        while self.inflight < RECV_QUEUE_DEPTH && self.rx.is_ready() {
            let buf = self.take_buf(recv_capacity);
            self.rx.try_recv(buf)?;
            self.inflight += 1;
        }
        Ok(())
    }

    /// [`prime`](Self::prime), downgrading a closed transport to an empty queue
    /// and other errors to a debug log so the hot path never stalls.
    pub(super) fn prime_or_report(&mut self, udc_name: &str, requested_capacity: usize) {
        if let Err(err) = self.prime(requested_capacity) {
            if crate::functionfs::is_closed_transport_error(&err) {
                self.inflight = 0;
            } else {
                log::debug!("[{udc_name}] FBK re-queue recv buffer error: {err}");
            }
        }
    }

    /// Return a drained buffer to the pool if it still matches the current
    /// capacity; otherwise let it drop (freeing its memory).
    pub(super) fn recycle(&mut self, mut buf: BytesMut) {
        if buf.capacity() != self.capacity {
            return;
        }
        buf.clear();
        self.pool.push(buf);
    }

    /// Cancel all in-flight buffers and reset the in-flight count. Discards any
    /// data already completed into those buffers but not yet fetched — only
    /// safe when the transport itself is being torn down (function disable),
    /// where that data is no longer meaningful anyway.
    pub(super) fn cancel(&mut self, udc_name: &str) {
        if let Err(err) = self.rx.cancel() {
            if !crate::functionfs::is_closed_transport_error(&err) {
                log::debug!("[{udc_name}] FBK receive queue cancel failed: {err}");
            }
        }
        self.inflight = 0;
    }

    /// Cancel in-flight buffers, then stall (halt) the bulk OUT endpoint.
    ///
    /// Use this when a download is abandoned mid-transfer (e.g. the SWUpdate
    /// sink failed) while the host may still be streaming the remainder of the
    /// declared payload. Resetting our own state alone is not enough: the host
    /// has no idea we gave up and keeps sending megabytes of firmware bytes,
    /// which would otherwise be silently discarded or misread as commands.
    /// Halting the endpoint makes the host's USB stack surface an immediate
    /// I/O error on the pending bulk write, so it stops streaming right away.
    /// The endpoint stays halted until the host itself clears the halt
    /// feature (standard USB error-recovery flow) before its next attempt.
    pub(super) fn cancel_and_halt(&mut self, udc_name: &str) {
        self.cancel(udc_name);
        match self.rx.control() {
            Ok(ctrl) => {
                if let Err(err) = ctrl.halt() {
                    log::debug!("[{udc_name}] FBK receive endpoint halt failed: {err}");
                }
            }
            Err(err) => {
                log::debug!("[{udc_name}] FBK receive endpoint control unavailable for halt: {err}");
            }
        }
    }
}
