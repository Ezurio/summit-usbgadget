//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
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
use std::ffi::{OsStr, OsString};
use std::fs;
use std::future::{pending, Future};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::registry::FunctionBuildContext;
use futures_util::future::join_all;
use futures_util::StreamExt;
use summit_usbgadget_config::{PluginConfig, Shutdown};
use tokio::task::JoinHandle;
use tokio_udev::EventType;
use usb_gadget::function::{self, Handle};
use usb_gadget::{
    registered, remove_all, Class, Config, Gadget, Id, OsDescriptor, RegGadget, Speed, Strings,
    Udc, UsbVersion,
};

use crate::config::{FunctionConfig, GadgetConfig, MsdConfig, NetConfig, SerialConfig};
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
pub struct RunningGadget {
    tasks: Vec<JoinHandle<()>>,
    services: Vec<Box<dyn crate::registry::GadgetService>>,
    _reg: RegGadget,
}

impl RunningGadget {
    /// Name of the UDC this gadget is bound to (the gadget's configfs name,
    /// which is set equal to the UDC name when it is built).
    fn udc_name(&self) -> &OsStr {
        self._reg.name()
    }

    fn push_task(&mut self, task: JoinHandle<()>) {
        self.tasks.push(task);
    }

    fn start_services(&mut self, udc_name: &str) {
        for service in std::mem::take(&mut self.services) {
            self.push_task(tokio::spawn(service.spawn(udc_name.to_owned())));
        }
    }

    async fn shutdown(mut self) {
        // Unbind from the UDC before touching tasks.  On ARM platforms using
        // the ci_hdrc USB controller, calling io_cancel (or dropping an AIO
        // context, which does the same thing via io_destroy) while a
        // bulk-OUT DMA transfer is in flight triggers a kernel crash:
        //   ffs_aio_cancel → ep_dequeue [ci_hdrc] → usb_gadget_unmap_request
        //   → dma_direct_unmap_sg → dcache_inval_poc (fault)
        // Confirmed on real hardware: reversing this order (cancel/abort
        // tasks first, unbind after) crashed the kernel. Do not reorder this
        // again without a real fix for whatever made `bind(None)` hang below.
        //
        // Writing "\n" to the UDC configfs file is a synchronous kernel call
        // that stops the controller and completes (with error) every pending
        // FunctionFS AIO request before returning.  After that, io_cancel
        // finds no in-flight operation and returns EINVAL without touching DMA.
        //
        // This process runs a single-threaded (`current_thread`) tokio
        // runtime (`src/main.rs`), so a blocking call made directly here --
        // as opposed to via `spawn_blocking` -- would freeze the *entire*
        // process for as long as it takes: no other task (crucially,
        // including every FunctionFS task this same `shutdown()` is about to
        // wait on below, which are relied on to notice the forced teardown
        // and exit on their own) can be scheduled at all while the one OS
        // thread driving the runtime is stuck in this syscall. Run it on the
        // blocking pool instead, exactly like `bind_one` already does for
        // `build_on_udc`, so those tasks can keep making progress no matter
        // how long the kernel takes to complete the disable.
        let reg = self._reg;
        let (reg, bind_result) = tokio::task::spawn_blocking(move || {
            let result = reg.bind(None);
            (reg, result)
        })
        .await
        .expect("bind(None) task panicked");
        self._reg = reg;
        if let Err(err) = bind_result {
            log::warn!("failed to unbind gadget from UDC before task shutdown: {err}");
        }

        let tasks = std::mem::take(&mut self.tasks);

        // Endpoint-zero control loops (DFU, and fastboot-usb's control plane --
        // both drive `functionfs::serve`) don't use cancellable AIO: `CtrlReceiver`/
        // `CtrlSender` recv/send a control transfer with a plain blocking
        // read/write run on a `spawn_blocking` thread. Aborting such a task while
        // that blocking call is in flight only unwinds the *outer* future --
        // `JoinHandle::abort`/dropping the inner `spawn_blocking` `JoinHandle`
        // does not stop the detached OS thread, which keeps the read/write
        // syscall running on ep0. If that orphaned syscall loses the race
        // against the rest of gadget teardown (this fn returning, `RegGadget`
        // being dropped, the FunctionFS instance going away), the driver logs a
        // spurious-looking but symptomatic error, e.g.:
        //   dwc3 ...: request ... was not queued to ep0out
        //
        // The unbind above already forces any pending ep0 (and bulk AIO)
        // request to complete with an error, so in the common case every task
        // notices via its own `is_torn_down_transport_error` check and returns
        // on its own almost immediately -- which naturally drains any
        // `spawn_blocking` thread before we consider the task done. Give tasks
        // a short grace period to exit that way; only hard-`abort()` ones that
        // are still stuck afterwards (e.g. blocked on something unbind can't
        // unblock), as a fallback rather than the default path.
        const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
        let joins = tasks.into_iter().map(|mut task| async move {
            match tokio::time::timeout(SHUTDOWN_GRACE, &mut task).await {
                Ok(result) => result,
                Err(_) => {
                    task.abort();
                    task.await
                }
            }
        });

        for result in join_all(joins).await {
            match result {
                Ok(()) => {}
                Err(err) if err.is_cancelled() => {}
                Err(err) => log::warn!("FunctionFS task ended with error during shutdown: {err}"),
            }
        }
    }
}

