//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! Shared system identity, serial, and runtime information.

pub mod nvmem;
pub mod runtime;
pub mod serial;

pub use nvmem::{fuse_macs, FuseMacs};
pub use runtime::{
	boot_info, fuse_serial, inactive_side, system_info_json, BootRootfsInfo, SystemInfo,
};
pub use serial::resolve_serial;
