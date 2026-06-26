//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! Shared FunctionFS custom-function event loop helpers.

use std::io;

use usb_gadget::function::custom::{Custom, Event};

fn is_terminal_event_error(err: &io::Error) -> bool {
    matches!(err.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::NotConnected)
        || err.raw_os_error() == Some(43)
}

/// Event handler for a FunctionFS custom function.
pub(crate) trait EventHandler {
    async fn handle_event(&mut self, udc_name: &str, event: Event<'_>) -> io::Result<()>;
}

/// Runs a standard FunctionFS event loop for a custom function.
pub(crate) async fn serve<H>(
    udc_name: String,
    label: &str,
    custom: &mut Custom,
    handler: &mut H,
) where
    H: EventHandler,
{
    log::info!("[{udc_name}] servicing {label}");
    loop {
        if let Err(err) = custom.wait_event().await {
            if is_terminal_event_error(&err) {
                log::info!("[{udc_name}] stopping {label}: {err}");
                break;
            }

            log::debug!("[{udc_name}] {label} wait_event error: {err}");
            continue;
        }

        let event = match custom.event() {
            Ok(event) => event,
            Err(err) => {
                if is_terminal_event_error(&err) {
                    log::info!("[{udc_name}] stopping {label}: {err}");
                    break;
                }

                log::error!("[{udc_name}] {label} event error: {err}");
                continue;
            }
        };

        if let Err(err) = handler.handle_event(&udc_name, event).await {
            log::error!("[{udc_name}] error handling {label} event: {err}");
        }
    }
}