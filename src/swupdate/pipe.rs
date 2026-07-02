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

use tokio::process::{Child, ChildStdin, Command};

use bytes::BytesMut;

use super::queued_writer::QueuedWriter;

const MAX_QUEUED_BYTES: usize = 512 * 1024;

fn normalize_pipe_error(err: io::Error) -> io::Error {
    if err.kind() == io::ErrorKind::BrokenPipe {
        io::Error::new(io::ErrorKind::NotConnected, err)
    } else {
        err
    }
}

pub(super) struct PipeSink {
    stdin: QueuedWriter<ChildStdin>,
    child: Child,
}

impl PipeSink {
    pub(super) async fn begin() -> io::Result<Self> {
        let image_mode = "complete";
        log::info!("pipe boot: streaming firmware through fw_update -m {image_mode}");
        let mut child = Command::new("fw_update")
            .args(["-x", "r", "-m", image_mode, "-"])
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
        Ok(Self { stdin: QueuedWriter::new(stdin, MAX_QUEUED_BYTES), child })
    }

    pub(super) async fn write_block(&mut self, data: BytesMut) -> io::Result<usize> {
        self.stdin.write_block(data, normalize_pipe_error).await
    }

    pub(super) async fn finish(mut self, timeout: Duration) -> io::Result<()> {
        self.stdin.log_diagnostics("fw_update finish requested");
        while !self.stdin.flush_queued(normalize_pipe_error).await? {
            self.stdin.wait_pending().await.map_err(normalize_pipe_error)?;
        }
        drop(self.stdin.into_inner());
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
        self.stdin.log_diagnostics("fw_update abort requested");
        drop(self.stdin.into_inner());
        let _ = self.child.kill().await;
    }

    pub(super) fn is_busy(&self) -> bool {
        self.stdin.is_busy()
    }

    pub(super) fn should_throttle(&self, reserve_bytes: usize) -> bool {
        self.stdin.should_throttle(reserve_bytes)
    }

    pub(super) async fn poll_progress(&mut self) -> io::Result<()> {
        self.stdin.poll_progress(normalize_pipe_error).await
    }

    pub(super) async fn wait_writable(&mut self) -> io::Result<()> {
        self.stdin.wait_writable(normalize_pipe_error).await
    }
}
