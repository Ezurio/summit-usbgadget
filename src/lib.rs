//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! Library crate for the `summit-usb-gadget` composite USB gadget.
//!
//! It is split into three modules:
//!
//! * [`config`] — parses the runtime gadget configuration file that selects
//!   which functions make up the composite gadget.
//! * [`dfu`] — when the `dfu` feature is enabled, the USB DFU 1.1 protocol
//!   state machine and the SWUpdate firmware sink.
//! * [`gadget`] — builds and binds the composite gadget described by a
//!   [`config::GadgetConfig`].
//!
//! The binary in `main.rs` wires these together: it loads the configuration,
//! builds the gadget, and runs the DFU endpoint-zero event loop.

pub mod config;
#[cfg(feature = "dfu")]
pub mod dfu;
#[cfg(feature = "fbk")]
pub mod fbk;
#[cfg(any(feature = "dfu", feature = "fbk"))]
mod functionfs;
#[cfg(any(feature = "dfu", feature = "fbk"))]
pub mod swupdate;
pub mod gadget;
pub mod sysinfo;
pub mod udc;
