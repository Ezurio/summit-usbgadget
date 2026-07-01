//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! SWUpdate IPC transport (NAND / A-B boot).
//!
//! Streams firmware directly into the SWUpdate IPC interface and waits for a
//! terminal result on the control status channel.

use std::io;
use std::collections::VecDeque;
use std::time::Duration;

use rustix::io::Errno;
use swupdate_ipc::r#async as swu;
use swupdate_ipc::Error as SwupdateError;
use swupdate_ipc::{RunType, SourceType, SwupdateRequest};
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;

use crate::sysinfo::BootRootfsInfo;

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

const MAX_QUEUED_BLOCKS: usize = 100;

/// Parameters for an SWUpdate-backed download.
#[derive(Debug, Clone)]
pub struct SwupdateParams {
    /// SWUpdate `software_set` selection (defaults to `"stable"`).
    pub software_set: Option<String>,
    /// Image-mode label used in the running mode (defaults to `"full"`). The
    /// running mode is `"{image_mode}-{inactive_side}"`, except `"complete"`,
    /// which is a full-disk update with no side suffix.
    pub image_mode: Option<String>,
    /// When `true`, the update is validated but not written (dry run).
    pub dry_run: bool,
    /// When `true`, SWUpdate must not persist the streamed SWU to disk; the
    /// image is processed directly from the stream. Enabled by default so that
    /// no local or temporary copy of the firmware is created.
    pub disable_store_swu: bool,
    /// Maximum time to wait for SWUpdate to report a terminal result.
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

/// Active SWUpdate IPC transport.
pub(super) struct IpcSink {
    conn: Option<swu::InstallConn>,
    writer_task: Option<JoinHandle<io::Result<swu::InstallConn>>>,
    queued_blocks: VecDeque<Vec<u8>>,
    params: SwupdateParams,
}

impl IpcSink {
    fn start_background_write(&mut self, pending: Vec<u8>) -> io::Result<()> {
        let mut conn = self
            .conn
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "SWUpdate install stream closed"))?;
        self.writer_task = Some(tokio::spawn(async move {
            conn.write_all(&pending).await.map_err(normalize_send_io_error)?;
            Ok(conn)
        }));
        Ok(())
    }

    async fn submit_block(&mut self, data: Vec<u8>) -> io::Result<()> {
        let conn = self
            .conn
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "SWUpdate install stream closed"))?;
        match super::try_write_once(conn, &data).await {
            Ok(n) if n == data.len() => Ok(()),
            Ok(n) => self.start_background_write(data[n..].to_vec()),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => self.start_background_write(data),
            Err(err) => Err(normalize_send_io_error(err)),
        }
    }

    async fn await_writer(&mut self) -> io::Result<()> {
        let task = self.writer_task.take().expect("writer task must exist");
        let conn = task.await.map_err(|err| io::Error::other(format!("SWUpdate send task failed: {err}")))??;
        self.conn = Some(conn);

        if let Some(block) = self.queued_blocks.pop_front() {
            self.submit_block(block).await?;
        }

        Ok(())
    }

    async fn poll_writer(&mut self) -> io::Result<()> {
        if self.writer_task.as_ref().is_some_and(JoinHandle::is_finished) {
            self.await_writer().await?;
        }
        Ok(())
    }

    /// Starts the install and opens a streaming connection for firmware blocks.
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
            conn: Some(conn),
            writer_task: None,
            queued_blocks: VecDeque::with_capacity(MAX_QUEUED_BLOCKS),
            params: params.clone(),
        })
    }

    pub(super) async fn write_block(&mut self, data: &[u8]) -> io::Result<()> {
        let mut block = Some(data.to_vec());

        while let Some(data) = block.take() {
            self.poll_progress().await?;
            if self.writer_task.is_some() {
                if self.queued_blocks.len() < MAX_QUEUED_BLOCKS {
                    self.queued_blocks.push_back(data);
                    return Ok(());
                }

                self.await_writer().await?;
                block = Some(data);
                continue;
            }

            self.submit_block(data).await?;
            return Ok(());
        }

        Ok(())
    }

    pub(super) async fn finish(mut self, _timeout: Duration) -> io::Result<()> {
        while self.writer_task.is_some() || !self.queued_blocks.is_empty() {
            if self.writer_task.is_some() {
                self.await_writer().await?;
            } else if let Some(block) = self.queued_blocks.pop_front() {
                self.submit_block(block).await?;
            }
        }

        let conn = match self.conn.take() {
            Some(conn) => conn,
            None => {
                return Err(io::Error::new(io::ErrorKind::NotConnected, "SWUpdate install stream closed"))
            }
        };
        conn.end()
            .await
            .map_err(|err| normalize_ipc_error(err, "end"))?;
        swu::await_install_result(self.params.timeout)
            .await
            .map_err(|err| normalize_ipc_error(err, "wait"))?;
        super::on_update_success(&self.params, false).await;
        Ok(())
    }

    pub(super) async fn abort(self) {
        let Self { conn, writer_task, .. } = self;
        if let Some(task) = writer_task {
            task.abort();
            return;
        }
        if let Some(conn) = conn {
            let mut stream = conn.into_stream();
            let _ = stream.shutdown().await;
        }
    }

    pub(super) fn is_busy(&self) -> bool {
        self.writer_task.is_some() || !self.queued_blocks.is_empty()
    }

    pub(super) async fn poll_progress(&mut self) -> io::Result<()> {
        self.poll_writer().await
    }
}
