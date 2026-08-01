//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! Product-name derivation.
//!
//! Mirrors the product-string behavior of the legacy summit-usbgadget
//! `usb-gadget.sh` script: use an explicit value when configured, or derive the
//! USB product string from the device-tree model.

use std::error::Error;
use std::fs;

use crate::config::DeviceConfig;

/// Resolves the gadget product string from the device configuration.
///
/// The source defaults to `custom` when `product_name` is set and `model`
/// otherwise, matching the legacy shell implementation.
pub fn resolve(device: &DeviceConfig) -> Result<String, Box<dyn Error>> {
    let default_source = if device.product_name.is_some() {
        "custom"
    } else {
        "model"
    };
    let source = device
        .product_name_source
        .as_deref()
        .unwrap_or(default_source);

    match source {
        "custom" => device
            .product_name
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "product_name_source = \"custom\" requires a non-empty `product_name`".into()
            }),
        "model" => Ok(read_model().unwrap_or_default()),
        other => Err(format!("invalid product_name_source: {other}").into()),
    }
}

fn read_model() -> Option<String> {
    let bytes = fs::read("/sys/firmware/devicetree/base/model").ok()?;
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let value = String::from_utf8_lossy(&bytes[..end]).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}
