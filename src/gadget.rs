//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! Builds and binds the composite USB gadget described by a
//! [`crate::config::GadgetConfig`].
//!
//! Functions are constructed using the same public `usb-gadget` builders as the
//! `usb-gadget` CLI tool, so a configuration written for that tool produces an
//! equivalent gadget here. Kernel-backed functions are registered and their
//! objects discarded (the registration is owned by the gadget). The `dfu`
//! function is a user-space FunctionFS custom interface built by
//! [`crate::dfu::gadget`]; its runtime is captured in [`RunningGadget`] so the
//! caller can run the DFU endpoint-zero event loop.

use std::collections::HashSet;
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::PathBuf;

use futures_util::StreamExt;
use tokio_udev::EventType;
use usb_gadget::function::{self, Handle};
use usb_gadget::{remove_all, Class, Config, Gadget, Id, OsDescriptor, RegGadget, Strings, Udc};

use crate::config::{DfuFnConfig, FunctionConfig, GadgetConfig, MsdConfig, NetConfig, SerialConfig};
#[cfg(feature = "dfu")]
use crate::dfu::gadget::DfuRuntime;
#[cfg(feature = "fbk")]
use crate::fbk::FbkRuntime;
use crate::sysinfo::product;
use crate::sysinfo::serial;
use crate::udc::{self, Selection};

/// A pending Microsoft OS extended-compatibility descriptor for a network
/// function, written to configfs after the gadget is registered.
#[derive(Debug)]
struct NetOsDescriptor {
    function: function::net::Net,
    /// Interface directory name under `os_desc/` (e.g. `ncm`, `rndis`).
    interface: &'static str,
    compatible_id: String,
    sub_compatible_id: Option<String>,
}

/// Records the Microsoft OS compatible IDs for a network function, if any.
///
/// Only NCM and RNDIS expose a per-interface `os_desc` directory in configfs,
/// so a compatible ID may only be set for those classes. NCM typically uses
/// `WINNCM`; RNDIS uses `RNDIS` with sub-compatible ID `5162001`.
fn remember_net_os_descriptor(
    class: function::net::NetClass,
    net: function::net::Net,
    compatible_id: Option<String>,
    sub_compatible_id: Option<String>,
    descriptors: &mut Vec<NetOsDescriptor>,
) -> Result<(), Box<dyn Error>> {
    let Some(compatible_id) = compatible_id else {
        if sub_compatible_id.is_some() {
            return Err("os_sub_compatible_id requires os_compatible_id".into());
        }
        return Ok(());
    };

    let interface = match class {
        function::net::NetClass::Ncm => "ncm",
        function::net::NetClass::Rndis => "rndis",
        other => {
            return Err(
                format!("os_compatible_id is only supported for ncm and rndis, got {other:?}").into()
            )
        }
    };

    descriptors.push(NetOsDescriptor { function: net, interface, compatible_id, sub_compatible_id });
    Ok(())
}

/// Writes the recorded Microsoft OS compatible IDs to configfs.
fn apply_net_os_descriptors(descriptors: &[NetOsDescriptor]) -> Result<(), Box<dyn Error>> {
    for descriptor in descriptors {
        let function_dir = descriptor.function.status().path().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "network function directory not registered")
        })?;
        let os_desc_dir = function_dir.join("os_desc").join(format!("interface.{}", descriptor.interface));
        fs::create_dir_all(&os_desc_dir)?;
        fs::write(os_desc_dir.join("compatible_id"), &descriptor.compatible_id)?;
        if let Some(sub_compatible_id) = &descriptor.sub_compatible_id {
            fs::write(os_desc_dir.join("sub_compatible_id"), sub_compatible_id)?;
        }
    }
    Ok(())
}

/// A bound composite gadget. Dropping it unbinds and removes the gadget.
#[derive(Debug)]
pub struct RunningGadget {
    _reg: RegGadget,
    #[cfg(feature = "dfu")]
    dfu: Option<DfuRuntime>,
    #[cfg(feature = "fbk")]
    fbk: Option<FbkRuntime>,
}

