//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! SWUpdate IPC transport (NAND / A-B boot).
//!
//! Streams firmware directly into the SWUpdate IPC interface and waits for a
//! terminal result on the control status channel.

use std::io;
use std::time::Duration;

use bytes::BytesMut;
use rustix::io::Errno;
use swupdate_ipc::r#async as swu;
use swupdate_ipc::Error as SwupdateError;
use swupdate_ipc::{RunType, SourceType, SwupdateRequest};
use tokio::io::AsyncWriteExt;

use crate::sysinfo::BootRootfsInfo;

use super::queued_writer::QueuedWriter;

const MAX_QUEUED_BYTES: usize = 512 * 1024;

fn normalize_ipc_error(err: SwupdateError, context: &str) -> io::Error {
    match err {
        SwupdateError::Io(io_err)
            if io_err.raw_os_error() == Some(Errno::SHUTDOWN.raw_os_error())
                || io_err.raw_os_error() == Some(Errno::CONNABORTED.raw_os_error()) =>
        {
            io::Error::new(io::ErrorKind::NotConnected, io_err)
        }
        SwupdateError::Closed => io::Error::new(io::ErrorKind::NotConnected, err),
        other => io::Error::other(format!("SWUpdate {context} failed: {other}")),
    }
}

fn normalize_send_io_error(err: io::Error) -> io::Error {
    normalize_ipc_error(SwupdateError::from(err), "send")
}

#[derive(Debug, Clone)]
pub struct SwupdateParams {
    pub software_set: Option<String>,
    pub image_mode: Option<String>,
    pub dry_run: bool,
    pub disable_store_swu: bool,
    pub timeout: Duration,
}

impl Default for SwupdateParams {
    fn default() -> Self {
        Self {
            software_set: None,
            image_mode: None,
            dry_run: false,
            disable_store_swu: true,
            timeout: Duration::from_secs(120),
        }
    }
}

pub(super) struct IpcSink {
    writer: Option<QueuedWriter<swu::InstallConn>>,
    params: SwupdateParams,
}

impl IpcSink {
    fn writer_mut(&mut self) -> io::Result<&mut QueuedWriter<swu::InstallConn>> {
        self.writer
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "SWUpdate install stream closed"))
    }

    pub(super) async fn begin(params: &SwupdateParams, info: &BootRootfsInfo) -> io::Result<Self> {
        let mut req = SwupdateRequest::prepare();
        req.source = SourceType::Local as i32;
        req.dry_run = if params.dry_run { RunType::DryRun as i32 } else { RunType::Install as i32 };
        req.disable_store_swu = params.disable_store_swu;

        let software_set = params.software_set.clone().unwrap_or_else(|| "stable".to_string());
        req.set_software_set(&software_set);

        let image_mode = params.image_mode.as_deref().unwrap_or("full");
        let running_mode =
            format!("{image_mode}-{}", crate::sysinfo::inactive_side(info.current_side_option()));
        req.set_running_mode(&running_mode);
        log::info!("SWUpdate software_set={software_set} running_mode={running_mode}");

        let conn = swu::inst_start_ext(&req)
            .await
            .map_err(|e| io::Error::other(format!("SWUpdate inst_start failed: {e}")))?;
        log::info!("SWUpdate install started; streaming firmware");

        Ok(Self {
            writer: Some(QueuedWriter::new(conn, MAX_QUEUED_BYTES)),
            params: params.clone(),
        })
    }

    pub(super) async fn write_block(&mut self, data: BytesMut) -> io::Result<usize> {
        self.writer_mut()?.write_block(data, normalize_send_io_error).await
    }

    pub(super) async fn finish(mut self, _timeout: Duration) -> io::Result<()> {
        QueuedWriter::log_diagnostics_or_default(self.writer.as_ref(), "SWUpdate finish requested");

        let mut writer = match self.writer.take() {
            Some(writer) => writer,
            None => {
                return Err(io::Error::new(io::ErrorKind::NotConnected, "SWUpdate install stream closed"))
            }
        };
        while !writer.flush_queued(normalize_send_io_error).await? {
            writer.wait_pending().await.map_err(normalize_send_io_error)?;
        }
        let conn = writer.into_inner();
        conn.end().await.map_err(|err| normalize_ipc_error(err, "end"))?;
        swu::await_install_result(self.params.timeout)
            .await
            .map_err(|err| normalize_ipc_error(err, "wait"))?;
        super::restart::on_update_success(&self.params, false).await;
        Ok(())
    }

    pub(super) async fn abort(self) {
        QueuedWriter::log_diagnostics_or_default(self.writer.as_ref(), "SWUpdate abort requested");
        let Self { writer, .. } = self;
        if let Some(writer) = writer {
            let conn = writer.into_inner();
            let mut stream = conn.into_stream();
            let _ = stream.shutdown().await;
        }
    }

    pub(super) fn is_busy(&self) -> bool {
        self.writer.as_ref().is_some_and(QueuedWriter::is_busy)
    }

    pub(super) fn should_throttle(&self, reserve_bytes: usize) -> bool {
        self.writer
            .as_ref()
            .is_some_and(|writer| writer.should_throttle(reserve_bytes))
    }

    pub(super) async fn poll_progress(&mut self) -> io::Result<()> {
        self.writer_mut()?.poll_progress(normalize_send_io_error).await
    }

    pub(super) async fn wait_writable(&mut self) -> io::Result<()> {
        self.writer_mut()?.wait_writable(normalize_send_io_error).await
    }
}
