//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! `fw_update` pipe transport (SD-card / initramfs boot).
//!
//! Spawns `fw_update -x r -m complete -` and pipes each block to its stdin,
//! mirroring the pipe mode in `summit-rcm`. The SWUpdate IPC socket is not used
//! in this path.

use std::io;
use std::process::Stdio;

use tokio::process::Command;
use tokio::time::timeout;

use super::SwupdateParams;
use crate::stream::TransportSpec;

pub(super) async fn begin(_params: &SwupdateParams, running_mode: &str) -> io::Result<TransportSpec> {
    log::info!("pipe boot: streaming firmware through fw_update -m {running_mode}");
    let mut child = Command::new("fw_update")
        .args(["-x", "r", "-m", running_mode, "-"])
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| io::Error::other(format!("fw_update spawn failed: {e}")))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("failed to open fw_update stdin"))?;

    // Data (stdin) and completion (the process exit code) are independent. The
    // drain task closes stdin when the producer is done; the completion task
    // owns the child and waits for it to exit — `kill_on_drop` tears it down if
    // that task is aborted.
    Ok(TransportSpec::new(
        Box::new(stdin),
        move |params| Box::pin(async move {
            match timeout(params.timeout, child.wait()).await {
                Ok(Ok(status)) if status.success() => {
                    log::info!("fw_update completed successfully");
                    Ok(())
                }
                Ok(Ok(status)) => Err(io::Error::other(format!("fw_update exited with status {status}"))),
                Ok(Err(err)) => Err(io::Error::other(format!("fw_update wait failed: {err}"))),
                Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "fw_update timed out")),
            }
        }),
    ))
}
