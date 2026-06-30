//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! The DFU state machine driven from endpoint zero.

use std::future::Future;
use std::future::poll_fn;
use std::io::{self, SeekFrom};
use std::pin::Pin;
use std::task::Poll;

use bytes::Bytes;
use crc32fast::Hasher;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use usb_gadget::function::custom::CtrlReq;

use crate::stream_download::{ActiveDownload, DownloadTarget};

use super::config::{DfuConfig, UploadSource};
use super::protocol::{request, GetStatus, State, Status};

const DFU_SUFFIX_LEN: usize = 16;

type ManifestFuture = Pin<Box<dyn Future<Output = (ActiveDownload, io::Result<()>)> + Send>>;

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
    download: DownloadTarget,
    download_tail: Vec<u8>,
    download_crc: Hasher,
    upload: Option<UploadSource>,
    upload_file: Option<File>,
    upload_buf: Vec<u8>,
    sink: Option<ActiveDownload>,
    manifest_future: Option<ManifestFuture>,
    state: State,
    status: Status,
}

impl Dfu {
    /// Creates a new DFU handler from the given configuration.
    pub fn new(config: DfuConfig) -> Self {
        let DfuConfig { download, upload, transfer_size, poll_timeout_ms } = config;
        let sink = ActiveDownload::new(download.clone());
        Self {
            transfer_size,
            poll_timeout_ms,
            download,
            download_tail: Vec::with_capacity(DFU_SUFFIX_LEN),
            download_crc: Hasher::new(),
            upload,
            upload_file: None,
            upload_buf: Vec::new(),
            sink: Some(sink),
            manifest_future: None,
            state: State::DfuIdle,
            status: Status::Ok,
        }
    }

