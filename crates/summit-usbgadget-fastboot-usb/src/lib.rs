//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

//! Minimal fastboot-usb-compatible custom function.

mod commands;
mod state;

use std::io;
use std::future::Future;
use std::time::Duration;

use bytes::Bytes;
use serde::Deserialize;
use summit_usbgadget_swupdate::{SwupdateConfig, SwupdateConfigError, SwupdateParams, SwupdateSession};
use summit_usbgadget_usb::registry::{FunctionBuildContext, GadgetService, RegisteredFunctionConfig};
use usb_gadget::function::custom::{
    Custom, Endpoint, EndpointDirection, EndpointReceiver, EndpointSender, Interface, OsExtCompat,
};
use usb_gadget::function::Handle;
use usb_gadget::Class;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FastbootUsbFnConfig {
    #[serde(flatten)]
    pub swupdate: SwupdateConfig,
}

impl FastbootUsbFnConfig {
    pub fn to_swupdate_params(&self) -> Result<SwupdateParams, SwupdateConfigError> {
        self.swupdate.to_params()
    }
}

struct FastbootUsbService(FastbootUsbRuntime);

impl GadgetService for FastbootUsbService {
    fn spawn(self: Box<Self>, udc_name: String) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> {
        let runtime = self.0;
        Box::pin(async move { serve(udc_name, runtime).await })
    }
}

impl RegisteredFunctionConfig for FastbootUsbFnConfig {
    fn kind(&self) -> &'static str {
        "fastboot-usb"
    }

    fn build(&self, serial: &str, context: &mut FunctionBuildContext) -> Result<Option<Handle>, Box<dyn std::error::Error>> {
        context.claim_singleton(RegisteredFunctionConfig::kind(self))?;

        let (handle, runtime) = match build(self, serial) {
            Ok(result) => result,
            Err(err) => {
                log::error!("ignoring unsupported fastboot-usb function section: {err}");
                return Ok(None);
            }
        };

        context.push_service(FastbootUsbService(runtime));
        Ok(Some(handle))
    }

    fn clone_box(&self) -> Box<dyn RegisteredFunctionConfig> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

summit_usbgadget_usb::declare_usb_function!("fastboot-usb" => FastbootUsbFnConfig);

/// Fixed capacity used for every receive buffer for the entire lifetime of a
/// bound fastboot-usb function.
/// ci_hdrc (USB Mentor/ChipIdea) rejects any single request larger than 16 KB,
/// so this must not exceed 16384.
const RECV_BUFFER_SIZE: usize = 16 * 1024;
/// Number of receive buffers kept simultaneously submitted to the kernel AIO
/// queue.
const RECV_QUEUE_DEPTH: usize = 1;
const DOWNLOAD_STALL_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) use summit_usbgadget_fastboot_proto::reply::*;

#[derive(Debug)]
pub struct FastbootUsbRuntime {
    custom: Custom,
    rx: EndpointReceiver,
    tx: EndpointSender,
    download: SwupdateParams,
    serial: String,
    /// Fallback used only if the real descriptor query in `serve_connected`
    /// fails; mirrors the max packet size the RX endpoint was actually built
    /// with (`usb_gadget::function::custom::Endpoint::bulk`'s default), so
    /// there is exactly one place this number is defined.
    rx_max_packet_size_default: usize,
}

#[derive(Debug)]
struct FastbootUsbState {
    rx_max_packet_size: usize,
    /// Fallback used only if the real descriptor query in `serve_connected`
    /// fails; see `FastbootUsbRuntime::rx_max_packet_size_default`.
    rx_max_packet_size_default: usize,
    rx: EndpointReceiver,
    tx: EndpointSender,
    download_params: SwupdateParams,
    download: Option<SwupdateSession>,
    fastboot_usb_session_open: bool,
    serial: String,
    download_size: usize,
    downloaded_size: usize,
    fastboot_pending_flash: bool,
    finish_pending: bool,
    /// Set once a bulk-OUT operation sees a closed-transport error
    /// (`is_closed_transport_error`: `ENOTCONN`/`ESHUTDOWN`/`BrokenPipe`, or
    /// ci_hdrc's unbind-time `EINTR`), which means the endpoint is gone for
    /// good (`RunningGadget::shutdown` in `summit-usbgadget-usb/src/gadget.rs`
    /// writes `\n` to the UDC configfs file synchronously, which blocks until
    /// the driver disables the endpoint and force-completes every pending
    /// request). Submitting *further* reads here is actively harmful: a read
    /// queued while the driver is mid-disable can prevent that disable's
    /// request-queue drain from ever completing, hanging the unbind
    /// indefinitely. So this is a one-way, unrecoverable signal — never
    /// cleared by `reset_state()` — that permanently stops both `submit_recv`
    /// and `data_loop` from ever touching the endpoint again for this
    /// instance, regardless of which specific closed-transport error tripped
    /// it.
    gadget_torn_down: bool,
}

