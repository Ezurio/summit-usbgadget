//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! USB Device Firmware Upgrade (DFU 1.1) protocol implementation.
//!
//! Services the DFU class-specific control requests that the kernel delivers on
//! endpoint zero of the FunctionFS-backed custom function. Received firmware is
//! streamed directly into the SWUpdate path as each block arrives — no local or
//! temporary file is created. Firmware uploads are served from a file.
//!
//! The module is split into:
//!
//! * [`protocol`] — request codes and the DFU state/status enumerations.
//! * [`config`] — the runtime DFU configuration and SWUpdate parameters.
//! * [`gadget`] — gadget-side integration: builds the DFU custom function and
//!   runs its endpoint-zero event loop.
//! * `sink` — firmware sinks (file or SWUpdate IPC), used internally.
//! * `handler` — the [`Dfu`] state machine driven from endpoint zero.
//!
//! Reference: USB Device Firmware Upgrade specification, revision 1.1.

mod handler;

pub mod config;
pub mod gadget;
pub mod protocol;

pub use config::{DfuConfig, UploadSource};
pub use gadget::{serve, DfuRuntime};
pub use handler::{is_dfu_request, Dfu};
pub use protocol::{request, GetStatus, State, Status};