impl RunningGadget {
    /// Takes the DFU runtime out of the gadget, if a DFU function is present.
    /// The gadget registration stays alive as long as `self` is held.
    #[cfg(feature = "dfu")]
    pub fn take_dfu(&mut self) -> Option<DfuRuntime> {
        self.dfu.take()
    }

    /// Takes the FBK runtime out of the gadget, if an FBK function is present.
    #[cfg(feature = "fbk")]
    pub fn take_fbk(&mut self) -> Option<FbkRuntime> {
        self.fbk.take()
    }
}

/// Removes any previously defined gadgets. Call once before binding.
pub fn reset() -> Result<(), Box<dyn Error>> {
    remove_all()?;
    Ok(())
}

/// Builds the gadget described by `cfg`, registers it, and binds it to the
/// given controller. The gadget is named after the controller so multiple
/// controllers can each host their own gadget.
pub fn build_on_udc(cfg: &GadgetConfig, udc: &Udc) -> Result<RunningGadget, Box<dyn Error>> {
    let device = &cfg.device;
    let class =
        Class::new(device.class.unwrap_or(0), device.sub_class.unwrap_or(0), device.protocol.unwrap_or(0));
    let serial_number = serial::resolve(device)?;
    let product_name = product::resolve(device)?;
    let strings = Strings::new(
        device.manufacturer.as_deref().unwrap_or(""),
        &product_name,
        &serial_number,
    );
    let mut gadget = Gadget::new(class, Id::new(device.vendor, device.product), strings);
    gadget.name = Some(udc.name().to_string_lossy().into_owned());
    if let Some(os_descriptor) = &cfg.os_descriptor {
        // Base the gadget OS descriptor on the crate's Microsoft defaults,
        // overriding individual fields only when the configuration sets them.
        let mut desc = OsDescriptor::microsoft();
        if let Some(vendor_code) = os_descriptor.vendor_code {
            desc.vendor_code = vendor_code;
        }
        if let Some(qw_sign) = &os_descriptor.qw_sign {
            desc.qw_sign = qw_sign.clone();
        }
        if let Some(config) = os_descriptor.config {
            desc.config = config;
        }
        gadget.os_descriptor = Some(desc);
    }

    if cfg.config.is_empty() {
        return Err("at least one [[config]] is required".into());
    }

    let mut outputs = FunctionOutputs::default();
    for usb_cfg in &cfg.config {
        let mut config = Config::new(usb_cfg.description.as_deref().unwrap_or(""));
        if let Some(v) = usb_cfg.max_power {
            config.max_power = v;
        }
        if let Some(v) = usb_cfg.self_powered {
            config.self_powered = v;
        }
        if let Some(v) = usb_cfg.remote_wakeup {
            config.remote_wakeup = v;
        }
        for func_cfg in &usb_cfg.function {
            if let Some(handle) = build_function(func_cfg, &serial_number, &mut outputs)? {
                config.functions.push(handle);
            }
        }
        gadget.configs.push(config);
    }

    let reg = gadget.register()?;
    apply_net_os_descriptors(&outputs.net_os)?;
    reg.bind(Some(udc))?;

    log::info!(
        "gadget {:04x}:{:04x} (serial {serial_number}) bound to UDC {}",
        device.vendor,
        device.product,
        udc.name().to_string_lossy()
    );

    #[cfg(feature = "dfu")]
    {
        Ok(RunningGadget {
            _reg: reg,
            dfu: outputs.dfu,
            #[cfg(feature = "fbk")]
            fbk: outputs.fbk,
        })
    }

    #[cfg(not(feature = "dfu"))]
    {
        Ok(RunningGadget {
            _reg: reg,
            #[cfg(feature = "fbk")]
            fbk: outputs.fbk,
        })
    }
}

/// Side outputs collected while building a configuration's functions.
#[derive(Default)]
struct FunctionOutputs {
    /// Network functions needing a Microsoft OS compatible ID written to
    /// configfs after registration (NCM/RNDIS).
    net_os: Vec<NetOsDescriptor>,
    /// DFU runtime captured from a `dfu` function, if any.
    #[cfg(feature = "dfu")]
    dfu: Option<DfuRuntime>,
    /// FBK runtime captured from an `fbk` function, if any.
    #[cfg(feature = "fbk")]
    fbk: Option<FbkRuntime>,
}

