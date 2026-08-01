//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! USB descriptor serial-source resolution.

use std::fs;
use super::nvmem::fuse_macs;

/// Resolves a USB descriptor serial from the configured source.
pub fn resolve_serial(serial_source: Option<&str>) -> Result<String, String> {
    match serial_source.unwrap_or("fuse_mac") {
        "fuse_mac" => fuse_macs()
            .and_then(|macs| macs.eth0)
            .ok_or_else(|| "serial_source = \"fuse_mac\" but no supported fuse MAC is available".to_string()),
        "wifi_mac" => read_trimmed("/etc/wifi_mac")
            .map(|value| normalize(&value))
            .ok_or_else(|| "serial_source = \"wifi_mac\" but /etc/wifi_mac is unavailable".to_string()),
        other => Err(format!("invalid serial_source: {other}")),
    }
}

fn normalize(value: &str) -> String { strip_colons(&value.to_ascii_lowercase()) }
fn strip_colons(value: &str) -> String { value.chars().filter(|character| *character != ':').collect() }

fn read_trimmed(path: &str) -> Option<String> {
    let value = fs::read_to_string(path).ok()?;
    let value = value.trim();
    (!value.is_empty()).then_some(value.to_string())
}
