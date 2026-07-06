//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! Shared FunctionFS custom-function event loop helpers.

use std::io;

use usb_gadget::function::custom::Event;

pub(crate) fn is_closed_transport_error(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::NotConnected
        || err.raw_os_error() == Some(rustix::io::Errno::IDRM.raw_os_error())
        || err.raw_os_error() == Some(rustix::io::Errno::SHUTDOWN.raw_os_error())
}

/// Event handler for a FunctionFS custom function.
pub(crate) trait EventHandler {
    async fn handle_event(&mut self, udc_name: &str, event: Event<'_>) -> io::Result<()>;
}