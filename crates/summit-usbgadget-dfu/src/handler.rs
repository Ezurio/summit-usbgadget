//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! The DFU state machine driven from endpoint zero.

use std::io;

use bytes::{Bytes, BytesMut};
use crc32fast::Hasher;

use usb_gadget::function::custom::{CtrlReceiver, CtrlReq};

use summit_usbgadget_swupdate::{Feed, SwupdateParams, SwupdateSession};

use super::config::{DfuConfig, UploadSource};
use super::protocol::{request, GetStatus, State, Status};

const DFU_SUFFIX_LEN: usize = 16;
const MANIFEST_POLL_TIMEOUT_MS: u32 = 250;

/// Payload returned for a DFU device-to-host control request.
#[derive(Debug, Clone)]
pub(crate) enum InReply {
    /// Small fixed-size payload stored inline (no heap allocation).
    Inline { buf: [u8; 6], len: usize },
    /// Variable-size payload backed by shared bytes.
    Data(Bytes),
}

impl InReply {
    /// Returns the payload as bytes.
    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            InReply::Inline { buf, len } => &buf[..*len],
            InReply::Data(data) => data.as_ref(),
        }
    }

    fn from_state(state: State) -> Self {
        let mut buf = [0u8; 6];
        buf[0] = state as u8;
        InReply::Inline { buf, len: 1 }
    }
}

/// Returns whether a control request is a DFU class request directed at an
/// interface, i.e. one this handler is responsible for.
pub fn is_dfu_request(req: &CtrlReq) -> bool {
    // bmRequestType: type field (bits 6:5) == class (0b01) and
    // recipient field (bits 4:0) == interface (0b00001).
    (req.request_type & 0x60) == 0x20 && (req.request_type & 0x1f) == 0x01
}

/// The DFU state machine and active firmware sink.
pub struct Dfu {
    transfer_size: u16,
    poll_timeout_ms: u32,
    download: SwupdateParams,
    download_tail: Vec<u8>,
    download_crc: Hasher,
    upload: Option<UploadSource>,
    sink: Option<SwupdateSession>,
    /// A block that could not be queued yet (queue was full); retried before the
    /// next block and reported to the host as `dfuDNBUSY` until it is accepted.
    pending: Option<BytesMut>,
    state: State,
    status: Status,
}

impl Dfu {
    /// Creates a new DFU handler from the given configuration.
    pub fn new(config: DfuConfig) -> Self {
        let DfuConfig { download, upload, transfer_size, poll_timeout_ms } = config;
        let sink = SwupdateSession::new(download.clone(), transfer_size as usize);
        Self {
            transfer_size,
            poll_timeout_ms,
            download,
            download_tail: Vec::with_capacity(DFU_SUFFIX_LEN),
            download_crc: Hasher::new(),
            upload,
            sink: Some(sink),
            pending: None,
            state: State::DfuIdle,
            status: Status::Ok,
        }
    }

    /// Handles a host-to-device DFU control request. Download data blocks are
    /// received separately in [`receive_dnload_block`](Self::receive_dnload_block);
    /// the requests dispatched here carry no OUT data stage.
    pub(crate) async fn handle_out(&mut self, req: &CtrlReq) -> io::Result<()> {
        self.poll_download_progress().await?;
        self.poll_manifest().await;

        match req.request {
            request::DETACH => {
                log::info!("DFU_DETACH (detach timeout {} ms)", req.value);
                self.state = State::AppDetach;
                Ok(())
            }
            request::DNLOAD => {
                // A zero-length DFU_DNLOAD begins manifestation. Non-empty
                // download blocks are received in receive_dnload_block and
                // never reach here.
                self.manifest().await
            }
            request::CLRSTATUS => {
                log::debug!("DFU_CLRSTATUS");
                if self.state == State::Error
                    || self.is_finishing()
                    || self.sink.is_none()
                {
                    self.abort_transfer().await;
                } else {
                    self.status = Status::Ok;
                    self.state = State::DfuIdle;
                    self.download_tail.clear();
                    self.download_crc = Hasher::new();
                }
                Ok(())
            }
            request::ABORT => {
                log::debug!("DFU_ABORT");
                self.abort_transfer().await;
                Ok(())
            }
            other => {
                log::warn!("unsupported DFU OUT request {other:#04x}");
                self.fault(Status::ErrStalledPkt);
                Err(io::Error::new(io::ErrorKind::InvalidInput, "unsupported DFU request"))
            }
        }
    }

    pub(crate) async fn abort_transfer(&mut self) {
        if let Some(sink) = self.sink.as_mut() {
            sink.abort().await;
        }
        self.sink = Some(SwupdateSession::new(self.download.clone(), self.transfer_size as usize));
        self.pending = None;
        self.status = Status::Ok;
        self.state = State::DfuIdle;
        self.download_tail.clear();
        self.download_crc = Hasher::new();
    }

