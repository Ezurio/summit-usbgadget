//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! TOML configuration for the composite USB gadget.
//!
//! The schema mirrors the `usb-gadget` CLI tool's TOML format (a `[device]`
//! table, one or more `[[config]]` tables, each with `[[config.function]]`
//! entries tagged by `type`), so configurations are interchangeable for the
//! function types this service supports. In addition to the kernel-backed
//! functions (`serial`, `net`, `msd`), a `dfu` function type is provided that
//! is implemented in user space and serviced by this daemon.
//!
//! Example:
//!
//! ```toml
//! name = "composite"
//! # udc = "11401000.usb"
//!
//! [device]
//! vendor = 0x1d50
//! product = 0x6089
//! manufacturer = "Ezurio"
//! product_name = "Composite DFU gadget"
//! serial = "0001"
//! class = 0xef        # miscellaneous (IAD)
//! sub_class = 2
//! protocol = 1
//!
//! [[config]]
//! description = "composite"
//!
//! [[config.function]]
//! type = "serial"
//! class = "acm"
//!
//! [[config.function]]
//! type = "dfu"
//! download = "swupdate"
//! ```

use std::error::Error;
use std::fmt;
use std::path::Path;

#[cfg(any(feature = "dfu", feature = "fbk"))]
use std::path::PathBuf;
#[cfg(any(feature = "dfu", feature = "fbk"))]
use std::time::Duration;

use serde::Deserialize;

#[cfg(feature = "dfu")]
use bytes::Bytes;
#[cfg(feature = "dfu")]
use crate::dfu::{DfuConfig, DownloadTarget, UploadSource};
#[cfg(all(not(feature = "dfu"), feature = "fbk"))]
use crate::stream_download::DownloadTarget;
#[cfg(any(feature = "dfu", feature = "fbk"))]
use crate::swupdate::SwupdateParams;

/// Top-level gadget configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct GadgetConfig {
    /// Gadget name in configfs (defaults to the file stem when omitted).
    pub name: Option<String>,
    /// USB device controller selection. Accepts a single controller name, a
    /// list of names, or the special value `"all"`/`"*"` to bind every
    /// controller. When omitted, the first available controller is used.
    pub udc: Option<UdcSelector>,
    /// Optional Microsoft OS descriptor configuration.
    pub os_descriptor: Option<OsDescriptorConfig>,
    /// Device descriptor fields.
    pub device: DeviceConfig,
    /// USB configurations (at least one required).
    #[serde(default)]
    pub config: Vec<UsbConfigConfig>,
}

/// Top-level OS descriptor configuration.
///
/// When present, the descriptor is built from the `usb-gadget` crate's
/// [`OsDescriptor::microsoft`](usb_gadget::OsDescriptor::microsoft) defaults;
/// the fields below override those defaults when set.
#[derive(Debug, Clone, Deserialize)]
pub struct OsDescriptorConfig {
    /// Vendor-specific request code used to fetch the OS descriptor. Defaults
    /// to the crate's Microsoft value when omitted.
    pub vendor_code: Option<u8>,
    /// Signature string. Defaults to the crate's Microsoft `MSFT100` value when
    /// omitted.
    pub qw_sign: Option<String>,
    /// Index of the USB configuration linked from `os_desc` (default `0`).
    pub config: Option<usize>,
}

/// Controller selection, accepting either a single name or a list of names.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum UdcSelector {
    /// A single controller name, or `"all"`/`"*"`.
    One(String),
    /// An explicit list of controller names.
    Many(Vec<String>),
}

/// Device-level configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceConfig {
    /// Vendor ID.
    pub vendor: u16,
    /// Product ID.
    pub product: u16,
    /// Device class code (default: 0, interface-specific).
    pub class: Option<u8>,
    /// Device sub-class code.
    pub sub_class: Option<u8>,
    /// Device protocol code.
    pub protocol: Option<u8>,
    /// Manufacturer name.
    pub manufacturer: Option<String>,
    /// Product name.
    pub product_name: Option<String>,
    /// How the product name is derived: `custom` (use `product_name`) or
    /// `model` (default when `product_name` is unset, read the device-tree
    /// model string), matching the legacy summit-usbgadget script.
    pub product_name_source: Option<String>,
    /// Serial number. Used directly when `serial_source` is unset or `custom`.
    pub serial: Option<String>,
    /// How the serial number is derived: `auto` (default when `serial` is
    /// unset), `custom` (use `serial` verbatim), `uboot_ethaddr`, or
    /// `uboot_eth1addr`. Mirrors the `USB_GADGET_SERIAL_SOURCE` rules of the
    /// summit-usbgadget script.
    pub serial_source: Option<String>,
}

/// A USB configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct UsbConfigConfig {
    /// Configuration description.
    pub description: Option<String>,
    /// Maximum power in mA.
    pub max_power: Option<u16>,
    /// Self-powered flag.
    pub self_powered: Option<bool>,
    /// Remote wakeup flag.
    pub remote_wakeup: Option<bool>,
    /// Functions in this configuration, in order.
    #[serde(default)]
    pub function: Vec<FunctionConfig>,
}

/// A USB function specification, tagged by `type`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FunctionConfig {
    /// CDC ACM or generic serial port.
    Serial(SerialConfig),
    /// CDC network interface.
    Net(NetConfig),
    /// Mass-storage device.
    Msd(MsdConfig),
    /// USB DFU interface implemented by this service.
    Dfu(DfuFnConfig),
    /// Minimal FBK-like upload interface implemented by this service.
    Fbk(DfuFnConfig),
}

/// Serial function configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct SerialConfig {
    /// `"acm"` or `"generic"`.
    pub class: String,
    /// Expose the port as a kernel console.
    pub console: Option<bool>,
}

