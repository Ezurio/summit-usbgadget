//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

//! SWUpdate firmware sink for streamed update payloads.

mod ipc;
mod pipe;
mod restart;
mod stream;

pub mod sysinfo;

use std::error::Error;
use std::fmt;
use std::future::poll_fn;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::BytesMut;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct SwupdateConfig {
    pub download: Option<String>,
    pub software_set: Option<String>,
    pub image_mode: Option<String>,
    pub dry_run: Option<bool>,
    pub disable_store_swu: Option<bool>,
    pub timeout_secs: Option<u64>,
}

impl SwupdateConfig {
    pub fn to_params(&self) -> Result<SwupdateParams, SwupdateConfigError> {
        self.ensure_download_target()?;
        Ok(SwupdateParams {
            software_set: self.software_set.clone(),
            image_mode: self.image_mode.clone(),
            dry_run: self.dry_run.unwrap_or(false),
            disable_store_swu: self.disable_store_swu.unwrap_or(true),
            timeout: Duration::from_secs(self.timeout_secs.unwrap_or(600)),
        })
    }

    fn ensure_download_target(&self) -> Result<(), SwupdateConfigError> {
        match self.download.as_deref() {
            None | Some("swupdate") => Ok(()),
            Some(other) => Err(SwupdateConfigError::UnsupportedDownloadTarget(other.to_string())),
        }
    }
}

#[derive(Debug)]
pub enum SwupdateConfigError {
    UnsupportedDownloadTarget(String),
}

impl fmt::Display for SwupdateConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedDownloadTarget(target) => {
                write!(f, "unsupported download target {target:?}; expected \"swupdate\"")
            }
        }
    }
}

impl Error for SwupdateConfigError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwupdateParams {
    pub software_set: Option<String>,
    pub image_mode: Option<String>,
    pub dry_run: bool,
    pub disable_store_swu: bool,
    pub timeout: Duration,
}

impl Default for SwupdateParams {
    fn default() -> Self {
        Self {
            software_set: None,
            image_mode: None,
            dry_run: false,
            disable_store_swu: true,
            timeout: Duration::from_secs(600),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EffectiveUpdateType {
    Complete,
    Slot { running_mode: String },
}

impl EffectiveUpdateType {
    pub(crate) fn resolve(params: &SwupdateParams, info: &sysinfo::BootRootfsInfo) -> Self {
        if info.is_running_on_sd() || info.is_running_on_initramfs() {
            Self::Complete
        } else {
            let image_mode = params.image_mode.as_deref().unwrap_or("full");
            let inactive_side = sysinfo::inactive_side(info.current_side_option());
            Self::Slot {
                running_mode: format!("{image_mode}-{inactive_side}"),
            }
        }
    }

    pub(crate) fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }
}

/// Target amount of in-flight firmware to keep buffered between the producer
/// and the backend writer. The channel capacity is derived from this and the
/// transport's block size, so total buffered memory stays near this bound
/// regardless of how large each block is.
const TARGET_QUEUE_BYTES: usize = 2 * 1024 * 1024;

/// Number of blocks to allow in the queue so that `blocks * block_size` stays
/// near [`TARGET_QUEUE_BYTES`]. At least 2 so a block can be written while the
/// next one waits.
fn queue_capacity(block_size: usize) -> usize {
    TARGET_QUEUE_BYTES.div_ceil(block_size.max(1)).max(2)
}

/// Connects to the backend (spawns `fw_update` or dials SWUpdate's IPC
/// socket). Slow, and deliberately a plain async fn so a caller can hand off
/// the resulting future to be driven by a spawned task instead of awaiting it
/// directly.
async fn connect_backend(use_pipe: bool, params: SwupdateParams, running_mode: String) -> io::Result<stream::TransportSpec> {
    if use_pipe {
        pipe::begin(&params, &running_mode).await
    } else {
        ipc::begin(&params, &running_mode).await
    }
}

/// Result of feeding a firmware block into the queue.
pub enum Feed {
    /// Queued (its length in bytes).
    Ok(usize),
    /// The queue is full; the rejected block is handed back (from `try_send`).
    Full(BytesMut),
    /// swupdate stopped consuming (it finished or failed). The data side is
    /// done; observe [`SwupdateSession::finished`] for the verdict. This is not
    /// itself an error.
    Closed,
}

/// How a producer-driven SWUpdate pump ended.
pub enum PumpToSwupdateEnd {
    /// The producer had no more blocks to provide.
    InputClosed,
    /// SWUpdate reached a terminal verdict while the producer was still active.
    SwupdateFinished(io::Result<()>),
}

/// Boxed future returned by a producer callback that yields the next block.
pub type NextSwupdateBlock<'a> = Pin<Box<dyn Future<Output = io::Result<Option<BytesMut>>> + Send + 'a>>;

/// Producer of reusable SWUpdate blocks.
pub trait SwupdatePumpSource {
    fn next_block<'a>(&'a mut self, sink: &'a mut SwupdateSession) -> NextSwupdateBlock<'a>;
}

/// A swupdate transfer.
///
/// Three independent concerns, deliberately not wired to each other:
///
/// * **Feed** ([`Self::send`] / [`Self::try_send`]) pushes firmware blocks into
///   the queue. [`Self::eof`] signals "no more data". This is the *only* side
///   that touches the data stream.
/// * **Status** ([`Self::finished`] / [`Self::try_finished`]) reports swupdate's
///   verdict — `Ok` for a normal finish, `Err` for an abort. It only observes;
///   it never touches the data stream.
/// * **Destroy** ([`Self::abort`]) tears everything down immediately.
pub struct SwupdateSession {
    params: SwupdateParams,
    /// Largest block the producer will send; sets the queue capacity so the
    /// buffered memory stays near [`TARGET_QUEUE_BYTES`].
    max_block_size: usize,
    update_type: Option<EffectiveUpdateType>,
    tx: Option<mpsc::Sender<BytesMut>>,
    recycle_rx: Option<mpsc::Receiver<BytesMut>>,
    status: Option<JoinHandle<io::Result<()>>>,
    terminal: Option<Result<(), String>>,
}

impl fmt::Debug for SwupdateSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SwupdateSession")
            .field("params", &self.params)
            .field("open", &self.is_open())
            .finish_non_exhaustive()
    }
}

