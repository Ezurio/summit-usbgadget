//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! Serial-number derivation.
//!
//! Mirrors the serial-number rules of the summit-usbgadget `usb-gadget.sh`
//! script: a source selector chooses between an explicit value, a SoC-specific
//! fuse MAC, a U-Boot environment variable (read via `fw_printenv`, lowercased
//! with colons stripped), or an auto-detection chain.

use std::error::Error;
use crate::config::DeviceConfig;
use summit_usbgadget_config::sysinfo::resolve_serial;

/// Resolves the gadget serial number from the device configuration, following
/// the same source rules as `usb-gadget.sh`.
///
/// The source defaults to `custom` when `serial` is set and `auto` otherwise.
pub fn resolve(device: &DeviceConfig) -> Result<String, Box<dyn Error>> {
    resolve_serial(device.serial_source.as_deref())
        .map_err(Into::into)
}
