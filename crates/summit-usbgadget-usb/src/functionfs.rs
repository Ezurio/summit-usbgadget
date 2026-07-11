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

use tokio::io::{unix::AsyncFd, Interest};
use usb_gadget::function::custom::{Custom, Event};

/// Returns `true` when an error means the FunctionFS transport was torn down
/// (endpoint disabled, gadget unbound), so the caller should stop rather than
/// treat it as a hard failure.
pub fn is_closed_transport_error(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::NotConnected
        || err.raw_os_error() == Some(rustix::io::Errno::IDRM.raw_os_error())
        || err.raw_os_error() == Some(rustix::io::Errno::SHUTDOWN.raw_os_error())
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
        match events.next().await {
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
            Err(err) if is_closed_transport_error(&err) => {
                log::info!("[{udc_name}] {label} endpoint zero closed, stopping");
                return;
            }
            Err(err) => log::debug!("[{udc_name}] {label} event error: {err}"),
        }
    }
}
