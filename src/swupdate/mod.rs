//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! SWUpdate firmware sink for DFU downloads.
//!
//! Selects the transport at `begin()` time based on the boot context (cached
//! by [`crate::sysinfo::init`] at startup):
//!
//! * **NAND / A-B boot** — streams directly into the SWUpdate IPC interface
//!   and waits for a terminal result from the progress stream. See [`ipc`].
//! * **SD-card / initramfs boot** — spawns `fw_update -x r -m complete -` and
//!   pipes each block to its stdin. See [`pipe`].

mod ipc;
mod pipe;

use std::io;
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;
use std::time::Duration;

use rustix::fs;
use rustix::system::{self, RebootCommand};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::time::sleep;
use ipc::IpcSink;
use pipe::PipeSink;

pub use ipc::SwupdateParams;

const REBOOT_DELAY: Duration = Duration::from_secs(5);
static RESTART_PENDING: AtomicBool = AtomicBool::new(false);

pub(super) async fn try_write_once<W>(writer: &mut W, buf: &[u8]) -> io::Result<usize>
where
    W: AsyncWrite + Unpin,
{
    poll_fn(|cx| match Pin::new(&mut *writer).poll_write(cx, buf) {
        Poll::Ready(result) => Poll::Ready(result),
        Poll::Pending => Poll::Ready(Err(io::Error::new(io::ErrorKind::WouldBlock, "writer not ready"))),
    })
    .await
}

pub(super) struct PendingWriter<W> {
    writer: W,
    pending: Vec<u8>,
    pending_offset: usize,
}

impl<W> PendingWriter<W> {
    pub(super) fn new(writer: W) -> Self {
        Self { writer, pending: Vec::new(), pending_offset: 0 }
    }

    pub(super) fn is_busy(&self) -> bool {
        self.pending_offset < self.pending.len()
    }

    pub(super) fn into_inner(self) -> W {
        self.writer
    }
}

impl<W> PendingWriter<W>
where
    W: AsyncWrite + Unpin,
{
    pub(super) async fn write_block(&mut self, data: &[u8]) -> io::Result<()> {
        self.poll_progress().await?;
        if self.is_busy() {
            return Ok(());
        }

        match try_write_once(&mut self.writer, data).await {
            Ok(n) if n == data.len() => Ok(()),
            Ok(n) => {
                self.pending.extend_from_slice(&data[n..]);
                self.pending_offset = 0;
                Ok(())
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                self.pending.extend_from_slice(data);
                self.pending_offset = 0;
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    pub(super) async fn poll_progress(&mut self) -> io::Result<()> {
        while self.is_busy() {
            match try_write_once(&mut self.writer, &self.pending[self.pending_offset..]).await {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "writer closed")),
                Ok(n) => {
                    self.pending_offset += n;
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(err) => return Err(err),
            }
        }

        self.pending.clear();
        self.pending_offset = 0;
        Ok(())
    }

    pub(super) async fn flush_pending(&mut self) -> io::Result<()> {
        self.poll_progress().await?;
        if self.is_busy() {
            self.writer.write_all(&self.pending[self.pending_offset..]).await?;
            self.pending.clear();
            self.pending_offset = 0;
        }
        self.writer.flush().await
    }
}

fn is_complete_update(params: &SwupdateParams) -> bool {
    matches!(params.image_mode.as_deref(), Some("complete"))
}

pub(super) async fn on_update_success(params: &SwupdateParams, force_complete: bool) {
    if force_complete || is_complete_update(params) {
        log::info!("SWUpdate complete update succeeded; skipping local restart");
        return;
    }

    RESTART_PENDING.store(true, Ordering::Relaxed);
    log::info!("SWUpdate update succeeded; scheduling local restart in {REBOOT_DELAY:?}");
    drop(tokio::spawn(async {
        sleep(REBOOT_DELAY).await;
        log::info!("SWUpdate reboot delay elapsed; syncing filesystems before local restart");
        fs::sync();
        if let Err(err) = system::reboot(RebootCommand::Restart) {
            log::error!("local reboot failed: {err}");
        }
    }));
}

/// Active transport during a download, selected by the boot context.
enum Transport {
    /// SWUpdate IPC connection plus its progress stream (NAND / A-B boot).
    Ipc(IpcSink),
    /// `fw_update` stdin pipe (SD-card / initramfs boot).
    Pipe(PipeSink),
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ipc(_) => f.write_str("Transport::Ipc(..)"),
            Self::Pipe(_) => f.write_str("Transport::Pipe(..)"),
        }
    }
}

/// A sink that streams the firmware to the appropriate transport based on the
/// boot context and awaits the installation result.
#[derive(Debug)]
pub(crate) struct SwupdateSink {
    params: SwupdateParams,
    transport: Option<Transport>,
}

impl SwupdateSink {
    pub(crate) fn new(params: SwupdateParams) -> Self {
        Self { params, transport: None }
    }

    pub(crate) async fn begin(&mut self) -> io::Result<()> {
        if RESTART_PENDING.load(Ordering::Relaxed) {
            return Err(io::Error::other("restart pending after successful SWUpdate; rejecting new update"));
        }

        let info = crate::sysinfo::boot_info();
        let transport = if info.use_pipe_mode() {
            Transport::Pipe(PipeSink::begin().await?)
        } else {
            Transport::Ipc(IpcSink::begin(&self.params, info).await?)
        };
        self.transport = Some(transport);
        Ok(())
    }

    pub(crate) async fn write_block(&mut self, data: &[u8]) -> io::Result<()> {
        match self.transport.as_mut() {
            Some(Transport::Ipc(sink)) => sink.write_block(data).await,
            Some(Transport::Pipe(sink)) => sink.write_block(data).await,
            None => Err(io::Error::new(io::ErrorKind::NotConnected, "download not started")),
        }
    }

    pub(crate) async fn finish(&mut self) -> io::Result<()> {
        match self.transport.take() {
            Some(Transport::Ipc(sink)) => sink.finish(self.params.timeout).await,
            Some(Transport::Pipe(sink)) => {
                sink.finish(self.params.timeout).await?;
                on_update_success(&self.params, true).await;
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub(crate) async fn abort(&mut self) {
        match self.transport.take() {
            Some(Transport::Ipc(sink)) => sink.abort().await,
            Some(Transport::Pipe(sink)) => sink.abort().await,
            None => {}
        }
    }

    pub(crate) fn is_busy(&self) -> bool {
        match self.transport.as_ref() {
            Some(Transport::Ipc(sink)) => sink.is_busy(),
            Some(Transport::Pipe(sink)) => sink.is_busy(),
            None => false,
        }
    }

    pub(crate) async fn poll_progress(&mut self) -> io::Result<()> {
        match self.transport.as_mut() {
            Some(Transport::Ipc(sink)) => sink.poll_progress().await,
            Some(Transport::Pipe(sink)) => sink.poll_progress().await,
            None => Ok(()),
        }
    }
}