/// Removes any previously defined gadgets. Call once before binding.
///
/// The gadget configfs hierarchy only exists once configfs is mounted and
/// `libcomposite` is loaded — which normally happens as a UDC appears. Before
/// that there is simply nothing to reset, so a missing configfs/`usb_gadget`
/// directory is not an error: the service starts quietly and the udev monitor
/// binds the gadget when the controller shows up. Only propagate other failures.
///
/// `remove_all` performs synchronous configfs I/O, so it runs on a
/// blocking-pool thread (see [`bind_one`]) instead of the shared async runtime.
pub async fn reset() -> Result<(), Box<dyn Error>> {
    match tokio::task::spawn_blocking(remove_all).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) if err.kind() == io::ErrorKind::NotFound => {
            log::debug!("no existing gadgets to reset ({err}); waiting for a UDC via udev");
            Ok(())
        }
        Ok(Err(err)) => Err(err.into()),
        Err(join_err) => Err(format!("gadget reset task panicked: {join_err}").into()),
    }
}

/// Builds the gadget described by `cfg`, registers it, and binds it to the
/// given controller. The gadget is named after the controller so multiple
/// controllers can each host their own gadget.
pub fn build_on_udc(cfg: &GadgetConfig, udc: &Udc) -> Result<RunningGadget, Box<dyn Error>> {
    match build_on_udc_once(cfg, udc) {
        Ok(running) => Ok(running),
        Err(err) => {
            let Some(filtered) = config_without_dfu(cfg, &*err) else {
                return Err(err);
            };

            log::warn!(
                "skipping unsupported DFU interface on UDC {} after registration failure: {err}",
                udc.name().to_string_lossy()
            );

            build_on_udc_once(&filtered, udc).map_err(|retry_err| {
                format!(
                    "retry without DFU after registration failure on UDC {} also failed: {retry_err}",
                    udc.name().to_string_lossy()
                )
                .into()
            })
        }
    }
}

fn build_on_udc_once(cfg: &GadgetConfig, udc: &Udc) -> Result<RunningGadget, Box<dyn Error>> {
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
    let gadget_name = udc.name().to_os_string();
    let mut gadget = Gadget::new(class, Id::new(device.vendor, device.product), strings);
    gadget.name = Some(gadget_name.to_string_lossy().into_owned());
    match udc.max_speed() {
        Ok(Speed::SuperSpeed) => {
            gadget.usb_version = UsbVersion::V30;
            gadget.max_speed = Some(Speed::SuperSpeed);
        }
        Ok(Speed::SuperSpeedPlus) => {
            gadget.usb_version = UsbVersion::V31;
            gadget.max_speed = Some(Speed::SuperSpeedPlus);
        }
        Ok(Speed::Unknown) | Err(_) => {}
        Ok(speed) => {
            gadget.max_speed = Some(speed);
        }
    }
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

    let reg = gadget.register().map_err(|err| {
        cleanup_failed_registration_by_name(
            &gadget_name,
            format!(
                "failed to register gadget for UDC {} before bind: {err}",
                udc.name().to_string_lossy()
            )
            .into(),
        )
    })?;
    if let Err(err) = apply_net_os_descriptors(&outputs.net_os).map_err(|err| {
        format!(
            "failed to finish gadget setup in configfs at {}: {err}",
            reg.path().display()
        )
        .into()
    }) {
        return Err(cleanup_failed_registration(reg, err));
    }

    if let Err(err) = reg.bind(Some(udc)).map_err(|err| {
        format!(
            "failed to bind registered gadget at {} to UDC {}: {err}",
            reg.path().display(),
            udc.name().to_string_lossy()
        )
        .into()
    }) {
        return Err(cleanup_failed_registration(reg, err));
    }

    log::info!(
        "gadget {:04x}:{:04x} (serial {serial_number}) bound to UDC {}",
        device.vendor,
        device.product,
        udc.name().to_string_lossy()
    );

    Ok(RunningGadget {
        tasks: Vec::new(),
        services: outputs.plugin.into_services(),
        _reg: reg,
    })
}