/// Network function configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct NetConfig {
    /// `"ecm"`, `"ecm_subset"`, `"eem"`, `"ncm"`, or `"rndis"`.
    pub class: String,
    /// Device-side MAC address.
    pub dev_addr: Option<String>,
    /// Host-side MAC address.
    pub host_addr: Option<String>,
    /// Queue length multiplier.
    pub qmult: Option<u32>,
    /// Optional Microsoft OS compatible ID for this interface. Supported for
    /// `ncm` (typically `WINNCM`) and `rndis` (typically `RNDIS`).
    pub os_compatible_id: Option<String>,
    /// Optional Microsoft OS sub-compatible ID (e.g. `5162001` for RNDIS).
    /// Requires `os_compatible_id` to be set.
    pub os_sub_compatible_id: Option<String>,
    /// RNDIS interface class override.
    pub interface_class: Option<u8>,
    /// RNDIS interface sub-class override.
    pub interface_sub_class: Option<u8>,
    /// RNDIS interface protocol override.
    pub interface_protocol: Option<u8>,
}

/// A mass-storage logical unit.
#[derive(Debug, Clone, Deserialize)]
pub struct LunConfig {
    /// Backing file or block device.
    pub file: Option<String>,
    /// Read-only access.
    pub read_only: Option<bool>,
    /// Report the LUN as a CD-ROM.
    pub cdrom: Option<bool>,
    /// Disable FUA in SCSI WRITE.
    pub no_fua: Option<bool>,
    /// Report the LUN as removable.
    pub removable: Option<bool>,
    /// SCSI inquiry string.
    pub inquiry_string: Option<String>,
}

/// Mass-storage function configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct MsdConfig {
    /// Stall on errors.
    pub stall: Option<bool>,
    /// Logical units.
    #[serde(default)]
    pub lun: Vec<LunConfig>,
}

/// DFU function configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct DfuFnConfig {
    /// `"swupdate"` (default) or `"file:/path"`.
    pub download: Option<String>,
    /// What `DFU_UPLOAD` serves: a file path, or `"sysinfo"` to report the
    /// device's system information (model, serial, SoC, memory, HW part number).
    pub upload: Option<String>,
    /// Maximum bytes per DFU control-write transaction.
    pub transfer_size: Option<u16>,
    /// `bwPollTimeout` reported in `DFU_GETSTATUS`, in milliseconds.
    pub poll_timeout_ms: Option<u32>,
    /// Optional SWUpdate `software_set` selection (defaults to `"stable"`).
    pub software_set: Option<String>,
    /// Image-mode label used in the running mode (defaults to `"full"`).
    pub image_mode: Option<String>,
    /// Validate the update without writing it.
    pub dry_run: Option<bool>,
    /// Forbid SWUpdate from persisting the streamed SWU (default `true`).
    pub disable_store_swu: Option<bool>,
    /// Maximum seconds to wait for the SWUpdate result.
    pub timeout_secs: Option<u64>,
}

impl DfuFnConfig {
    /// Converts the configured download target into the shared streamed-install
    /// destination used by DFU and FBK.
    #[cfg(any(feature = "dfu", feature = "fbk"))]
    pub(crate) fn to_download_target(&self) -> DownloadTarget {
        match self.download.as_deref() {
            None | Some("swupdate") => DownloadTarget::Swupdate(self.to_swupdate_params()),
            Some(other) => {
                let path = other.strip_prefix("file:").unwrap_or(other);
                DownloadTarget::File(PathBuf::from(path))
            }
        }
    }

    /// Converts SWUpdate tuning into sink parameters.
    #[cfg(any(feature = "dfu", feature = "fbk"))]
    pub(crate) fn to_swupdate_params(&self) -> SwupdateParams {
        SwupdateParams {
            software_set: self.software_set.clone(),
            image_mode: self.image_mode.clone(),
            dry_run: self.dry_run.unwrap_or(false),
            disable_store_swu: self.disable_store_swu.unwrap_or(true),
            timeout: Duration::from_secs(self.timeout_secs.unwrap_or(120)),
        }
    }
}

#[cfg(feature = "dfu")]
impl DfuFnConfig {
    /// Converts the parsed configuration into the runtime [`DfuConfig`].
    ///
    /// `serial` is the resolved gadget serial number, embedded in the system
    /// information report when `upload = "sysinfo"`.
    pub fn to_config(&self, serial: &str) -> DfuConfig {
        let transfer_size = self.transfer_size.unwrap_or(4096);
        let poll_timeout_ms = self.poll_timeout_ms.unwrap_or(10);

        let upload = match self.upload.as_deref() {
            None => None,
            Some("sysinfo") => {
                let info = crate::sysinfo::SystemInfo::collect(Some(serial.to_string()));
                Some(UploadSource::Data(Bytes::from(info.to_bytes())))
            }
            Some(other) => {
                let path = other.strip_prefix("file:").unwrap_or(other);
                Some(UploadSource::File(PathBuf::from(path)))
            }
        };

        let download = self.to_download_target();

        DfuConfig { download, upload, transfer_size, poll_timeout_ms }
    }
}

/// An error produced while loading or parsing the configuration.
#[derive(Debug)]
pub enum ConfigError {
    /// The configuration file could not be read.
    Io(std::io::Error),
    /// The TOML could not be parsed.
    Toml(toml::de::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "cannot read configuration: {e}"),
            ConfigError::Toml(e) => write!(f, "invalid configuration: {e}"),
        }
    }
}

impl Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(e: toml::de::Error) -> Self {
        ConfigError::Toml(e)
    }
}

impl GadgetConfig {
    /// Loads and parses a TOML configuration file from `path`.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }
}
