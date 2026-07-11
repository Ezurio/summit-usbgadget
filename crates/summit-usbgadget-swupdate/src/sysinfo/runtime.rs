//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

//! Runtime system information: boot context, hardware identity, and device
//! metadata used by DFU/FBK/SWUpdate paths.

use std::fs;
use std::process::Command;
use std::sync::OnceLock;

use serde::Serialize;

/// Boot rootfs context as reported by `boot-rootfs.sh`, plus hardware
/// information resolved in the same script invocation.
#[derive(Debug, Clone, Default)]
pub struct BootRootfsInfo {
    root_dev_type: String,
    current_side: String,
    /// Base hardware part number from `getBaseHwPartNumber`.
    pub hw_part_number: Option<String>,
}

impl BootRootfsInfo {
    /// Whether the system booted from an SD card.
    pub fn is_running_on_sd(&self) -> bool {
        self.root_dev_type == "SD"
    }

    /// Whether the system booted from an initramfs.
    pub fn is_running_on_initramfs(&self) -> bool {
        self.root_dev_type == "initramfs"
    }

    /// Whether `fw_update` pipe mode should be used for firmware installation.
    pub fn use_pipe_mode(&self) -> bool {
        self.is_running_on_sd() || self.is_running_on_initramfs()
    }

    /// The current boot side (`"a"` or `"b"`), if valid.
    pub fn current_side_option(&self) -> Option<&str> {
        match self.current_side.as_str() {
            "a" | "b" => Some(self.current_side.as_str()),
            _ => None,
        }
    }
}

static BOOT_INFO: OnceLock<BootRootfsInfo> = OnceLock::new();

fn run_boot_rootfs() -> std::io::Result<BootRootfsInfo> {
    let output = Command::new("/bin/sh")
        .args([
            "-c",
            ". boot-rootfs.sh && getSide >/dev/null && getBaseHwPartNumber >/dev/null && \
             printf 'rootDevType=%s\\ncurrentSide=%s\\nbaseHwPartNumber=%s\\n' \
             \"$rootDevType\" \"$bootside\" \"$baseHwPartNumber\"",
        ])
        .output()?;

    let text = String::from_utf8_lossy(&output.stdout);
    let mut info = BootRootfsInfo::default();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim().to_string();
            match key {
                "rootDevType" => info.root_dev_type = value,
                "currentSide" => info.current_side = value,
                "baseHwPartNumber" if !value.is_empty() => {
                    info.hw_part_number = Some(value);
                }
                _ => {}
            }
        }
    }
    Ok(info)
}

/// Returns the cached boot rootfs context, running `boot-rootfs.sh` once on
/// first access to gather the boot side and hardware part number.
pub fn boot_info() -> &'static BootRootfsInfo {
    BOOT_INFO.get_or_init(|| run_boot_rootfs().unwrap_or_default())
}

/// Returns the inactive boot side: the active side defaults to `"a"` when
/// unknown, and the opposite is returned.
pub fn inactive_side(current_side: Option<&str>) -> &'static str {
    let active = current_side.unwrap_or("a");
    if active == "a" { "b" } else { "a" }
}

/// Collected system information.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SystemInfo {
    /// Device-tree model string.
    pub model: String,
    /// Serial number (as advertised by the USB gadget).
    pub serial: String,
    /// SoC/CPU identifier.
    pub soc: String,
    /// Total system memory in mebibytes.
    pub memory_mb: u64,
    /// Base hardware part number.
    pub hw_part_number: String,
}

impl SystemInfo {
    /// Collects system information from the running system.
    pub fn collect(serial: Option<String>) -> Self {
        let base = base_info();
        Self {
            model: base.model.clone().unwrap_or_default(),
            serial: serial.unwrap_or_default(),
            soc: base.soc.clone().unwrap_or_default(),
            memory_mb: base.memory_mb.unwrap_or_default(),
            hw_part_number: boot_info().hw_part_number.clone().unwrap_or_default(),
        }
    }

    /// Serializes the compact JSON object representation into `out`.
    pub fn write_json(&self, out: &mut Vec<u8>) {
        // `Vec<u8>` is an infallible `io::Write` sink, so serialization cannot fail.
        let _ = serde_json::to_writer(out, self);
    }
}

#[derive(Debug, Clone, Default)]
struct BaseSystemInfo {
    model: Option<String>,
    soc: Option<String>,
    memory_mb: Option<u64>,
}

fn base_info() -> &'static BaseSystemInfo {
    static BASE_INFO: OnceLock<BaseSystemInfo> = OnceLock::new();
    BASE_INFO.get_or_init(|| BaseSystemInfo {
        model: read_dt_string("/sys/firmware/devicetree/base/model"),
        soc: soc_id(),
        memory_mb: mem_total_mb(),
    })
}

fn read_dt_string(path: &str) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let s = String::from_utf8_lossy(&bytes[..end]).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn read_trimmed(path: &str) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn soc_id() -> Option<String> {
    read_trimmed("/sys/devices/soc0/soc_id")
        .or_else(|| read_trimmed("/sys/devices/soc0/family"))
}

fn mem_total_mb() -> Option<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let rest = rest.trim();
            if let Some(num) = rest.strip_suffix("kB") {
                let kb: u64 = num.trim().parse().ok()?;
                return Some(kb / 1024);
            } else {
                let bytes: u64 = rest.split_whitespace().next()?.parse().ok()?;
                return Some(bytes / (1024 * 1024));
            }
        }
    }
    None
}