/// Builds a function handle from its configuration, mirroring the `usb-gadget`
/// CLI tool's dispatch. Returns `None` when the function is recognized but
/// intentionally skipped (a `dfu` function in a build without the `dfu`
/// feature).
fn build_function(
    cfg: &FunctionConfig,
    serial: &str,
    outputs: &mut FunctionOutputs,
) -> Result<Option<Handle>, Box<dyn Error>> {
    match cfg {
        FunctionConfig::Serial(c) => build_serial(c).map(Some),
        FunctionConfig::Net(c) => build_net(c, &mut outputs.net_os).map(Some),
        FunctionConfig::Msd(c) => build_msd(c).map(Some),
        FunctionConfig::Dfu(c) => build_dfu(c, serial, outputs),
        FunctionConfig::Fbk(c) => build_fbk(c, outputs),
    }
}

fn build_serial(c: &SerialConfig) -> Result<Handle, Box<dyn Error>> {
    let class = match c.class.as_str() {
        "acm" => function::serial::SerialClass::Acm,
        "generic" => function::serial::SerialClass::Generic,
        other => return Err(format!("unknown serial class: {other}").into()),
    };
    let mut b = function::serial::Serial::builder(class);
    b.console = c.console;
    let (_serial, handle) = b.build();
    Ok(handle)
}

fn build_net(c: &NetConfig, net_os: &mut Vec<NetOsDescriptor>) -> Result<Handle, Box<dyn Error>> {
    let class = match c.class.as_str() {
        "ecm" => function::net::NetClass::Ecm,
        "ecm_subset" => function::net::NetClass::EcmSubset,
        "eem" => function::net::NetClass::Eem,
        "ncm" => function::net::NetClass::Ncm,
        "rndis" => function::net::NetClass::Rndis,
        other => return Err(format!("unknown net class: {other}").into()),
    };
    let mut b = function::net::Net::builder(class);
    if let Some(addr) = &c.dev_addr {
        b.dev_addr = Some(addr.parse().map_err(|e| format!("bad dev_addr: {e}"))?);
    }
    if let Some(addr) = &c.host_addr {
        b.host_addr = Some(addr.parse().map_err(|e| format!("bad host_addr: {e}"))?);
    }
    b.qmult = c.qmult;
    if c.interface_class.is_some() || c.interface_sub_class.is_some() || c.interface_protocol.is_some() {
        b.interface_class = Some(Class::new(
            c.interface_class.unwrap_or(0),
            c.interface_sub_class.unwrap_or(0),
            c.interface_protocol.unwrap_or(0),
        ));
    }
    let (net, handle) = b.build();
    remember_net_os_descriptor(class, net, c.os_compatible_id.clone(), c.os_sub_compatible_id.clone(), net_os)?;
    Ok(handle)
}

fn build_msd(c: &MsdConfig) -> Result<Handle, Box<dyn Error>> {
    let mut b = function::msd::Msd::builder();
    b.stall = c.stall;
    for lun_cfg in &c.lun {
        let mut lun = match &lun_cfg.file {
            Some(file) => function::msd::Lun::new(PathBuf::from(file))?,
            None => function::msd::Lun::empty(),
        };
        if let Some(v) = lun_cfg.read_only {
            lun.read_only = v;
        }
        if let Some(v) = lun_cfg.cdrom {
            lun.cdrom = v;
        }
        if let Some(v) = lun_cfg.no_fua {
            lun.no_fua = v;
        }
        if let Some(v) = lun_cfg.removable {
            lun.removable = v;
        }
        if let Some(v) = &lun_cfg.inquiry_string {
            lun.inquiry_string = v.clone();
        }
        b.luns.push(lun);
    }
    let (_msd, handle) = b.build();
    Ok(handle)
}

