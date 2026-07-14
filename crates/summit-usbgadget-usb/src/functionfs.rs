//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! Shared FunctionFS endpoint-zero helpers.
//!
//! Every custom function (DFU, fastboot-usb, …) drives its control channel the
//! same way: park on endpoint-zero readiness, then read the queued event.
//! [`Ep0Events`] owns that one primitive so each function only has to decide
//! what to do with the event.

use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

use tokio::io::{unix::AsyncFd, Interest};
use tokio::time::timeout;
use usb_gadget::function::custom::{Custom, Event};

/// Returns `true` when an error means the FunctionFS transport was permanently
/// torn down (endpoint disabled, gadget unbound), so the caller should stop
/// rather than retry.
///
/// Deliberately does **not** include `EIDRM`: see [`is_setup_superseded_error`].
fn is_torn_down_transport_error(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::NotConnected
        || err.kind() == io::ErrorKind::BrokenPipe
        || err.raw_os_error() == Some(rustix::io::Errno::SHUTDOWN.raw_os_error())
}

/// Returns `true` when an error means the host superseded the control request
/// we were servicing with a newer `SETUP` packet before we replied to it (e.g.
/// after a stall-and-retry).
///
/// FunctionFS's ep0 reports this as `EIDRM` (see `ffs_ep0_read`/`ffs_ep0_write`
/// in the kernel's `f_fs.c`: both check `ffs_setup_state_clear_cancelled()`
/// before anything else, and it applies to *every* read/write on ep0 --
/// including the read used to fetch the next event). It is a normal,
/// recoverable occurrence, not a sign the transport is gone, so it must never
/// be treated the same as [`is_torn_down_transport_error`].
fn is_setup_superseded_error(err: &io::Error) -> bool {
    err.raw_os_error() == Some(rustix::io::Errno::IDRM.raw_os_error())
}

/// Returns `true` when an error means the current control request was
/// cancelled or the FunctionFS transport was torn down, so the caller should
/// give up on this request/transfer without treating it as a hard failure.
pub fn is_closed_transport_error(err: &io::Error) -> bool {
    is_torn_down_transport_error(err) || is_setup_superseded_error(err)
}

/// Control-event source for one bound custom function.
///
/// Holds the readiness registration for the gadget's endpoint-zero fd alongside
/// its [`Custom`], and yields events with [`Ep0Events::next`]: it parks on
/// readiness and only then reads, so it never blocks the runtime. (It cannot be
/// a `Stream` because each [`Event`] borrows the `Custom`.)
pub struct Ep0Events<'c> {
    ep0: AsyncFd<RawFd>,
    custom: &'c mut Custom,
}

impl<'c> Ep0Events<'c> {
    /// Registers the gadget's endpoint zero for readiness polling.
    pub fn new(custom: &'c mut Custom) -> io::Result<Self> {
        let ep0 = AsyncFd::with_interest(custom.fd()?, Interest::READABLE)?;
        Ok(Self { ep0, custom })
    }

    /// Waits for the next endpoint-zero event, then reads it.
    ///
    /// Readiness firing guarantees a complete FunctionFS event is queued, so the
    /// read cannot block: this is the whole "wait on poll, then read" loop.
    #[allow(clippy::should_implement_trait)]
    pub async fn next(&mut self) -> io::Result<Event<'_>> {
        let mut guard = self.ep0.readable().await?;
        guard.clear_ready();
        self.custom.event()
    }
}

/// Handles FunctionFS control events for a custom function.
///
/// Implement this and hand it to [`serve`] to get the whole endpoint-zero loop
/// for free — the function only has to route each event.
#[allow(async_fn_in_trait)]
pub trait EventHandler {
    async fn handle_event(&mut self, udc_name: &str, event: Event<'_>) -> io::Result<()>;

    /// Idle timeout applied to the *next* `events.next()` wait; `None` (the
    /// default) waits indefinitely. Re-evaluated every loop iteration, so a
    /// handler turns this on/off simply by deriving it from its own state
    /// (e.g. only while a transfer is in progress).
    fn idle_timeout(&self) -> Option<Duration> {
        None
    }

    /// Called when `idle_timeout` elapses with no event. Default: no-op.
    async fn on_idle_timeout(&mut self, _udc_name: &str) {}
}

/// Runs the shared endpoint-zero event loop until the endpoint is torn down,
/// dispatching each event to `handler` — the generic "park on ep0, hand the
/// event to a callback" control loop. Pure-control functions (e.g. DFU) do all
/// their work in the handler; functions with a separate bulk data plane (e.g.
/// fastboot-usb) use the handler to publish control state (over a channel) that
/// their own data loop observes.
pub async fn serve<H>(udc_name: String, label: &str, custom: &mut Custom, handler: &mut H)
where
    H: EventHandler,
{
    log::info!("[{udc_name}] servicing {label}");
    let mut events = match Ep0Events::new(custom) {
        Ok(events) => events,
        Err(err) => {
            log::error!("[{udc_name}] {label} endpoint zero unavailable: {err}");
            return;
        }
    };
    loop {
        let step = match handler.idle_timeout() {
            Some(limit) => timeout(limit, events.next())
                .await
                .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "endpoint zero idle"))),
            None => events.next().await,
        };
        match step {
            Ok(event) => {
                if let Err(err) = handler.handle_event(&udc_name, event).await {
                    if is_closed_transport_error(&err) {
                        log::info!("[{udc_name}] {label} handler saw closed transport: {err}");
                    } else {
                        log::error!("[{udc_name}] error handling {label} event: {err}");
                    }
                }
            }
            // Endpoint zero is gone (gadget unbound / function torn down): stop
            // the loop so the task ends deterministically instead of spinning.
            Err(err) if is_torn_down_transport_error(&err) => {
                log::info!("[{udc_name}] {label} endpoint zero closed, stopping");
                return;
            }
            // The setup we were about to fetch details for was superseded by a
            // newer one (e.g. the host stalled and retried): retry rather than
            // stopping, the next iteration will read the new event.
            Err(err) if is_setup_superseded_error(&err) => {
                log::debug!("[{udc_name}] {label} setup request superseded, retrying: {err}");
            }
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                log::warn!("[{udc_name}] {label} idle timeout");
                handler.on_idle_timeout(&udc_name).await;
            }
            Err(err) => log::debug!("[{udc_name}] {label} event error: {err}"),
        }
    }
}
