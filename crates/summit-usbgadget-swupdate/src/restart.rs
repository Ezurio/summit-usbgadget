//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rustix::fs;
use rustix::system::{self, RebootCommand};
use tokio::time::sleep;

use super::EffectiveUpdateType;

const REBOOT_DELAY: Duration = Duration::from_secs(2);
static RESTART_PENDING: AtomicBool = AtomicBool::new(false);

pub(super) fn reject_if_restart_pending() -> io::Result<()> {
    if RESTART_PENDING.load(Ordering::Relaxed) {
        Err(io::Error::other(
            "restart pending after successful SWUpdate; rejecting new update",
        ))
    } else {
        Ok(())
    }
}

pub(super) async fn on_update_success(update_type: &EffectiveUpdateType) {
    if update_type.is_complete() {
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