/// Builds the DFU custom function and captures its runtime in `outputs`.
#[cfg(feature = "dfu")]
fn build_dfu(
    c: &DfuFnConfig,
    serial: &str,
    outputs: &mut FunctionOutputs,
) -> Result<Option<Handle>, Box<dyn Error>> {
    if outputs.dfu.is_some() {
        return Err("configuration declares more than one DFU function".into());
    }
    let (handle, runtime) = crate::dfu::gadget::build(c, serial);
    outputs.dfu = Some(runtime);
    Ok(Some(handle))
}

/// Builds the FBK custom function and captures its runtime in `outputs`.
#[cfg(feature = "fbk")]
fn build_fbk(c: &DfuFnConfig, outputs: &mut FunctionOutputs) -> Result<Option<Handle>, Box<dyn Error>> {
    if outputs.fbk.is_some() {
        return Err("configuration declares more than one FBK function".into());
    }
    let (handle, runtime) = crate::fbk::build(c);
    outputs.fbk = Some(runtime);
    Ok(Some(handle))
}

/// Skips an FBK function in builds compiled without the `fbk` feature.
#[cfg(not(feature = "fbk"))]
fn build_fbk(_c: &DfuFnConfig, _outputs: &mut FunctionOutputs) -> Result<Option<Handle>, Box<dyn Error>> {
    log::warn!("ignoring FBK function because this build was compiled without the fbk feature");
    Ok(None)
}

/// Skips a DFU function in builds compiled without the `dfu` feature.
#[cfg(not(feature = "dfu"))]
fn build_dfu(
    _c: &DfuFnConfig,
    _serial: &str,
    _outputs: &mut FunctionOutputs,
) -> Result<Option<Handle>, Box<dyn Error>> {
    log::warn!("ignoring DFU function because this build was compiled without the dfu feature");
    Ok(None)
}

/// Resets any existing gadget, then binds to all selected controllers —
/// current and future — servicing udev `add` events until the stream ends.
pub async fn serve(config: GadgetConfig) -> Result<(), Box<dyn Error>> {
    reset()?;

    let selection = Selection::from_config(&config.udc);
    log::info!("controller selection: {selection:?}");

    // Create the udev monitor before enumerating existing controllers so no
    // hotplug event is missed in the gap between the two.
    let mut monitor = udc::monitor()?;

    let mut gadgets: Vec<RunningGadget> = Vec::new();
    let mut bound: HashSet<OsString> = HashSet::new();

    for name in udc::existing() {
        bind_one(&config, &selection, &mut bound, &mut gadgets, name);
    }

    if !selection.wants_more(&bound) {
        log::info!("all selected controllers are bound");
    } else {
        log::info!("listening for USB device controllers via udev...");
    }

    while let Some(event) = monitor.next().await {
        let event = match event {
            Ok(event) => event,
            Err(err) => {
                log::warn!("udev monitor error: {err}");
                continue;
            }
        };
        if event.event_type() != EventType::Add {
            continue;
        }
        bind_one(&config, &selection, &mut bound, &mut gadgets, event.sysname().to_os_string());
    }

    Ok(())
}

/// Builds and binds a gadget on controller `name` if the selection allows it.
fn bind_one(
    config: &GadgetConfig,
    selection: &Selection,
    bound: &mut HashSet<OsString>,
    gadgets: &mut Vec<RunningGadget>,
    name: OsString,
) {
    if !selection.wants(&name, bound) {
        return;
    }
    let Some(udc) = udc::by_name(&name) else {
        return;
    };

    match build_on_udc(config, &udc) {
        Ok(running) => {
            #[cfg(feature = "dfu")]
            let mut running = running;

            #[cfg(feature = "dfu")]
            if let Some(dfu_runtime) = running.take_dfu() {
                let udc_name = name.to_string_lossy().into_owned();
                tokio::spawn(crate::dfu::serve(udc_name, dfu_runtime));
            }

            #[cfg(feature = "fbk")]
            if let Some(fbk_runtime) = running.take_fbk() {
                let udc_name = name.to_string_lossy().into_owned();
                tokio::spawn(crate::fbk::serve(udc_name, fbk_runtime));
            }
            gadgets.push(running);
            bound.insert(name);
        }
        Err(err) => log::error!("failed to bind gadget on UDC {}: {err}", name.to_string_lossy()),
    }
}
