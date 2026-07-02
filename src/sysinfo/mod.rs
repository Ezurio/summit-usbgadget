//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! Platform system information helpers.
//!
//! The always-available pieces here derive the USB product string and serial
//! number. Runtime boot/update metadata lives in [`runtime`] and is only built
//! when DFU or FBK support is enabled.

pub mod product;
pub mod serial;

#[cfg(any(feature = "dfu", feature = "fbk"))]
pub mod runtime;

#[cfg(any(feature = "dfu", feature = "fbk"))]
pub use runtime::{boot_info, inactive_side, init, BootRootfsInfo, SystemInfo};
