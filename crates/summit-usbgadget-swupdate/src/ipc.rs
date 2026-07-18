//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! SWUpdate IPC transport (NAND / A-B boot).
//!
//! Streams firmware directly into the SWUpdate IPC interface and waits for a
//! terminal result on the progress notification socket.

use std::io;

use rustix::io::Errno;
use swupdate_ipc::Error as SwupdateError;
use swupdate_ipc::r#async as swu;
use swupdate_ipc::{InstallMode, InstallRequest, InstallSource};

use super::SwupdateParams;
use crate::stream::TransportSpec;

pub(super) fn normalize_ipc_error(err: SwupdateError, context: &str) -> io::Error {
    match err {
        SwupdateError::Io(io_err)
            if io_err.raw_os_error() == Some(Errno::SHUTDOWN.raw_os_error())
                || io_err.raw_os_error() == Some(Errno::CONNABORTED.raw_os_error()) =>
        {
            io::Error::new(io::ErrorKind::NotConnected, io_err)
        }
        SwupdateError::Closed => io::Error::new(io::ErrorKind::NotConnected, err),
        other => io::Error::other(format!("SWUpdate {context} failed: {other}")),
    }
}

pub(super) async fn begin(
    params: &SwupdateParams,
    running_mode: &str,
) -> io::Result<TransportSpec> {
    let software_set = params
        .software_set
        .clone()
        .unwrap_or_else(|| "stable".to_string());
    let request = InstallRequest {
        software_set,
        running_mode: running_mode.to_owned(),
        source: InstallSource::Local,
        mode: if params.dry_run {
            InstallMode::DryRun
        } else {
            InstallMode::Install
        },
        disable_store_swu: params.disable_store_swu,
    };
    log::info!(
        "SWUpdate software_set={} running_mode={}",
        request.software_set,
        request.running_mode
    );

    let conn = swu::inst_start_request(&request)
        .await
        .map_err(|e| io::Error::other(format!("SWUpdate inst_start failed: {e}")))?;
    log::info!("SWUpdate install started; streaming firmware");

    // Data (the InstallConn socket) and completion (the progress notification
    // socket) are independent; the progress watcher is the sole authority on the
    // result. The callback logs progress; the library owns connect/reconnect and
    // terminal-verdict detection.
    Ok(TransportSpec::new(Box::new(conn), |params| {
        Box::pin(async move {
            swu::await_progress_result_with(params.timeout, |msg| {
                log::debug!(
                    "SWUpdate progress: step {}/{} {} {}% info={}",
                    msg.current_step(),
                    msg.total_steps(),
                    msg.cur_image(),
                    msg.current_percent(),
                    msg.info(),
                );
            })
            .await
            .map_err(|err| normalize_ipc_error(err, "wait"))
        })
    }))
}
