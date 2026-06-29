//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! Shared FunctionFS custom-function event loop helpers.

use std::io;

use usb_gadget::function::custom::{Custom, Event};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorSource {
    WaitEvent,
    Event,
    HandleEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorAction {
    Continue,
    Log,
}

fn classify_error(source: ErrorSource, err: &io::Error) -> ErrorAction {
    if err.kind() != io::ErrorKind::NotConnected {
        return ErrorAction::Log;
    }

    match source {
        ErrorSource::HandleEvent => ErrorAction::Continue,
        ErrorSource::WaitEvent | ErrorSource::Event => ErrorAction::Continue,
    }
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
            match classify_error(ErrorSource::WaitEvent, &err) {
                ErrorAction::Continue => continue,
                ErrorAction::Log => {
                    log::debug!("[{udc_name}] {label} wait_event error: {err}");
                    continue;
                }
            }
        }

        let event = match custom.event() {
            Ok(event) => event,
            Err(err) => {
                match classify_error(ErrorSource::Event, &err) {
                    ErrorAction::Continue => continue,
                    ErrorAction::Log => {
                        log::error!("[{udc_name}] {label} event error: {err}");
                        continue;
                    }
                }
            }
        };

        if let Err(err) = handler.handle_event(&udc_name, event).await {
            match classify_error(ErrorSource::HandleEvent, &err) {
                ErrorAction::Continue => {
                    log::info!("[{udc_name}] {label} handler saw closed transport: {err}");
                    continue;
                }
                ErrorAction::Log => {
                    log::error!("[{udc_name}] error handling {label} event: {err}");
                }
            }
        }
    }
}