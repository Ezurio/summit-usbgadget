//
// SPDX-License-Identifier: MIT OR Apache-2.0
//

//! Runtime boot and system information shared by DFU, FBK, and SWUpdate.

mod runtime;

pub use runtime::{boot_info, inactive_side, BootRootfsInfo, SystemInfo};