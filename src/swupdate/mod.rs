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

    pub(crate) async fn begin(&mut self) -> io::Result<()> {
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
            Some(Transport::Pipe(sink)) => sink.finish(self.params.timeout).await,
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
}
