//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! Shared system identity, serial, and runtime information.

pub mod runtime;
pub mod serial;

pub use runtime::{boot_info, inactive_side, system_info_json, BootRootfsInfo, SystemInfo};
pub use serial::{format_serial_mac, macs, resolve_serial, serial, MacAddresses};
