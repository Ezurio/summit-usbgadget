//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! USB gadget runtime/plugin crate for `summit-usbgadget`.
//!
//! This crate owns Linux UDC discovery and configfs/usb-gadget binding.

pub mod config;
pub mod registry;
#[doc(hidden)]
pub use registry::__inventory_submit;
#[doc(hidden)]
pub use registry::{parse_registered_function, FunctionRegistration};
pub mod gadget;
pub use gadget::run;
pub mod sysinfo;
pub mod udc;

// Register the USB gadget as a top-level startup service.
summit_usbgadget_config::declare_service!("usb" => run);
