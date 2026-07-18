//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

//! Backend plumbing for a swupdate transfer: a data queue and a status signal.
//!
//! Two independent halves, wired by two independent tokio tasks:
//!
//! * **Data** — producers feed firmware blocks into a bounded channel; a drain
//!   task writes them to the backend (an IPC socket or `fw_update` stdin),
//!   best-effort. When the producer signals EOF (drops the sender) the drain
//!   closes the writer. Writes never decide the result.
//! * **Status** — a task that reports swupdate's verdict: `Ok` for a normal
//!   finish, `Err` for an abnormal one. For IPC it polls `GET_STATUS`; for the
//!   pipe it is the `fw_update` exit code. It observes only; it never touches
//!   the data stream.

use std::future::Future;
use std::io;
use std::pin::Pin;

use bytes::BytesMut;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::SwupdateParams;

pub(crate) type BoxedWriter = Box<dyn AsyncWrite + Send + Unpin>;
type StatusFuture = Pin<Box<dyn Future<Output = io::Result<()>> + Send>>;
type StatusFn = Box<dyn FnOnce(SwupdateParams) -> StatusFuture + Send>;

/// The two independent halves of a backend: where firmware bytes go, and how
/// the result is observed. Built by [`super::ipc::begin`] / [`super::pipe::begin`].
pub(crate) struct TransportSpec {
    writer: BoxedWriter,
    status: StatusFn,
}

impl TransportSpec {
    pub(crate) fn new(
        writer: BoxedWriter,
        status: impl FnOnce(SwupdateParams) -> StatusFuture + Send + 'static,
    ) -> Self {
        Self {
            writer,
            status: Box::new(status),
        }
    }
}

/// The running tasks and channels for one transfer.
pub(crate) struct Tasks {
    pub(crate) tx: mpsc::Sender<BytesMut>,
    pub(crate) recycle_rx: mpsc::Receiver<BytesMut>,
    pub(crate) status: JoinHandle<io::Result<()>>,
}

/// Spawns the tasks for a transfer. `connect` performs the actual backend
/// setup -- spawning `fw_update` or dialing SWUpdate's IPC socket, both slow
/// -- entirely inside the spawned task, never awaited by the caller: the
/// channel exists and `tx` is usable immediately, so a producer (the DFU
/// request path) can start queueing blocks right away and is never stalled
/// waiting for the connection. Queued blocks simply sit buffered until the
/// connection completes and the drain task starts writing them out.
pub(crate) fn spawn(
    connect: impl Future<Output = io::Result<TransportSpec>> + Send + 'static,
    capacity: usize,
    params: SwupdateParams,
) -> Tasks {
    let (tx, rx) = mpsc::channel(capacity);
    let (recycle_tx, recycle_rx) = mpsc::channel(capacity);
    let status = tokio::spawn(async move {
        let TransportSpec { writer, status } = connect.await?;
        drop(tokio::spawn(drain_data(writer, rx, recycle_tx)));
        status(params).await
    });
    Tasks { tx, recycle_rx, status }
}

/// Drains queued blocks into the writer, best-effort. Stops if the backend
/// stops reading; closes the writer (EOF) once the producer drops the sender.
async fn drain_data(
    mut writer: BoxedWriter,
    mut rx: mpsc::Receiver<BytesMut>,
    recycle_tx: mpsc::Sender<BytesMut>,
) {
    while let Some(buf) = rx.recv().await {
        if writer.write_all(&buf).await.is_err() {
            return; // backend stopped reading; drop the writer.
        }
        let _ = recycle_tx.try_send(buf);
    }
    // Producer signalled EOF (sender dropped): close the writer.
    let _ = writer.shutdown().await;
}
