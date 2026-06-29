//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! `fw_update` pipe transport (SD-card / initramfs boot).
//!
//! Spawns `fw_update -x r -m complete -` and pipes each block to its stdin,
//! mirroring the pipe mode in `summit-rcm`. The SWUpdate IPC socket is not used
//! in this path.

use std::io;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};

fn normalize_pipe_error(err: io::Error) -> io::Error {
    if err.kind() == io::ErrorKind::BrokenPipe {
        io::Error::new(io::ErrorKind::NotConnected, err)
    } else {
        err
    }
}

/// Active `fw_update` pipe transport: stdin handle plus the child process.
pub(super) struct PipeSink {
    stdin: ChildStdin,
    child: Child,
}

impl PipeSink {
    pub(super) async fn begin() -> io::Result<Self> {
        // fw_update rejects any method other than "complete" in these environments.
        let image_mode = "complete";
        log::info!("pipe boot: streaming firmware through fw_update -m {image_mode}");
        let mut child = Command::new("fw_update")
            .args(["-x", "r", "-m", image_mode, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| io::Error::other(format!("fw_update spawn failed: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("failed to open fw_update stdin"))?;
        Ok(Self { stdin, child })
    }

    pub(super) async fn write_block(&mut self, data: &[u8]) -> io::Result<()> {
        self.stdin.write_all(data).await.map_err(normalize_pipe_error)
    }

    pub(super) async fn finish(mut self, timeout: Duration) -> io::Result<()> {
        self.stdin.flush().await.map_err(normalize_pipe_error)?;
        drop(self.stdin); // EOF → fw_update
        match tokio::time::timeout(timeout, self.child.wait()).await {
            Ok(Ok(s)) if s.success() => {
                log::info!("fw_update completed successfully");
                Ok(())
            }
            Ok(Ok(_)) => Err(io::Error::other("fw_update failed")),
            Ok(Err(e)) => Err(io::Error::other(format!("fw_update wait failed: {e}"))),
            Err(_) => {
                let _ = self.child.kill().await;
                Err(io::Error::new(io::ErrorKind::TimedOut, "timed out waiting for fw_update"))
            }
        }
    }

    pub(super) async fn abort(mut self) {
        drop(self.stdin);
        let _ = self.child.kill().await;
    }
}