fn config_without_dfu(cfg: &GadgetConfig, err: &dyn Error) -> Option<GadgetConfig> {
    if !err.to_string().contains("FunctionFS") {
        return None;
    }

    let mut filtered = cfg.clone();
    let mut removed_dfu = false;

    filtered.config.retain_mut(|usb_cfg| {
        let original_len = usb_cfg.function.len();
        usb_cfg.function.retain(|function| function.kind() != "dfu");
        removed_dfu |= usb_cfg.function.len() != original_len;
        !usb_cfg.function.is_empty()
    });

    if removed_dfu && !filtered.config.is_empty() {
        Some(filtered)
    } else {
        None
    }
}

fn cleanup_failed_registration(reg: RegGadget, err: Box<dyn Error>) -> Box<dyn Error> {
    let gadget_name = reg.name().to_string_lossy().into_owned();
    match reg.remove() {
        Ok(()) => err,
        Err(cleanup_err) => format!(
            "{err}; additionally failed to remove partially registered gadget {gadget_name}: {cleanup_err}"
        )
        .into(),
    }
}

fn cleanup_failed_registration_by_name(name: &OsStr, err: Box<dyn Error>) -> Box<dyn Error> {
    let gadget_name = name.to_string_lossy().into_owned();

    let gadgets = match registered() {
        Ok(gadgets) => gadgets,
        Err(cleanup_err) => {
            return format!(
                "{err}; additionally failed to enumerate registered gadgets while cleaning up partially registered gadget {gadget_name}: {cleanup_err}"
            )
            .into();
        }
    };

    let Some(gadget) = gadgets.into_iter().find(|gadget| gadget.name() == name) else {
        return err;
    };

    match gadget.remove() {
        Ok(()) => err,
        Err(cleanup_err) => format!(
            "{err}; additionally failed to remove partially registered gadget {gadget_name}: {cleanup_err}"
        )
        .into(),
    }
}

/// Side outputs collected while building a configuration's functions.
#[derive(Default)]
struct FunctionOutputs {
    /// Network functions needing a Microsoft OS compatible ID written to
    /// configfs after registration (NCM/RNDIS).
    net_os: Vec<NetOsDescriptor>,
    /// Plugin services captured from registered custom functions.
    plugin: FunctionBuildContext,
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
        FunctionConfig::Plugin(config) => config.build(serial, &mut outputs.plugin),
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

/// Resets any existing gadget, then binds to all selected controllers —
/// current and future — servicing relevant `udc` udev events until the stream
/// ends.
pub async fn serve(config: GadgetConfig) -> Result<(), Box<dyn Error>> {
    serve_until(config, pending()).await
}

/// Runs the USB gadget anchor: loads the gadget configuration from
/// `config_path`, serves it until `shutdown` is requested, and tears down any
/// gadgets on failure or shutdown.
///
/// This owns all USB-specific orchestration — configuration loading, cleanup on
/// a failed load, and the serve loop — while the shared [`Shutdown`] signal is
/// owned by the composing binary.
pub async fn run(config_path: impl AsRef<Path>, shutdown: Shutdown) -> Result<(), Box<dyn Error>> {
    let config_path = config_path.as_ref();
    log::info!("loading gadget configuration from {}", config_path.display());
    let config = match GadgetConfig::load(config_path) {
        Ok(config) => config,
        Err(err) => {
            if let Err(reset_err) = reset().await {
                log::warn!("failed to remove gadgets after configuration load error: {reset_err}");
            }
            return Err(err.into());
        }
    };

    serve_until(config, shutdown.wait()).await
}

/// Like [`serve`], but stops when `shutdown` resolves and tears down spawned
/// FunctionFS tasks before dropping gadget registrations.
pub async fn serve_until<F>(config: GadgetConfig, shutdown: F) -> Result<(), Box<dyn Error>>
where
    F: Future<Output = ()>,
{
    reset().await?;

    let selection = Selection::from_config(&config.udc);
    log::info!("controller selection: {selection:?}");

    // Create the udev monitor before enumerating existing controllers so no
    // hotplug event is missed in the gap between the two.
    let mut monitor = udc::monitor()?;
    tokio::pin!(shutdown);

    let mut gadgets: Vec<RunningGadget> = Vec::new();
    let mut bound: HashSet<OsString> = HashSet::new();

    bind_existing(&config, &selection, &mut bound, &mut gadgets).await;

    if !selection.wants_more(&bound) {
        log::info!("all selected controllers are bound");
    } else {
        log::info!("listening for USB device controllers via udev...");
    }

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                log::info!("shutdown requested, stopping USB gadgets");
                break;
            }
            maybe_event = monitor.next() => {
                let Some(event) = maybe_event else {
                    break;
                };

                let event = match event {
                    Ok(event) => event,
                    Err(err) => {
                        log::warn!("udev monitor error: {err}");
                        continue;
                    }
                };
                let Some((name, event_type)) = udc_name_from_event(&event) else {
                    continue;
                };
                match event_type {
                    EventType::Add => bind_one(&config, &selection, &mut bound, &mut gadgets, name).await,
                    EventType::Remove => unbind_one(&mut bound, &mut gadgets, name).await,
                    _ => {}
                }
            }
        }
    }

    shutdown_all(gadgets).await;

    Ok(())
}

