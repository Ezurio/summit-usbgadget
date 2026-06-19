//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! SWUpdate IPC transport (NAND / A-B boot).
//!
//! Streams firmware directly into the SWUpdate IPC interface and waits for a
//! terminal result on the control status channel.

use std::io;
use std::time::Duration;

use rustix::fs;
use rustix::system::{self, RebootCommand};
use swupdate_ipc::r#async as swu;
use swupdate_ipc::{RunType, SourceType, SwupdateRequest};
use tokio::time::sleep;

use crate::sysinfo::BootRootfsInfo;

const REBOOT_DELAY: Duration = Duration::from_secs(5);

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
    conn: swu::InstallConn,
}

impl IpcSink {
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

        Ok(Self { conn })
    }

    pub(super) async fn write_block(&mut self, data: &[u8]) -> io::Result<()> {
        self.conn
            .send_data(data)
            .await
            .map_err(|e| io::Error::other(format!("SWUpdate send failed: {e}")))
    }

    pub(super) async fn finish(self, timeout: Duration) -> io::Result<()> {
        let Self { conn } = self;
        conn.end()
            .await
            .map_err(|e| io::Error::other(format!("SWUpdate end failed: {e}")))?;
        swu::await_install_result(timeout)
            .await
            .map_err(|e| io::Error::other(format!("SWUpdate wait failed: {e}")))?;
        tokio::spawn(async {
            sleep(REBOOT_DELAY).await;
            log::info!("SWUpdate install succeeded; syncing filesystems before local restart");
            fs::sync();
            if let Err(err) = system::reboot(RebootCommand::Restart) {
                log::error!("local reboot failed: {err}");
            }
        });
        Ok(())
    }

    pub(super) async fn abort(self) {
        // Dropping the install connection closes the socket.
    }
}
