//
// SPDX-License-Identifier: MIT OR Apache-2.0
//

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rustix::fs;
use rustix::system::{self, RebootCommand};
use tokio::time::sleep;

use super::SwupdateParams;

const REBOOT_DELAY: Duration = Duration::from_secs(5);
static RESTART_PENDING: AtomicBool = AtomicBool::new(false);

fn is_complete_update(params: &SwupdateParams) -> bool {
    matches!(params.image_mode.as_deref(), Some("complete"))
}

pub(super) fn reject_if_restart_pending() -> io::Result<()> {
    if RESTART_PENDING.load(Ordering::Relaxed) {
        Err(io::Error::other(
            "restart pending after successful SWUpdate; rejecting new update",
        ))
    } else {
        Ok(())
    }
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