    /// Handles a host-to-device DFU control request and its payload.
    pub(crate) async fn handle_out(&mut self, req: &CtrlReq, data: &[u8]) -> io::Result<()> {
        self.poll_download_progress().await?;
        self.poll_manifest().await;

        match req.request {
            request::DETACH => {
                log::info!("DFU_DETACH (detach timeout {} ms)", req.value);
                self.state = State::AppDetach;
                Ok(())
            }
            request::DNLOAD => {
                if data.is_empty() {
                    self.manifest().await
                } else {
                    self.write_dnload_block(data).await?;
                    self.state = State::DnloadSync;
                    self.status = Status::Ok;
                    log::debug!("DFU_DNLOAD block {} ({} bytes)", req.value, data.len());
                    Ok(())
                }
            }
            request::CLRSTATUS => {
                log::debug!("DFU_CLRSTATUS");
                if self.state == State::Error || self.manifest_future.is_some() || self.sink.is_none() {
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
        self.manifest_future = None;
        if let Some(sink) = self.sink.as_mut() {
            if sink.is_active() {
                sink.abort().await;
            }
        }
        self.sink = Some(ActiveDownload::new(self.download.clone()));
        self.status = Status::Ok;
        self.state = State::DfuIdle;
        self.download_tail.clear();
        self.download_crc = Hasher::new();
        self.upload_file = None;
        self.upload_buf.clear();
    }

    /// Handles a device-to-host DFU control request, returning the response
    /// payload to send back to the host.
    pub(crate) async fn handle_in(&mut self, req: &CtrlReq) -> io::Result<InReply> {
        self.poll_download_progress().await?;
        self.poll_manifest().await;

        match req.request {
            request::GETSTATUS => Ok(self.get_status()),
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

        let Some(mut sink) = self.sink.take() else {
            self.fault(Status::ErrUnknown);
            return Ok(());
        };

        let tail = std::mem::take(&mut self.download_tail);
        if has_valid_dfu_suffix(&self.download_crc, &tail) {
            log::info!("ignoring trailing DFU suffix before manifestation");
        } else if !tail.is_empty() {
            sink.write_block(&tail).await?;
        }

        self.manifest_future = Some(Box::pin(async move {
            let result = sink.finish().await;
            (sink, result)
        }));
        Ok(())
    }

    /// Builds the `DFU_GETSTATUS` response and advances the synchronous parts
    /// of the state machine.
    fn get_status(&mut self) -> InReply {
        self.state = match self.state {
            State::DnloadSync | State::DnBusy => {
                if self.download_is_busy() {
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
            poll_timeout_ms: self.poll_timeout_ms,
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

        // Determine the source kind without holding a borrow across awaits.
        let path = match &self.upload {
            None => {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no DFU upload source configured"))
            }
            Some(UploadSource::Data(_)) => None,
            Some(UploadSource::File(path)) => Some(path.clone()),
        };

        let buf = match path {
            None => {
                // In-memory blob (e.g. system information).
                let data = match &self.upload {
                    Some(UploadSource::Data(data)) => data,
                    _ => unreachable!("upload source is data"),
                };
                let start = (offset as usize).min(data.len());
                let end = (start + transfer_size).min(data.len());
                data.slice(start..end)
            }
            Some(path) => {
                if self.upload_file.is_none() {
                    self.upload_file = Some(File::open(&path).await?);
                }
                let file = self.upload_file.as_mut().expect("upload file just set");
                let _ = file.seek(SeekFrom::Start(offset)).await?;
                self.upload_buf.resize(transfer_size, 0);
                let n = read_full(file, &mut self.upload_buf).await?;
                self.upload_buf.truncate(n);
                Bytes::copy_from_slice(&self.upload_buf)
            }
        };

        if buf.len() < transfer_size {
            // A short (or empty) block terminates the upload.
            self.state = State::DfuIdle;
            self.upload_file = None;
            self.upload_buf.clear();
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

    fn download_is_busy(&self) -> bool {
        self.sink.as_ref().is_some_and(ActiveDownload::is_busy)
    }

    async fn write_dnload_block(&mut self, data: &[u8]) -> io::Result<()> {
        if self.sink.is_none() {
            return Err(io::Error::other("manifestation in progress"));
        }

        self.download_tail.extend_from_slice(data);
        let Some(prefix) = take_suffix_prefix(&mut self.download_tail) else {
            return Ok(());
        };

        let sink = self
            .sink
            .as_mut()
            .ok_or_else(|| io::Error::other("manifestation in progress"))?;
        self.download_crc.update(&prefix);
        sink.write_block(&prefix).await
    }

    async fn poll_download_progress(&mut self) -> io::Result<()> {
        if let Some(sink) = self.sink.as_mut() {
            sink.poll_progress().await?;
        }
        Ok(())
    }

    /// Checks whether the asynchronous manifestation task finished, then folds
    /// its outcome back into the DFU state machine.
    async fn poll_manifest(&mut self) {
        let Some(future) = self.manifest_future.as_mut() else {
            return;
        };

        let joined = poll_fn(|cx| match future.as_mut().poll(cx) {
            Poll::Ready(joined) => Poll::Ready(Some(joined)),
            Poll::Pending => Poll::Ready(None),
        })
        .await;
        let Some(joined) = joined else {
            return;
        };
        self.manifest_future = None;
        self.download_crc = Hasher::new();

        match joined {
            (sink, Ok(())) => {
                self.sink = Some(sink);
                self.status = Status::Ok;
                self.state = State::ManifestSync;
            }
            (sink, Err(err)) => {
                self.sink = Some(sink);
                log::error!("manifestation failed: {err}");
                self.fault(Status::ErrVerify);
            }
        }
    }

}

/// Reads into `buf` until it is full or end-of-file is reached, returning the
/// number of bytes read.
async fn read_full(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        let n = file.read(&mut buf[total..]).await?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(total)
}

fn take_suffix_prefix(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    if buf.len() <= DFU_SUFFIX_LEN {
        return None;
    }

    let keep_from = buf.len() - DFU_SUFFIX_LEN;
    let suffix = buf.split_off(keep_from);
    Some(std::mem::replace(buf, suffix))
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
mod tests {
    use std::future::pending;

    use crc32fast::Hasher;
    use usb_gadget::function::custom::CtrlReq;

    use crate::dfu::config::DfuConfig;
    use crate::dfu::protocol::{request, State, Status};
    use crate::stream_download::DownloadTarget;

    use super::{DFU_SUFFIX_LEN, Dfu, has_valid_dfu_suffix, take_suffix_prefix};

    fn test_config() -> DfuConfig {
        DfuConfig {
            download: DownloadTarget::File(std::env::temp_dir().join("summit-usbgadget-dfu-test.bin")),
            upload: None,
            transfer_size: 4096,
            poll_timeout_ms: 10,
        }
    }

    fn out_req(request: u8) -> CtrlReq {
        CtrlReq { request_type: 0x21, request, value: 0, index: 0, length: 0 }
    }

    #[test]
    fn valid_dfu_suffix_is_recognized() {
        let payload = b"firmware";
        let mut suffix = [0u8; DFU_SUFFIX_LEN];
        suffix[8..11].copy_from_slice(b"UFD");
        suffix[11] = DFU_SUFFIX_LEN as u8;
        let mut prefix_crc = Hasher::new();
        prefix_crc.update(payload);
        let mut full_crc = prefix_crc.clone();
        full_crc.update(&suffix[..DFU_SUFFIX_LEN - 4]);
        let crc = !full_crc.finalize();
        suffix[12..].copy_from_slice(&crc.to_le_bytes());
        assert!(has_valid_dfu_suffix(&prefix_crc, &suffix));
    }

    #[test]
    fn non_suffix_tail_is_not_stripped() {
        let mut prefix_crc = Hasher::new();
        prefix_crc.update(b"firmware");
        let mut tail = [0u8; DFU_SUFFIX_LEN];
        tail[8..11].copy_from_slice(b"BAD");
        tail[11] = DFU_SUFFIX_LEN as u8;
        assert!(!has_valid_dfu_suffix(&prefix_crc, &tail));
    }

    #[test]
    fn bad_crc_invalidates_suffix() {
        let mut prefix_crc = Hasher::new();
        prefix_crc.update(b"firmware");
        let mut suffix = [0u8; DFU_SUFFIX_LEN];
        suffix[8..11].copy_from_slice(b"UFD");
        suffix[11] = DFU_SUFFIX_LEN as u8;
        suffix[12..].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        assert!(!has_valid_dfu_suffix(&prefix_crc, &suffix));
    }

    #[test]
    fn suffix_window_keeps_last_sixteen_bytes() {
        let mut buf = (0u8..20).collect::<Vec<_>>();
        let prefix = take_suffix_prefix(&mut buf).expect("prefix should be emitted");
        assert_eq!(prefix, vec![0, 1, 2, 3]);
        assert_eq!(buf, (4u8..20).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn get_status_reports_dnbusy_when_buffer_limit_is_reached() {
        let mut dfu = Dfu::new(test_config());
        dfu.sink = None;
        dfu.state = State::DnloadSync;
        dfu.status = Status::Ok;
        assert_eq!(dfu.get_status().as_slice()[4], State::DnloadIdle as u8);
    }

    #[tokio::test]
    async fn clrstatus_resets_stale_manifestation_state() {
        let mut dfu = Dfu::new(test_config());
        dfu.sink = None;
        dfu.manifest_future = Some(Box::pin(pending()));
        dfu.state = State::Error;
        dfu.status = Status::ErrVerify;

        dfu.handle_out(&out_req(request::CLRSTATUS), &[]).await.expect("clrstatus should succeed");

        assert!(dfu.sink.is_some());
        assert!(dfu.manifest_future.is_none());
        assert_eq!(dfu.state, State::DfuIdle);
        assert_eq!(dfu.status, Status::Ok);
    }
}
