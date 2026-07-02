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
mod queued_writer;
mod restart;

use std::io;
use bytes::BytesMut;
use ipc::IpcSink;
use pipe::PipeSink;

pub use ipc::SwupdateParams;

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

    pub(crate) fn is_active(&self) -> bool {
        self.transport.is_some()
    }

    pub(crate) async fn begin(&mut self) -> io::Result<()> {
        if self.transport.is_some() {
            return Ok(());
        }

        restart::reject_if_restart_pending()?;

        let info = crate::sysinfo::boot_info();
        log::warn!(
            "SwupdateSink::begin pipe_mode={} image_mode={:?} software_set={:?}",
            info.use_pipe_mode(),
            self.params.image_mode,
            self.params.software_set
        );
        let transport = if info.use_pipe_mode() {
            Transport::Pipe(PipeSink::begin().await?)
        } else {
            Transport::Ipc(IpcSink::begin(&self.params, info).await?)
        };
        self.transport = Some(transport);
        Ok(())
    }

    pub(crate) async fn write_block(&mut self, data: BytesMut) -> io::Result<()> {
        self.begin().await?;

        match self.transport.as_mut() {
            Some(Transport::Ipc(sink)) => sink.write_block(data).await.map(|_| ()),
            Some(Transport::Pipe(sink)) => sink.write_block(data).await.map(|_| ()),
            None => Err(io::Error::new(io::ErrorKind::NotConnected, "download not started")),
        }
    }

    pub(crate) async fn write_block_if_active(&mut self, data: BytesMut) -> io::Result<usize> {
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
                restart::on_update_success(&self.params, true).await;
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

    pub(crate) fn should_throttle(&self, reserve_bytes: usize) -> bool {
        match self.transport.as_ref() {
            Some(Transport::Ipc(sink)) => sink.should_throttle(reserve_bytes),
            Some(Transport::Pipe(sink)) => sink.should_throttle(reserve_bytes),
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

    pub(crate) async fn wait_writable(&mut self) -> io::Result<()> {
        match self.transport.as_mut() {
            Some(Transport::Ipc(sink)) => sink.wait_writable().await,
            Some(Transport::Pipe(sink)) => sink.wait_writable().await,
            None => Ok(()),
        }
    }
}
