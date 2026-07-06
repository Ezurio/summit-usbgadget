//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! SWUpdate IPC transport (NAND / A-B boot).
//!
//! Streams firmware directly into the SWUpdate IPC interface and waits for a
//! terminal result on the control status channel.

use std::io;

use rustix::io::Errno;
use swupdate_ipc::Error as SwupdateError;
use swupdate_ipc::r#async as swu;
use swupdate_ipc::{RunType, SourceType, SwupdateRequest};

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
    let mut req = SwupdateRequest::prepare();
    req.source = SourceType::Local as i32;
    req.dry_run = if params.dry_run { RunType::DryRun as i32 } else { RunType::Install as i32 };
    req.disable_store_swu = params.disable_store_swu;

    let software_set = params.software_set.clone().unwrap_or_else(|| "stable".to_string());
    req.set_software_set(&software_set);

    req.set_running_mode(running_mode);
    log::info!("SWUpdate software_set={software_set} running_mode={running_mode}");

    let conn = swu::inst_start_ext(&req)
        .await
        .map_err(|e| io::Error::other(format!("SWUpdate inst_start failed: {e}")))?;
    log::info!("SWUpdate install started; streaming firmware");

    // Data (the InstallConn socket) and completion (a GET_STATUS poll on a
    // separate control socket) are independent; the status poll is the sole
    // authority on the result.
    Ok(TransportSpec::new(
        Box::new(conn),
        |params| Box::pin(async move {
            swu::await_install_result(params.timeout)
                .await
                .map_err(|err| normalize_ipc_error(err, "wait"))
        }),
    ))
}
