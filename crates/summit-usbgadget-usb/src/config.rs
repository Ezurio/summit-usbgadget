//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

//! USB gadget-specific configuration types for the summit-usbgadget app.

use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Deserializer};
use crate::registry::RegisteredFunctionConfig;
use summit_usbgadget_config::PluginConfig;
use toml::{Table, Value};

/// USB gadget configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct GadgetConfig {
    /// Gadget name in configfs (defaults to the file stem when omitted).
    pub name: Option<String>,
    /// USB device controller selection. Accepts a single controller name, a
    /// list of names, the special value `"all"`/`"*"` to bind every controller,
    /// or `"first"` to bind only the first one. When omitted, every available
    /// controller is bound (and controllers appearing later are bound too).
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
#[derive(Debug, Clone, Deserialize)]
pub struct OsDescriptorConfig {
    pub vendor_code: Option<u8>,
    pub qw_sign: Option<String>,
    pub config: Option<usize>,
}

/// Controller selection, accepting either a single name or a list of names.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum UdcSelector {
    One(String),
    Many(Vec<String>),
}

/// Device-level configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceConfig {
    pub vendor: u16,
    pub product: u16,
    pub class: Option<u8>,
    pub sub_class: Option<u8>,
    pub protocol: Option<u8>,
    pub manufacturer: Option<String>,
    pub product_name: Option<String>,
    pub product_name_source: Option<String>,
    pub serial: Option<String>,
    pub serial_source: Option<String>,
}

/// A USB configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct UsbConfigConfig {
    pub description: Option<String>,
    pub max_power: Option<u16>,
    pub self_powered: Option<bool>,
    pub remote_wakeup: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_function_configs")]
    pub function: Vec<FunctionConfig>,
}

/// A USB function specification, tagged by `type`.
#[derive(Debug)]
pub enum FunctionConfig {
    Serial(SerialConfig),
    Net(NetConfig),
    Msd(MsdConfig),
    Plugin(Box<dyn RegisteredFunctionConfig>),
}

impl Clone for FunctionConfig {
    fn clone(&self) -> Self {
        match self {
            Self::Serial(config) => Self::Serial(config.clone()),
            Self::Net(config) => Self::Net(config.clone()),
            Self::Msd(config) => Self::Msd(config.clone()),
            Self::Plugin(config) => Self::Plugin(config.clone()),
        }
    }
}

impl FunctionConfig {
    pub fn plugin<T>(config: T) -> Self
    where
        T: RegisteredFunctionConfig + 'static,
    {
        Self::Plugin(Box::new(config))
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Serial(_) => "serial",
            Self::Net(_) => "net",
            Self::Msd(_) => "msd",
            Self::Plugin(config) => config.kind(),
        }
    }

    pub fn downcast_ref<T>(&self) -> Option<&T>
    where
        T: 'static,
    {
        match self {
            Self::Plugin(config) => config.as_any().downcast_ref::<T>(),
            _ => None,
        }
    }
}

fn deserialize_function_configs<'de, D>(deserializer: D) -> Result<Vec<FunctionConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    let functions = Vec::<Table>::deserialize(deserializer)?;
    let mut parsed = Vec::new();

    for table in functions {
        if let Some(function) = parse_function_config(table).map_err(D::Error::custom)? {
            parsed.push(function);
        }
    }

    Ok(parsed)
}

fn parse_function_config(table: Table) -> Result<Option<FunctionConfig>, String> {
    let kind = table
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "USB function entry is missing string field `type`".to_string())?
        .to_string();

    match kind.as_str() {
        "serial" => parse_builtin_function::<SerialConfig, _>(table, FunctionConfig::Serial).map(Some),
        "net" => parse_builtin_function::<NetConfig, _>(table, FunctionConfig::Net).map(Some),
        "msd" => parse_builtin_function::<MsdConfig, _>(table, FunctionConfig::Msd).map(Some),
        other => match crate::registry::parse_function(other, table)? {
            Some(config) => Ok(Some(FunctionConfig::Plugin(config))),
            None => {
                log::error!("ignoring unsupported USB function section type {other:?}");
                Ok(None)
            }
        },
    }
}

fn parse_builtin_function<T, F>(table: Table, wrap: F) -> Result<FunctionConfig, String>
where
    T: DeserializeOwned,
    F: FnOnce(T) -> FunctionConfig,
{
    let config: T = Value::Table(table).try_into().map_err(|err: toml::de::Error| err.to_string())?;
    Ok(wrap(config))
}

#[derive(Debug, Clone, Deserialize)]
pub struct SerialConfig {
    pub class: String,
    pub console: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetConfig {
    pub class: String,
    pub dev_addr: Option<String>,
    pub host_addr: Option<String>,
    pub qmult: Option<u32>,
    pub os_compatible_id: Option<String>,
    pub os_sub_compatible_id: Option<String>,
    pub interface_class: Option<u8>,
    pub interface_sub_class: Option<u8>,
    pub interface_protocol: Option<u8>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LunConfig {
    pub file: Option<String>,
    pub read_only: Option<bool>,
    pub cdrom: Option<bool>,
    pub no_fua: Option<bool>,
    pub removable: Option<bool>,
    pub inquiry_string: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MsdConfig {
    pub stall: Option<bool>,
    #[serde(default)]
    pub lun: Vec<LunConfig>,
}

/// The USB gadget definition is the root of the shared configuration document,
/// so it is read with no section; other anchors/plugins retrieve their own
/// sections from the same file through the common configuration subsystem.
impl PluginConfig for GadgetConfig {}