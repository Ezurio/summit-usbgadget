//
// SPDX-License-Identifier: MIT OR Apache-2.0
//

//! Runtime system information: boot context, hardware identity, and device
//! metadata used by DFU/FBK/SWUpdate paths.

use std::fmt::Write as _;
use std::fs;
use std::sync::OnceLock;

use tokio::process::Command;

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

/// Runs `boot-rootfs.sh` once and caches the result.
pub async fn init() {
    let info = run_boot_rootfs().await.unwrap_or_default();
    let _ = BOOT_INFO.set(info);
}

async fn run_boot_rootfs() -> std::io::Result<BootRootfsInfo> {
    let output = Command::new("/bin/sh")
        .args([
            "-c",
            ". boot-rootfs.sh && getSide >/dev/null && getBaseHwPartNumber >/dev/null && \
             printf 'rootDevType=%s\\ncurrentSide=%s\\nbaseHwPartNumber=%s\\n' \
             \"$rootDevType\" \"$bootside\" \"$baseHwPartNumber\"",
        ])
        .output()
        .await?;

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

/// Returns the cached boot rootfs context.
pub fn boot_info() -> &'static BootRootfsInfo {
    BOOT_INFO.get_or_init(BootRootfsInfo::default)
}

/// Returns the inactive boot side: the active side defaults to `"a"` when
/// unknown, and the opposite is returned.
pub fn inactive_side(current_side: Option<&str>) -> &'static str {
    let active = current_side.unwrap_or("a");
    if active == "a" { "b" } else { "a" }
}

/// Collected system information.
#[derive(Debug, Clone, Default)]
pub struct SystemInfo {
    /// Device-tree model string.
    pub model: Option<String>,
    /// Serial number (as advertised by the USB gadget).
    pub serial: Option<String>,
    /// SoC/CPU identifier.
    pub soc: Option<String>,
    /// Total system memory in mebibytes.
    pub memory_mb: Option<u64>,
    /// Base hardware part number.
    pub hw_part_number: Option<String>,
}

impl SystemInfo {
    /// Collects system information from the running system.
    pub fn collect(serial: Option<String>) -> Self {
        let base = base_info();
        Self {
            model: base.model.clone(),
            serial,
            soc: base.soc.clone(),
            memory_mb: base.memory_mb,
            hw_part_number: boot_info().hw_part_number.clone(),
        }
    }

    /// Serializes the information as newline-terminated `key=value` lines.
    pub fn to_keyvalue(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "model={}", opt(&self.model));
        let _ = writeln!(s, "serial={}", opt(&self.serial));
        let _ = writeln!(s, "soc={}", opt(&self.soc));
        let memory = self.memory_mb.map(|m| m.to_string());
        let _ = writeln!(s, "memory_mb={}", memory.as_deref().unwrap_or("unknown"));
        let _ = writeln!(s, "hw_part_number={}", opt(&self.hw_part_number));
        s
    }

    /// Serializes the information to its byte representation.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.to_keyvalue().into_bytes()
    }

    /// Serializes the information as a compact JSON object.
    pub fn to_json(&self) -> String {
        let mut s = String::from("{");
        push_json_field(&mut s, "model", self.model.as_deref().unwrap_or("unknown"));
        s.push(',');
        push_json_field(&mut s, "serial", self.serial.as_deref().unwrap_or("unknown"));
        s.push(',');
        push_json_field(&mut s, "soc", self.soc.as_deref().unwrap_or("unknown"));
        s.push(',');
        s.push_str("\"memory_mb\":");
        if let Some(memory_mb) = self.memory_mb {
            let _ = write!(s, "{memory_mb}");
        } else {
            s.push_str("null");
        }
        s.push(',');
        push_json_field(
            &mut s,
            "hw_part_number",
            self.hw_part_number.as_deref().unwrap_or("unknown"),
        );
        s.push('}');
        s
    }

    /// Serializes the information to UTF-8 JSON bytes.
    pub fn to_json_bytes(&self) -> Vec<u8> {
        self.to_json().into_bytes()
    }
}

fn opt(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("unknown")
}

fn push_json_field(out: &mut String, key: &str, value: &str) {
    out.push('"');
    out.push_str(key);
    out.push_str("\":\"");
    push_json_escaped(out, value);
    out.push('"');
}

fn push_json_escaped(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
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