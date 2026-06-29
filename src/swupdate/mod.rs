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
use std::time::Duration;

use rustix::fs;
use rustix::system::{self, RebootCommand};
use tokio::time::sleep;
use ipc::IpcSink;
use pipe::PipeSink;

pub use ipc::SwupdateParams;

const REBOOT_DELAY: Duration = Duration::from_secs(5);

fn is_complete_update(params: &SwupdateParams) -> bool {
    matches!(params.image_mode.as_deref(), Some("complete"))
}

pub(super) async fn on_update_success(params: &SwupdateParams, force_complete: bool) {
    if force_complete || is_complete_update(params) {
        log::info!("SWUpdate complete update succeeded; skipping local restart");
        return;
    }

    log::info!("SWUpdate update succeeded; scheduling local restart in {REBOOT_DELAY:?}");
    tokio::spawn(async {
        sleep(REBOOT_DELAY).await;
        log::info!("SWUpdate reboot delay elapsed; syncing filesystems before local restart");
        fs::sync();
        if let Err(err) = system::reboot(RebootCommand::Restart) {
            log::error!("local reboot failed: {err}");
        }
    });
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
}
