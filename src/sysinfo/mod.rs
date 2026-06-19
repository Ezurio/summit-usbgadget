//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! Platform system information: boot context, hardware identity, and device
//! metadata.
//!
//! When the `dfu` feature is enabled, call [`init`] once at startup before any
//! swupdate sinks are started. All subsequent calls to [`boot_info`] return the
//! cached result without spawning a subprocess.

pub mod product;
pub mod serial;

#[cfg(feature = "dfu")]
use std::fmt::Write as _;
#[cfg(feature = "dfu")]
use std::fs;
#[cfg(feature = "dfu")]
use std::sync::OnceLock;

#[cfg(feature = "dfu")]
use tokio::process::Command;

// ---------------------------------------------------------------------------
// Boot rootfs context (from boot-rootfs.sh)
// ---------------------------------------------------------------------------

/// Boot rootfs context as reported by `boot-rootfs.sh`, plus hardware
/// information resolved in the same script invocation.
#[cfg(feature = "dfu")]
#[derive(Debug, Clone, Default)]
pub struct BootRootfsInfo {
    root_dev_type: String,
    current_side: String,
    /// Base hardware part number from `getBaseHwPartNumber`.
    pub hw_part_number: Option<String>,
}

#[cfg(feature = "dfu")]
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
    ///
    /// Both SD-card and initramfs boots require streaming through `fw_update`
    /// because SWUpdate IPC is not available in those environments.
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

#[cfg(feature = "dfu")]
static BOOT_INFO: OnceLock<BootRootfsInfo> = OnceLock::new();

/// Runs `boot-rootfs.sh` once and caches the result. Must be called at startup
/// before [`boot_info`] or any swupdate sink is started.
///
/// Errors are non-fatal: a default (all-empty) [`BootRootfsInfo`] is stored so
/// the rest of the system can still operate.
#[cfg(feature = "dfu")]
pub async fn init() {
    let info = run_boot_rootfs().await.unwrap_or_default();
    let _ = BOOT_INFO.set(info);
}

#[cfg(feature = "dfu")]
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

/// Returns the cached boot rootfs context. If [`init`] has not been called,
/// returns a default (all-empty) value.
#[cfg(feature = "dfu")]
pub fn boot_info() -> &'static BootRootfsInfo {
    BOOT_INFO.get_or_init(BootRootfsInfo::default)
}

/// Returns the inactive boot side: the active side defaults to `"a"` when
/// unknown, and the opposite is returned. Mirrors summit-rcm's `inactive_side`.
#[cfg(feature = "dfu")]
pub fn inactive_side(current_side: Option<&str>) -> &'static str {
    let active = current_side.unwrap_or("a");
    if active == "a" { "b" } else { "a" }
}

// ---------------------------------------------------------------------------
// System information (model, SoC, memory, serial)
// ---------------------------------------------------------------------------

/// Collected system information.
#[cfg(feature = "dfu")]
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

#[cfg(feature = "dfu")]
impl SystemInfo {
    /// Collects system information from the running system. The serial number,
    /// determined when the gadget is built, is passed in. Requires [`init`] to
    /// have been called first so that `hw_part_number` is available.
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
}

#[cfg(feature = "dfu")]
fn opt(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("unknown")
}

// ---------------------------------------------------------------------------
// Static hardware info (device-tree + /proc, cached on first read)
// ---------------------------------------------------------------------------

#[cfg(feature = "dfu")]
#[derive(Debug, Clone, Default)]
struct BaseSystemInfo {
    model: Option<String>,
    soc: Option<String>,
    memory_mb: Option<u64>,
}

#[cfg(feature = "dfu")]
fn base_info() -> &'static BaseSystemInfo {
    static BASE_INFO: OnceLock<BaseSystemInfo> = OnceLock::new();
    BASE_INFO.get_or_init(|| BaseSystemInfo {
        model: read_dt_string("/sys/firmware/devicetree/base/model"),
        soc: soc_id(),
        memory_mb: mem_total_mb(),
    })
}

/// Reads a NUL-terminated device-tree string property.
#[cfg(feature = "dfu")]
fn read_dt_string(path: &str) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let s = String::from_utf8_lossy(&bytes[..end]).trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(feature = "dfu")]
fn read_trimmed(path: &str) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// SoC identifier, preferring `soc_id` and falling back to `family`.
#[cfg(feature = "dfu")]
fn soc_id() -> Option<String> {
    read_trimmed("/sys/devices/soc0/soc_id")
        .or_else(|| read_trimmed("/sys/devices/soc0/family"))
}

/// Total system memory in MiB, parsed from `/proc/meminfo`.
/// New kernels report in kB (`"16384000 kB"`); old kernels report raw bytes.
#[cfg(feature = "dfu")]
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
