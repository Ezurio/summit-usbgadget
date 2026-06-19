//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! Minimal FBK-compatible custom function.
//!
//! This module implements a narrow clean-room subset required for host-to-device
//! file upload (`FBK:UCP local t:remote`) and streams the uploaded bytes into
//! the same download session used by DFU.
//!
//! Supported command subset (on the bulk OUT command channel):
//! - `WOpen:<path>`
//! - `donwload:<hex_size>` (mfgtools spelling)
//! - `download:<hex_size>` (accepted alias)
//! - `Close`
//!
//! Reply subset (on bulk IN):
//! - `OKAY`
//! - `FAIL<message>`
//! - `DATA%08X`

use std::io;

use bytes::{Bytes, BytesMut};
use usb_gadget::function::custom::{
    Custom, Endpoint, EndpointDirection, EndpointReceiver, EndpointSender, Event, Interface,
    OsExtCompat,
};
use usb_gadget::function::Handle;
use usb_gadget::Class;

use crate::config::DfuFnConfig;
use crate::functionfs::EventHandler;
use crate::stream_download::{ActiveDownload, DownloadTarget};

const OKAY: &[u8] = b"OKAY";
const FAIL_CMD: &[u8] = b"FAILCMD";
const FAIL_CLOSE: &[u8] = b"FAILCLOSE";
const FAIL_EPIPE: &[u8] = b"FAILEPIPE";
const FAIL_NOTOPEN: &[u8] = b"FAILNOTOPEN";
const FAIL_OPEN: &[u8] = b"FAILOPEN";

/// Runtime pieces for the FBK custom function.
#[derive(Debug)]
pub struct FbkRuntime {
    custom: Custom,
    rx: EndpointReceiver,
    tx: EndpointSender,
    download: DownloadTarget,
}

/// Build the FBK custom function and return its gadget handle plus runtime.
pub fn build(cfg: &DfuFnConfig) -> (Handle, FbkRuntime) {
    let download = cfg.to_download_target();
    let (rx, rx_dir) = EndpointDirection::host_to_device();
    let (tx, tx_dir) = EndpointDirection::device_to_host();

    let (custom, handle) = Custom::builder()
        .with_interface(
            Interface::new(Class::vendor_specific(0, 0), "FBK")
                .with_endpoint(Endpoint::bulk(rx_dir))
                .with_endpoint(Endpoint::bulk(tx_dir))
                .with_os_ext_compat(OsExtCompat::winusb()),
        )
        .build();

    (handle, FbkRuntime { custom, rx, tx, download })
}

/// Serve the minimal FBK command/data loop for one bound gadget.
pub async fn serve(udc_name: String, mut runtime: FbkRuntime) {
    let max_packet = match runtime.rx.max_packet_size() {
        Ok(sz) if sz > 0 => sz,
        _ => 1024,
    };

    let mut state = FbkState {
        rx: runtime.rx,
        tx: runtime.tx,
        download: ActiveDownload::new(runtime.download),
        pending_download: None,
        recv_capacity: max_packet * 16,
        recv_buf_pool: Vec::new(),
    };

    crate::functionfs::serve(
        udc_name,
        "FBK upload function",
        &mut runtime.custom,
        &mut state,
    )
    .await;
}

#[derive(Debug)]
struct FbkState {
    rx: EndpointReceiver,
    tx: EndpointSender,
    download: ActiveDownload,
    pending_download: Option<usize>,
    recv_capacity: usize,
    recv_buf_pool: Vec<BytesMut>,
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
                self.pending_download = None;
            }
            Event::SetupHostToDevice(req) => {
                let _ = req.recv_all_async().await;
            }
            Event::SetupDeviceToHost(req) => {
                let _ = req.send_async(&[]).await;
            }
            _ => {}
        }

        loop {
            let recv_buf = self.take_recv_buf();
            let data_opt = match self.rx.recv_async(recv_buf).await {
                Ok(data_opt) => data_opt,
                Err(_) => break,
            };

            let Some(mut chunk) = data_opt else {
                break;
            };

            self.handle_rx_chunk(udc_name, &mut chunk).await;
            self.recycle_recv_buf(chunk);
        }

        Ok(())
    }
}