impl SwupdateSession {
    pub fn new(params: SwupdateParams, max_block_size: usize) -> Self {
        Self {
            params,
            max_block_size,
            update_type: None,
            tx: None,
            recycle_rx: None,
            status: None,
            terminal: None,
        }
    }

    /// Whether a transfer is currently active (feeding or awaiting its verdict).
    pub fn is_open(&self) -> bool {
        self.tx.is_some() || self.status.is_some()
    }

    /// Borrows a reusable buffer returned by the drain task, or allocates one.
    pub fn buffer(&mut self, capacity: usize) -> BytesMut {
        let mut buf = self
            .recycle_rx
            .as_mut()
            .and_then(|rx| rx.try_recv().ok())
            .unwrap_or_else(|| BytesMut::with_capacity(capacity));
        if buf.capacity() < capacity {
            buf.reserve(capacity - buf.capacity());
        }
        buf.clear();
        buf
    }

    /// Sets up the queue and hands it to the caller immediately; connecting to
    /// the backend (spawning `fw_update` or dialing SWUpdate's IPC socket) is
    /// slow, so it happens inside the spawned task instead -- this never
    /// blocks, so callers on the DFU request path never stall on it.
    pub fn open(&mut self) -> io::Result<()> {
        if self.is_open() {
            return Ok(());
        }
        self.terminal = None;
        restart::reject_if_restart_pending()?;

        let info = sysinfo::boot_info();
        let update_type = EffectiveUpdateType::resolve(&self.params, info);
        let running_mode = match &update_type {
            EffectiveUpdateType::Complete => "complete".to_string(),
            EffectiveUpdateType::Slot { running_mode } => running_mode.clone(),
        };
        log::warn!(
            "SwupdateSession::open pipe_mode={} image_mode={:?} software_set={:?} running_mode={}",
            info.use_pipe_mode(),
            self.params.image_mode,
            self.params.software_set,
            running_mode,
        );
        self.update_type = Some(update_type);

        let connect = connect_backend(info.use_pipe_mode(), self.params.clone(), running_mode);
        let tasks = stream::spawn(connect, queue_capacity(self.max_block_size), self.params.clone());
        self.tx = Some(tasks.tx);
        self.recycle_rx = Some(tasks.recycle_rx);
        self.status = Some(tasks.status);
        Ok(())
    }

    fn ensure_open(&mut self) -> io::Result<()> {
        if self.tx.is_some() || self.status.is_some() || self.terminal.is_some() {
            return Ok(());
        }
        self.open()
    }

    // --- Feed side: the only methods that touch the data stream. ---