pub fn build(cfg: &FastbootUsbFnConfig, serial: &str) -> Result<(Handle, FastbootUsbRuntime), SwupdateConfigError> {
    let download = cfg.to_swupdate_params()?;
    let (rx, rx_dir) = EndpointDirection::host_to_device();
    let (tx, tx_dir) = EndpointDirection::device_to_host();

    // The endpoint builder is the one place this default is configured; read
    // it back instead of duplicating the literal as a separate fallback.
    let rx_endpoint = Endpoint::bulk(rx_dir.with_queue_len(RECV_QUEUE_DEPTH as u32));
    let rx_max_packet_size_default = rx_endpoint.max_packet_size_hs as usize;

    let (custom, handle) = Custom::builder()
        .with_interface(
            Interface::new(Class::new(0xff, 0x42, 0x03), "fastboot-usb")
                .with_endpoint(rx_endpoint)
                .with_endpoint(Endpoint::bulk(tx_dir))
                .with_os_ext_compat(OsExtCompat::winusb()),
        )
        .build();

    Ok((
        handle,
        FastbootUsbRuntime {
            custom,
            rx,
            tx,
            download,
            serial: serial.to_owned(),
            rx_max_packet_size_default,
        },
    ))
}

/// Builds one fastboot-usb function, storing its runtime when supported and skipping
/// unsupported sections after logging the configuration error.
pub fn build_fastboot_usb(
    cfg: &FastbootUsbFnConfig,
    serial: &str,
    runtime_slot: &mut Option<FastbootUsbRuntime>,
) -> Result<Option<Handle>, Box<dyn std::error::Error>> {
    if runtime_slot.is_some() {
        return Err("configuration declares more than one fastboot-usb function".into());
    }

    let (handle, runtime) = match build(cfg, serial) {
        Ok(result) => result,
        Err(err) => {
            log::error!("ignoring unsupported fastboot-usb function section: {err}");
            return Ok(None);
        }
    };

    *runtime_slot = Some(runtime);
    Ok(Some(handle))
}

/// Writes a fixed reply token onto the fastboot-usb bulk IN endpoint.
pub(crate) async fn send_static(tx: &mut EndpointSender, data: &'static [u8]) -> io::Result<()> {
    tx.send_async(Bytes::from_static(data)).await
}

pub async fn serve(udc_name: String, mut runtime: FastbootUsbRuntime) {
    let mut state = FastbootUsbState::new(
        runtime.rx,
        runtime.tx,
        runtime.download,
        runtime.serial,
        runtime.rx_max_packet_size_default,
    );

    // Split the function into two independent async planes that communicate only
    // through `enabled`: the generic endpoint-zero control loop
    // (`summit_usbgadget_usb::functionfs::serve`, shared with DFU) drives a
    // fastboot-specific `FastbootControl` callback that publishes the host's
    // configured state, and the bulk data loop observes it and reacts in its own
    // iteration. Neither blocks the other, and the data loop never touches a USB
    // endpoint until the control loop reports the function enabled. When either
    // loop ends (gadget unbound), the other is dropped with it.
    let (enabled_tx, enabled_rx) = tokio::sync::watch::channel(false);
    let mut control = state::FastbootControl::new(enabled_tx);

    tokio::select! {
        _ = summit_usbgadget_usb::functionfs::serve(
            udc_name.clone(),
            "fastboot-usb upload function",
            &mut runtime.custom,
            &mut control,
        ) => {}
        _ = state.data_loop(&udc_name, enabled_rx) => {}
    }
}