    /// Handles a device-to-host DFU control request, returning the response
    /// payload to send back to the host.
    pub(crate) async fn handle_in(&mut self, req: &CtrlReq) -> io::Result<InReply> {
        self.poll_download_progress().await?;
        self.poll_manifest().await;

        match req.request {
            request::GETSTATUS => Ok(self.get_status().await),
            request::GETSTATE => Ok(InReply::from_state(self.state)),
            request::UPLOAD => self.upload_block(req).await,
            other => {
                log::warn!("unsupported DFU IN request {other:#04x}");
                self.fault(Status::ErrStalledPkt);
                Err(io::Error::new(io::ErrorKind::InvalidInput, "unsupported DFU request"))
            }
        }
    }

    /// Performs the manifestation (programming) phase after a zero-length
    /// `DFU_DNLOAD`, mapping the sink's outcome to a DFU state and status.
    async fn manifest(&mut self) -> io::Result<()> {
        log::info!("DFU_DNLOAD complete, entering manifestation");
        self.state = State::Manifest;
        self.status = Status::Ok;

        if self.sink.is_none() {
            self.fault(Status::ErrUnknown);
            return Ok(());
        }

        let tail = std::mem::take(&mut self.download_tail);
        if has_valid_dfu_suffix(&self.download_crc, &tail) {
            log::info!("ignoring trailing DFU suffix before manifestation");
        } else if !tail.is_empty() {
            let chunk = {
                let sink = self.sink.as_mut().expect("sink present");
                recycled_chunk_buffer(sink, &tail)
            };
            self.enqueue_chunk(chunk).await?;
        }

        // Flush any stashed block into the queue, then signal EOF (no more
        // data). Finalization is observed by poll_manifest via the status
        // signal, not driven from here.
        if let Some(prev) = self.pending.take() {
            let _ = self.sink.as_mut().expect("sink present").send(prev).await?;
        }
        self.sink.as_mut().expect("sink present").eof();
        Ok(())
    }

    /// Builds the `DFU_GETSTATUS` response and advances the synchronous parts
    /// of the state machine.
    async fn get_status(&mut self) -> InReply {
        self.state = match self.state {
            State::DnloadSync | State::DnBusy => {
                if self.download_is_busy().await {
                    State::DnBusy
                } else {
                    State::DnloadIdle
                }
            }
            // Manifestation is performed during the zero-length DNLOAD, so it is
            // complete when the background manifestation task reports success.
            State::ManifestSync => State::DfuIdle,
            other => other,
        };

        let response = GetStatus {
            status: self.status,
            poll_timeout_ms: if matches!(self.state, State::Manifest | State::ManifestSync) {
                MANIFEST_POLL_TIMEOUT_MS
            } else {
                self.poll_timeout_ms
            },
            state: self.state,
            string_index: 0,
        };
        log::debug!("DFU_GETSTATUS -> status {:?}, state {:?}", self.status, self.state);
        let mut buf = [0u8; 6];
        buf.copy_from_slice(&response.to_bytes());
        InReply::Inline { buf, len: 6 }
    }

    /// Serves a single `DFU_UPLOAD` block from the configured upload source.
    async fn upload_block(&mut self, req: &CtrlReq) -> io::Result<InReply> {
        let transfer_size = self.transfer_size as usize;
        let offset = req.value as u64 * self.transfer_size as u64;

        let buf = match &self.upload {
            None => {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no DFU upload source configured"))
            }
            Some(UploadSource::Data(data)) => {
                // In-memory blob (e.g. system information).
                let start = (offset as usize).min(data.len());
                let end = (start + transfer_size).min(data.len());
                data.slice(start..end)
            }
        };

        if buf.len() < transfer_size {
            // A short (or empty) block terminates the upload.
            self.state = State::DfuIdle;
        } else {
            self.state = State::UploadIdle;
        }

        log::debug!("DFU_UPLOAD block {} ({} bytes @ offset {})", req.value, buf.len(), offset);
        Ok(InReply::Data(buf))
    }

    /// Transitions to the error state with the given status.
    fn fault(&mut self, status: Status) {
        self.status = status;
        self.state = State::Error;
    }

    /// Whether the transfer is in its manifestation (finalization) phase.
    fn is_finishing(&self) -> bool {
        matches!(self.state, State::Manifest | State::ManifestSync)
    }

    /// Tries to flush a previously stashed block. Returns `true` while a block
    /// is still stashed (i.e. the host should keep seeing `dfuDNBUSY`).
    async fn flush_pending(&mut self) -> io::Result<bool> {
        let Some(chunk) = self.pending.take() else {
            return Ok(false);
        };
        let Some(sink) = self.sink.as_mut() else {
            return Ok(false);
        };
        match sink.try_send(chunk).await? {
            Feed::Ok(_) => Ok(false),
            Feed::Full(chunk) => {
                self.pending = Some(chunk);
                Ok(true)
            }
            Feed::Closed => Ok(false),
        }
    }