    /// Tries to enqueue a block without blocking.
    pub async fn try_send(&mut self, item: BytesMut) -> io::Result<Feed> {
        self.ensure_open()?;
        let len = item.len();
        let result = match self.tx.as_ref() {
            Some(tx) => tx.try_send(item),
            None => return Ok(Feed::Closed),
        };
        Ok(match result {
            Ok(()) => Feed::Ok(len),
            Err(mpsc::error::TrySendError::Full(item)) => Feed::Full(item),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.tx = None;
                Feed::Closed
            }
        })
    }

    /// Enqueues a block, awaiting a free slot.
    pub async fn send(&mut self, item: BytesMut) -> io::Result<Feed> {
        self.ensure_open()?;
        let len = item.len();
        let outcome = match self.tx.as_ref() {
            Some(tx) => tx.send(item).await,
            None => return Ok(Feed::Closed),
        };
        Ok(match outcome {
            Ok(()) => Feed::Ok(len),
            Err(_) => {
                self.tx = None;
                Feed::Closed
            }
        })
    }

    /// Signals that no more data will be sent: closes the queue so the drain
    /// task closes the backend writer (EOF). Independent of the status signal.
    pub fn eof(&mut self) {
        self.tx = None;
    }

    // --- Status side: pure observation; never touches the data stream. ---

    fn poll_status(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(terminal) = &self.terminal {
            return Poll::Ready(terminal.clone().map_err(io::Error::other));
        }
        let Some(task) = self.status.as_mut() else {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::NotConnected, "no update in progress")));
        };
        match Pin::new(task).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(joined) => {
                self.status = None;
                Poll::Ready(self.settle(joined))
            }
        }
    }

    /// Awaits swupdate's verdict: `Ok` = finished, `Err` = aborted (failed).
    pub async fn finished(&mut self) -> io::Result<()> {
        poll_fn(|cx| self.poll_status(cx)).await
    }

    /// Non-blocking observation of the verdict. `None` = swupdate still working.
    pub fn try_finished(&mut self) -> Option<io::Result<()>> {
        let waker = futures_util::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        match self.poll_status(&mut cx) {
            Poll::Ready(result) => Some(result),
            Poll::Pending => None,
        }
    }

    // --- Destruction: full immediate teardown (USB abort or swupdate abort). ---

    /// Tears the session down for the caller's purposes without leaving
    /// SWUpdate's own install stuck. Signals EOF the same way a normal finish
    /// does (drops the sender) instead of aborting the drain task outright:
    /// `drain_data` (stream.rs) only calls `writer.shutdown()` on the SWUpdate
    /// connection when it observes EOF and returns normally — aborting the
    /// task throws it away mid-write without ever notifying SWUpdate, leaving
    /// its in-progress install stuck waiting for more data that will never
    /// arrive (seen on hardware: a retried transfer stalls again because the
    /// *previous* install was never told to stop). The data task is left
    /// running detached (same as a normal finish never joins it) so a
    /// still-writing/stuck backend can't block this call; only the
    /// status-observer task (which never touches the data stream) is aborted.
    pub async fn abort(&mut self) {
        self.tx = None;
        self.recycle_rx = None;
        if let Some(task) = self.status.take() {
            task.abort();
            let _ = task.await;
        }
        self.terminal = None;
    }

    /// Folds a joined status-task outcome into cached terminal state.
    fn settle(&mut self, joined: Result<io::Result<()>, tokio::task::JoinError>) -> io::Result<()> {
        match joined {
            Ok(Ok(())) => {
                self.mark_complete();
                Ok(())
            }
            Ok(Err(err)) => {
                self.terminal = Some(Err(err.to_string()));
                Err(err)
            }
            Err(join) => {
                let err = io::Error::other(format!("SWUpdate status task failed: {join}"));
                self.terminal = Some(Err(err.to_string()));
                Err(err)
            }
        }
    }

    fn mark_complete(&mut self) {
        self.terminal = Some(Ok(()));
        if let Some(update_type) = self.update_type.clone() {
            drop(tokio::spawn(async move {
                restart::on_update_success(&update_type).await;
            }));
        }
    }
}

/// Repeatedly reads producer blocks and forwards them into `sink` until the
/// producer is exhausted or SWUpdate reaches a verdict.
pub async fn pump_to_swupdate<S>(
    sink: &mut SwupdateSession,
    source: &mut S,
) -> io::Result<PumpToSwupdateEnd>
where
    S: SwupdatePumpSource,
{
    loop {
        let Some(block) = source.next_block(sink).await? else {
            return Ok(PumpToSwupdateEnd::InputClosed);
        };

        if sink.is_open()
            && let Some(result) = sink.try_finished()
        {
            return Ok(PumpToSwupdateEnd::SwupdateFinished(result));
        }

        if matches!(sink.send(block).await?, Feed::Closed) {
            return Ok(PumpToSwupdateEnd::SwupdateFinished(sink.finished().await));
        }
    }
}