impl FbkState {
    async fn handle_rx_chunk(&mut self, udc_name: &str, chunk: &mut BytesMut) {
        if let Some(rem) = self.pending_download {
            let take = rem.min(chunk.len());
            let data = chunk.split_to(take);
            let failed = match self.download.write_block_if_active(&data).await {
                Ok(()) => false,
                Err(err) if err.kind() == io::ErrorKind::NotConnected => {
                    let _ = send_static(&mut self.tx, FAIL_NOTOPEN).await;
                    true
                }
                Err(err) => {
                    log::error!("[{udc_name}] FBK write to SWUpdate failed: {err}");
                    let _ = send_static(&mut self.tx, FAIL_EPIPE).await;
                    true
                }
            };

            if failed {
                self.download.abort().await;
                self.pending_download = None;
                return;
            }

            let new_rem = rem - take;
            self.pending_download = if new_rem == 0 { None } else { Some(new_rem) };
            if new_rem == 0 {
                let _ = send_static(&mut self.tx, OKAY).await;
            }

            if chunk.is_empty() {
                return;
            }
        }

        let cmd = trim_command(chunk);
        if cmd.is_empty() {
            return;
        }

        if cmd.starts_with(b"WOpen:") {
            if let Err(err) = self.download.begin().await {
                log::error!("[{udc_name}] FBK WOpen begin failed: {err}");
                let _ = send_static(&mut self.tx, FAIL_OPEN).await;
                return;
            }
            let _ = send_static(&mut self.tx, OKAY).await;
            return;
        }

        if let Some(len) = parse_download_len(cmd) {
            if !self.download.is_active() {
                let _ = send_static(&mut self.tx, FAIL_NOTOPEN).await;
                return;
            }
            if send_data_header(&mut self.tx, len).await.is_err() {
                self.download.abort().await;
                self.pending_download = None;
                return;
            }

            self.pending_download = Some(len as usize);
            if len == 0 {
                self.pending_download = None;
                let _ = send_static(&mut self.tx, OKAY).await;
            }
            return;
        }

        if cmd.eq_ignore_ascii_case(b"Close") {
            if let Err(err) = self.download.finish().await {
                log::error!("[{udc_name}] FBK close/finish failed: {err}");
                let _ = send_static(&mut self.tx, FAIL_CLOSE).await;
                return;
            }
            self.pending_download = None;
            let _ = send_static(&mut self.tx, OKAY).await;
            return;
        }

        log::warn!("[{udc_name}] unsupported FBK command: {}", String::from_utf8_lossy(cmd));
        let _ = send_static(&mut self.tx, FAIL_CMD).await;
    }

    fn take_recv_buf(&mut self) -> BytesMut {
        self.recv_buf_pool
            .pop()
            .unwrap_or_else(|| BytesMut::with_capacity(self.recv_capacity))
    }

    fn recycle_recv_buf(&mut self, mut buf: BytesMut) {
        buf.clear();
        self.recv_buf_pool.push(buf);
    }
}

fn trim_command(data: &[u8]) -> &[u8] {
    let start = data.iter().position(|b| !matches!(*b, b'\0' | b' ' | b'\t' | b'\r' | b'\n'));
    let Some(start) = start else {
        return &[];
    };
    let end = data
        .iter()
        .rposition(|b| !matches!(*b, b'\0' | b' ' | b'\t' | b'\r' | b'\n'))
        .expect("trim_command start implies non-empty slice");
    &data[start..=end]
}

fn parse_download_len(cmd: &[u8]) -> Option<u32> {
    let payload = cmd
        .strip_prefix(b"donwload:")
        .or_else(|| cmd.strip_prefix(b"download:"))?;
    let payload = std::str::from_utf8(trim_command(payload)).ok()?;
    u32::from_str_radix(payload, 16).ok()
}

async fn send_static(tx: &mut EndpointSender, data: &'static [u8]) -> io::Result<()> {
    tx.send_async(Bytes::from_static(data)).await
}

async fn send_data_header(tx: &mut EndpointSender, len: u32) -> io::Result<()> {
    tx.send_async(Bytes::from(format!("DATA{len:08X}"))).await
}
