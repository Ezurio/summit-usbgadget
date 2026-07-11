//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! FunctionFS event-loop re-exports for DFU.
//!
//! The endpoint-zero loop lives in the shared usb crate; DFU only supplies an
//! [`EventHandler`] and hands it to [`serve`].

pub(crate) use summit_usbgadget_usb::functionfs::{is_closed_transport_error, serve, EventHandler};