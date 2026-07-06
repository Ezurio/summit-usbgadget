//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! Shared FunctionFS custom-function event loop helpers.

use std::io;

use bytes::Bytes;
use usb_gadget::function::custom::{EndpointSender, Event};

pub(crate) fn is_closed_transport_error(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::NotConnected
        || err.raw_os_error() == Some(rustix::io::Errno::IDRM.raw_os_error())
        || err.raw_os_error() == Some(rustix::io::Errno::SHUTDOWN.raw_os_error())
}

/// Writes a fixed reply token onto the fastboot-usb bulk IN endpoint.
pub(crate) async fn send_static(tx: &mut EndpointSender, data: &'static [u8]) -> io::Result<()> {
    tx.send_async(Bytes::from_static(data)).await
}

/// Writes the `DATA%08X` data-phase reply header onto the fastboot-usb bulk IN endpoint.
pub(crate) async fn send_data_header(tx: &mut EndpointSender, len: usize) -> io::Result<()> {
    tx.send_async(Bytes::from(summit_usbgadget_fastboot_proto::data_header(len))).await
}

/// Event handler for a FunctionFS custom function.
pub(crate) trait EventHandler {
    async fn handle_event(&mut self, udc_name: &str, event: Event<'_>) -> io::Result<()>;
}