    /// Enqueues a firmware block, stashing it (busy) if the queue is full.
    async fn enqueue_chunk(&mut self, chunk: BytesMut) -> io::Result<()> {
        // Any block stashed from an earlier busy reply goes first. The host
        // should not send a new block while busy, so this rarely blocks.
        if let Some(prev) = self.pending.take() {
            let sink = self
                .sink
                .as_mut()
                .ok_or_else(|| io::Error::other("manifestation in progress"))?;
            let _ = sink.send(prev).await?;
        }
        let sink = self
            .sink
            .as_mut()
            .ok_or_else(|| io::Error::other("manifestation in progress"))?;
        match sink.try_send(chunk).await? {
            Feed::Ok(_) => Ok(()),
            Feed::Full(chunk) => {
                self.pending = Some(chunk);
                Ok(())
            }
            Feed::Closed => Ok(()),
        }
    }

    async fn download_is_busy(&mut self) -> bool {
        self.flush_pending().await.unwrap_or(false)
    }

    /// Receives a `DFU_DNLOAD` data block straight into a recycled buffer and
    /// enqueues it, avoiding the intermediate `Vec` from `recv_all_async` and
    /// the extra copy into a fresh chunk buffer. The small carried-over suffix
    /// tail is written to the front of the buffer, the USB data is read in
    /// directly after it, and the same buffer is reused as the outgoing chunk.
    pub(crate) async fn receive_dnload_block(&mut self, req: CtrlReceiver<'_>) -> io::Result<()> {
        self.poll_download_progress().await?;
        self.poll_manifest().await;

        if self.is_finishing() || self.sink.is_none() {
            return Err(io::Error::other("manifestation in progress"));
        }

        let carry = self.download_tail.len();
        let len = req.len();

        let mut buf = self.sink.as_mut().expect("sink present").buffer(carry + len);
        buf.extend_from_slice(&self.download_tail);
        buf.resize(carry + len, 0);
        let received = req.recv_async(&mut buf[carry..]).await?;
        buf.truncate(carry + received);

        self.state = State::DnloadSync;
        self.status = Status::Ok;

        // Hold back the final DFU-suffix-sized window for the suffix check at
        // manifestation; flush everything before it as one chunk.
        let Some(flush) = flushable_len(buf.len()) else {
            self.download_tail.clear();
            self.download_tail.extend_from_slice(&buf);
            return Ok(());
        };
        self.download_tail.clear();
        self.download_tail.extend_from_slice(&buf[flush..]);
        buf.truncate(flush);
        self.download_crc.update(&buf);
        self.enqueue_chunk(buf).await
    }

    async fn poll_download_progress(&mut self) -> io::Result<()> {
        if let Err(err) = self.flush_pending().await {
            if self.state != State::Manifest {
                return Err(io::Error::other(err));
            }
        }

        // A swupdate failure is reported immediately: dfu-util polls GETSTATUS,
        // so faulting here makes it exit non-zero even mid-download. Early
        // *success* is cached (in the session) and only acted on at
        // manifestation, since the host keeps sending the whole file.
        if !self.is_finishing() {
            let failed = match self.sink.as_mut() {
                Some(sink) if sink.is_open() => sink.try_finished().and_then(Result::err),
                _ => None,
            };
            if let Some(err) = failed {
                log::error!("swupdate failed during download: {err}");
                self.fault(Status::ErrVerify);
            }
        }
        Ok(())
    }

    /// Checks whether the asynchronous manifestation task finished, then folds
    /// its outcome back into the DFU state machine.
    async fn poll_manifest(&mut self) {
        if self.state != State::Manifest {
            return;
        }

        let complete = match self.sink.as_mut() {
            Some(sink) => sink.try_finished(),
            None => return,
        };

        match complete {
            Some(Ok(())) => {
                self.download_crc = Hasher::new();
                self.sink = Some(SwupdateSession::new(self.download.clone(), self.transfer_size as usize));
                self.pending = None;
                self.status = Status::Ok;
                self.state = State::ManifestSync;
            }
            None => {}
            Some(Err(err)) => {
                self.download_crc = Hasher::new();
                self.sink = Some(SwupdateSession::new(self.download.clone(), self.transfer_size as usize));
                self.pending = None;
                log::error!("manifestation failed: {err}");
                self.fault(Status::ErrVerify);
            }
        }
    }

}

fn recycled_chunk_buffer(sink: &mut SwupdateSession, data: &[u8]) -> BytesMut {
    let mut buf = sink.buffer(data.len());
    buf.extend_from_slice(data);
    buf
}

fn flushable_len(total: usize) -> Option<usize> {
    (total > DFU_SUFFIX_LEN).then(|| total - DFU_SUFFIX_LEN)
}

fn has_valid_dfu_suffix(prefix_crc: &Hasher, buf: &[u8]) -> bool {
    if buf.len() != DFU_SUFFIX_LEN || buf.get(8..11) != Some(b"UFD") || buf[11] as usize != DFU_SUFFIX_LEN {
        return false;
    }

    let mut hasher = prefix_crc.clone();
    hasher.update(&buf[..DFU_SUFFIX_LEN - 4]);
    let crc = !hasher.finalize();
    let expected = u32::from_le_bytes(buf[DFU_SUFFIX_LEN - 4..].try_into().expect("crc bytes"));
    crc == expected
}

#[cfg(test)]
mod tests;
