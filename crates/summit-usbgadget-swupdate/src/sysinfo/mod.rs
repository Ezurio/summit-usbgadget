//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

//! Runtime boot and system information shared by DFU, FBK, and SWUpdate.

mod runtime;

pub use runtime::{boot_info, inactive_side, BootRootfsInfo, SystemInfo};
pub use summit_usbgadget_config::sysinfo::{macs, system_info_json};
