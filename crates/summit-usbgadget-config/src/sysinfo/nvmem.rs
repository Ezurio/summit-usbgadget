//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! SoC-specific NVMEM and U-Boot Ethernet identity helpers.

use std::fs;
use std::process::Command;

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FuseMacs {
    pub eth0: Option<String>,
    pub eth1: Option<String>,
}

pub fn fuse_macs() -> Option<FuseMacs> {
    let soc = soc_id()?;
    let macs = match soc.as_str() {
        "i.MX95" => read_cell_macs(IMX95_MAC_CELLS),
        "i.MX8MP" => read_cell_macs(IMX8MP_MAC_CELLS),
        "i.MX8MM" => read_single_cell_mac(IMX8MM_ETH0_MAC_CELL),
        "i.MX91" | "i.MX93" => read_cell_macs(IMX93_MAC_CELLS),
        "AM62X" | "J722S" => read_cell_macs(AM62X_MAC_CELLS),
        "sama5d36" => read_cell_macs(SOM60_MAC_CELLS),
        "sama5d31" => read_uboot_macs(),
        _ => return None,
    }?;
    (macs.eth0.is_some() || macs.eth1.is_some()).then_some(macs)
}

pub fn soc_id() -> Option<String> {
    read_trimmed("/sys/devices/soc0/soc_id").or_else(|| read_trimmed("/sys/devices/soc0/family"))
}

fn read_cell_macs(cells: &[&str]) -> Option<FuseMacs> {
    Some(FuseMacs {
        eth0: read_mac_cell(cells[0]),
        eth1: read_mac_cell(cells[1]),
    })
}

fn read_single_cell_mac(path: &str) -> Option<FuseMacs> {
    Some(FuseMacs { eth0: read_mac_cell(path), eth1: None })
}

fn read_uboot_macs() -> Option<FuseMacs> {
    Some(FuseMacs { eth0: read_uboot_mac("ethaddr"), eth1: None })
}

fn read_uboot_mac(variable: &str) -> Option<String> {
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

/// Formats a six-byte NVMEM MAC payload as a reversed lowercase hex serial.
pub fn format_serial_mac(bytes: &[u8]) -> Option<String> {
    (bytes.len() == 6).then(|| bytes.iter().rev().map(|byte| format!("{byte:02x}")).collect())
}