async fn shutdown_all(gadgets: Vec<RunningGadget>) {
    for gadget in gadgets {
        gadget.shutdown().await;
    }
}

async fn bind_existing(
    config: &GadgetConfig,
    selection: &Selection,
    bound: &mut HashSet<OsString>,
    gadgets: &mut Vec<RunningGadget>,
) {
    for name in udc::existing() {
        bind_one(config, selection, bound, gadgets, name).await;
    }
}

fn udc_name_from_event(event: &tokio_udev::Event) -> Option<(OsString, EventType)> {
    match event.event_type() {
        event_type @ (EventType::Add | EventType::Remove) => Some((event.sysname().to_os_string(), event_type)),
        _ => None,
    }
}

/// Tears down the gadget bound to controller `name` when its udev device is
/// removed, and forgets it so a later `Add` for the same name rebuilds it from
/// scratch.
///
/// Without this, a controller that disappears and reappears (module reload,
/// core reset, ...) would be silently ignored forever: `Selection::wants`
/// treats any name still in `bound` as already handled, so the stale
/// `RunningGadget` -- and its by-then-dangling configfs registration -- would
/// never be replaced, leaving the gadget permanently non-functional even
/// though this process keeps running.
async fn unbind_one(bound: &mut HashSet<OsString>, gadgets: &mut Vec<RunningGadget>, name: OsString) {
    if !bound.remove(&name) {
        return;
    }
    let Some(pos) = gadgets.iter().position(|gadget| gadget.udc_name() == name.as_os_str()) else {
        log::warn!("UDC {} removed but no matching gadget was tracked", name.to_string_lossy());
        return;
    };
    log::info!("UDC {} removed, tearing down its gadget", name.to_string_lossy());
    gadgets.remove(pos).shutdown().await;
}

/// Builds and binds a gadget on controller `name` if the selection allows it.
///
/// Building and binding a gadget performs many synchronous configfs reads and
/// writes (and can block waiting on the kernel). The whole process runs on a
/// blocking-pool thread via [`tokio::task::spawn_blocking`] so it never stalls
/// the async runtime — this service shares a single-threaded runtime with
/// every other plugin (e.g. fastboot-over-TCP), and a blocking call made
/// directly from a polled future would freeze all of them for its duration.
async fn bind_one(
    config: &GadgetConfig,
    selection: &Selection,
    bound: &mut HashSet<OsString>,
    gadgets: &mut Vec<RunningGadget>,
    name: OsString,
) {
    if !selection.wants(&name, bound) {
        return;
    }

    let config = config.clone();
    let build_name = name.clone();
    let result = tokio::task::spawn_blocking(move || {
        let udc = udc::by_name(&build_name)?;
        Some(build_on_udc(&config, &udc).map_err(|err| err.to_string()))
    })
    .await;

    let build_result = match result {
        Ok(Some(result)) => result,
        Ok(None) => return,
        Err(join_err) => {
            log::error!("gadget build task for UDC {} panicked: {join_err}", name.to_string_lossy());
            return;
        }
    };

    match build_result {
        Ok(running) => {
            #[allow(unused_mut)]
            let mut running = running;

            let udc_name = name.to_string_lossy().into_owned();
            running.start_services(&udc_name);
            gadgets.push(running);
            let inserted = bound.insert(name);
            debug_assert!(inserted);
        }
        Err(err) => log::error!("failed to bind gadget on UDC {}: {err}", name.to_string_lossy()),
    }
}

