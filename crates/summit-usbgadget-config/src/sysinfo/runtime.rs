//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! Shared runtime system information.

use std::fs;
use std::process::Command;
use std::sync::OnceLock;

use serde::Serialize;

#[derive(Debug, Clone, Default)]
pub struct BootRootfsInfo {
    root_dev_type: String,
    current_side: String,
    pub hw_part_number: Option<String>,
}

impl BootRootfsInfo {
    pub fn is_running_on_sd(&self) -> bool { self.root_dev_type == "SD" }
    pub fn is_running_on_initramfs(&self) -> bool { self.root_dev_type == "initramfs" }
    pub fn is_single_slot(&self) -> bool { self.is_running_on_sd() || self.is_running_on_initramfs() }
    pub fn use_pipe_mode(&self) -> bool { self.is_single_slot() }
    pub fn current_side_option(&self) -> Option<&str> {
        match self.current_side.as_str() { "a" | "b" => Some(&self.current_side), _ => None }
    }
}

static BOOT_INFO: OnceLock<BootRootfsInfo> = OnceLock::new();

fn run_boot_rootfs() -> std::io::Result<BootRootfsInfo> {
    let output = Command::new("/bin/sh").args(["-c", ". boot-rootfs.sh && getSide >/dev/null && getBaseHwPartNumber >/dev/null && printf 'rootDevType=%s\\ncurrentSide=%s\\nbaseHwPartNumber=%s\\n' \"$rootDevType\" \"$bootside\" \"$baseHwPartNumber\""]).output()?;
    let mut info = BootRootfsInfo::default();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim().to_string();
            match key {
                "rootDevType" => info.root_dev_type = value,
                "currentSide" => info.current_side = value,
                "baseHwPartNumber" if !value.is_empty() => info.hw_part_number = Some(value),
                _ => {}
            }
        }
    }
    Ok(info)
}

pub fn boot_info() -> &'static BootRootfsInfo { BOOT_INFO.get_or_init(|| run_boot_rootfs().unwrap_or_default()) }
pub fn inactive_side(current_side: Option<&str>) -> &'static str { if current_side.unwrap_or("a") == "a" { "b" } else { "a" } }
pub fn fuse_serial() -> String { super::nvmem::fuse_macs().and_then(|macs| macs.eth0).unwrap_or_default() }

pub fn system_info_json() -> Vec<u8> {
    let mut json = Vec::new();
    SystemInfo::collect(Some(fuse_serial())).write_json(&mut json);
    json
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SystemInfo {
    pub model: String,
    pub serial: String,
    pub soc: String,
    pub memory_mb: u64,
    pub hw_part_number: String,
}

impl SystemInfo {
    pub fn collect(serial: Option<String>) -> Self {
        let base = base_info();
        Self { model: base.model.clone().unwrap_or_default(), serial: serial.unwrap_or_default(), soc: base.soc.clone().unwrap_or_default(), memory_mb: base.memory_mb.unwrap_or_default(), hw_part_number: boot_info().hw_part_number.clone().unwrap_or_default() }
    }
    pub fn write_json(&self, out: &mut Vec<u8>) { let _ = serde_json::to_writer(out, self); }
}

#[derive(Default)]
struct BaseSystemInfo { model: Option<String>, soc: Option<String>, memory_mb: Option<u64> }
fn base_info() -> &'static BaseSystemInfo {
    static BASE_INFO: OnceLock<BaseSystemInfo> = OnceLock::new();
    BASE_INFO.get_or_init(|| BaseSystemInfo { model: read_dt_string("/sys/firmware/devicetree/base/model"), soc: super::nvmem::soc_id(), memory_mb: mem_total_mb() })
}
fn read_dt_string(path: &str) -> Option<String> { let bytes = fs::read(path).ok()?; let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len()); let value = String::from_utf8_lossy(&bytes[..end]).trim().to_string(); (!value.is_empty()).then_some(value) }
fn mem_total_mb() -> Option<u64> { let text = fs::read_to_string("/proc/meminfo").ok()?; let line = text.lines().find(|line| line.starts_with("MemTotal:"))?; let kb = line.strip_prefix("MemTotal:")?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()?; Some(kb / 1024) }
