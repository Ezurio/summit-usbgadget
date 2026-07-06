//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! Serial-number derivation.
//!
//! Mirrors the serial-number rules of the summit-usbgadget `usb-gadget.sh`
//! script: a source selector chooses between an explicit value, a U-Boot
//! environment variable (read via `fw_printenv`, lowercased with colons
//! stripped), or an auto-detection chain.

use std::error::Error;
use std::fs;
use std::process::Command;

use crate::config::DeviceConfig;

/// Resolves the gadget serial number from the device configuration, following
/// the same source rules as `usb-gadget.sh`.
///
/// The source defaults to `custom` when `serial` is set and `auto` otherwise.
pub fn resolve(device: &DeviceConfig) -> Result<String, Box<dyn Error>> {
    let default_source = if device.serial.is_some() { "custom" } else { "auto" };
    let source = device.serial_source.as_deref().unwrap_or(default_source);

    match source {
        "custom" => device
            .serial
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "serial_source = \"custom\" requires a non-empty `serial`".into()),
        "uboot_ethaddr" => {
            from_env("ethaddr").ok_or_else(|| "serial_source = \"uboot_ethaddr\" but ethaddr is not set".into())
        }
        "uboot_eth1addr" => from_env("eth1addr")
            .ok_or_else(|| "serial_source = \"uboot_eth1addr\" but eth1addr is not set".into()),
        "auto" => Ok(auto()),
        other => Err(format!("invalid serial_source: {other}").into()),
    }
}

/// Auto-detection chain (first available wins), matching the shell script.
fn auto() -> String {
    if let Some(s) = read_trimmed("/etc/wifi_mac") {
        return s;
    }
    if let Some(s) = read_trimmed("/sys/devices/soc0/soc_uid") {
        return s;
    }
    if let Some(s) = read_trimmed("/sys/class/net/eth1/address") {
        return strip_colons(&s);
    }
    if let Some(s) = read_trimmed("/sys/class/net/eth0/address") {
        return strip_colons(&s);
    }
    "deadbeefdeadbeef".to_string()
}

/// Reads a U-Boot environment variable via `fw_printenv` and normalizes it.
fn from_env(var: &str) -> Option<String> {
    let output = Command::new("fw_printenv").arg("-n").arg(var).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        return None;
    }
    Some(normalize(&value))
}

/// Lowercases and strips colons, matching `normalize_serial_number`.
fn normalize(s: &str) -> String {
    strip_colons(&s.to_ascii_lowercase())
}

fn strip_colons(s: &str) -> String {
    s.chars().filter(|&c| c != ':').collect()
}

fn read_trimmed(path: &str) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}
