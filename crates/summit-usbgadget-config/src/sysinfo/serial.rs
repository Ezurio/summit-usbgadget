//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! USB descriptor serial-source resolution.

use std::fs;
use std::process::Command;

use serde::Serialize;

const AM62X_MAC_CELLS: &[&str] = &[
    "/sys/bus/nvmem/devices/rv3028_eeprom0/cells/mac-address@0,0",
    "/sys/bus/nvmem/devices/rv3028_eeprom0/cells/mac-address@0,6",
];
const IMX95_MAC_CELLS: &[&str] = &[
    "/sys/bus/nvmem/devices/ELE-OCOTP0/cells/mac-address@514,0",
    "/sys/bus/nvmem/devices/ELE-OCOTP0/cells/mac-address@1514,0",
];
const IMX93_MAC_CELLS: &[&str] = &[
    "/sys/bus/nvmem/devices/ELE-OCOTP0/cells/mac-address@4ec,0",
    "/sys/bus/nvmem/devices/ELE-OCOTP0/cells/mac-address@4f2,0",
];
const IMX8MP_MAC_CELLS: &[&str] = &[
    "/sys/bus/nvmem/devices/imx-ocotp0/cells/mac-address@90,0",
    "/sys/bus/nvmem/devices/imx-ocotp0/cells/mac-address@96,0",
];
const IMX8MM_ETH0_MAC_CELL: &str = "/sys/bus/nvmem/devices/imx-ocotp0/cells/mac-address@90,0";
const SOM60_MAC_CELLS: &[&str] = &[
    "/sys/bus/nvmem/devices/0-00500/of_node/mac-address@2",
    "/sys/bus/nvmem/devices/0-00500/of_node/mac-address@8",
];

#[derive(Debug, Clone, Default, Serialize)]
pub struct MacAddresses {
    pub eth0: String,
    pub eth1: String,
}

pub fn macs() -> Option<MacAddresses> {
    let soc = soc_id()?;
    match soc.as_str() {
        "i.MX95" => read_cell_macs(IMX95_MAC_CELLS),
        "i.MX8MP" => read_cell_macs(IMX8MP_MAC_CELLS),
        "i.MX8MM" => read_single_cell_mac(IMX8MM_ETH0_MAC_CELL),
        "i.MX91" | "i.MX93" => read_cell_macs(IMX93_MAC_CELLS),
        "AM62X" | "J722S" | "AM62LX" => read_cell_macs(AM62X_MAC_CELLS),
        "sama5d36" => read_cell_macs(SOM60_MAC_CELLS),
        "sama5d31" => read_uboot_mac("ethaddr"),
        _ => return None,
    }
}

pub fn serial(macs: &MacAddresses) -> String {
    macs.eth0.clone()
}

pub fn soc_id() -> Option<String> {
    read_trimmed("/sys/devices/soc0/soc_id").or_else(|| read_trimmed("/sys/devices/soc0/family"))
}

fn read_cell_macs(cells: &[&str]) -> Option<MacAddresses> {
    Some(MacAddresses {
        eth0: read_mac_cell(cells[0]).unwrap_or_default(),
        eth1: read_mac_cell(cells[1]).unwrap_or_default(),
    })
}

fn read_single_cell_mac(path: &str) -> Option<MacAddresses> {
    Some(MacAddresses { eth0: read_mac_cell(path).unwrap_or_default(), eth1: String::new() })
}

fn read_uboot_mac(variable: &str) -> Option<MacAddresses> {
    let eth0 = read_uboot_value(variable)?;
    Some(MacAddresses { eth0, eth1: String::new() })
}

fn read_uboot_value(variable: &str) -> Option<String> {
    let output = Command::new("fw_printenv").args(["-n", variable]).output().ok()?;
    if !output.status.success() { return None; }
    let value = String::from_utf8_lossy(&output.stdout).trim().replace(':', "");
    (value.len() == 12 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(value.to_ascii_lowercase())
}

fn read_mac_cell(path: &str) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    format_serial_mac(&bytes)
}

fn read_trimmed(path: &str) -> Option<String> {
    let value = fs::read_to_string(path).ok()?;
    let value = value.trim();
    (!value.is_empty()).then_some(value.to_string())
}

pub fn format_serial_mac(bytes: &[u8]) -> Option<String> {
    (bytes.len() == 6).then(|| bytes.iter().rev().map(|byte| format!("{byte:02x}")).collect())
}

/// Resolves a USB descriptor serial from the configured source.
pub fn resolve_serial(serial_source: Option<&str>) -> Result<String, String> {
    let source = serial_source.unwrap_or("auto");
    match source {
        "wifi_mac" => match fs::read_to_string("/etc/wifi_mac") {
            Ok(value) if !value.trim().is_empty() => return Ok(normalize(value.trim())),
            _ => log::warn!("serial_source = \"wifi_mac\" is unavailable; falling back to auto"),
        },
        "auto" => {}
        other => log::warn!("invalid serial_source: {other}; falling back to auto"),
    }

    Ok(macs().and_then(|macs| (!macs.eth0.is_empty()).then_some(macs.eth0)).unwrap_or_else(|| {
        log::warn!("serial_source = \"auto\" but no supported fuse MAC is available; using random serial");
        random_serial()
    }))
}

fn random_serial() -> String {
    let mut bytes = [0u8; 6];
    let _: usize = rustix::rand::getrandom(&mut bytes, rustix::rand::GetRandomFlags::empty())
        .expect("getrandom(2) failed");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn normalize(value: &str) -> String {
    value.to_ascii_lowercase().chars().filter(|character| *character != ':').collect()
}