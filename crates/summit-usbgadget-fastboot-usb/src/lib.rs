//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

//! Minimal fastboot-usb-compatible custom function.

mod commands;
mod functionfs;
mod state;

use std::time::Duration;

use serde::Deserialize;
use summit_usbgadget_swupdate::{SwupdateConfig, SwupdateConfigError, SwupdateParams, SwupdateSession};
use summit_usbgadget_usb::registry::{FunctionBuildContext, GadgetService, RegisteredFunctionConfig};
use usb_gadget::function::custom::{
    Custom, Endpoint, EndpointDirection, EndpointReceiver, EndpointSender, Interface, OsExtCompat,
};
use usb_gadget::function::Handle;
use usb_gadget::Class;

#[derive(Debug, Clone, Deserialize)]
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
    fn spawn(self: Box<Self>, udc_name: String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
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
pub(crate) use summit_usbgadget_fastboot_proto::{
    FAIL_BADSIZE, FAIL_CLOSE, FAIL_CMD, FAIL_EPIPE, FAIL_FLASH, FAIL_NOTOPEN, FAIL_OPEN,
    FAIL_UNKNOWN_PART, INFO_WAIT_SWUPDATE, OKAY,
};

#[derive(Debug)]
pub struct FastbootUsbRuntime {
    custom: Custom,
    rx: EndpointReceiver,
    tx: EndpointSender,
    download: SwupdateParams,
    serial: String,
}

#[derive(Debug, Clone, Copy)]
enum EndpointAction {
    Cancel,
    Halt,
}

#[derive(Debug)]
struct FastbootUsbState {
    rx_max_packet_size: usize,
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
    /// Whether the function is currently enabled by a connected host. While
    /// disabled (e.g. no USB cable attached) no bulk-OUT read is primed, so the
    /// kernel AIO context has nothing in flight and teardown cannot block.
    enabled: bool,
}

pub fn build(cfg: &FastbootUsbFnConfig, serial: &str) -> Result<(Handle, FastbootUsbRuntime), SwupdateConfigError> {
    let download = cfg.to_swupdate_params()?;
    let (rx, rx_dir) = EndpointDirection::host_to_device();
    let (tx, tx_dir) = EndpointDirection::device_to_host();

    let (custom, handle) = Custom::builder()
        .with_interface(
            Interface::new(Class::new(0xff, 0x42, 0x03), "fastboot-usb")
                .with_endpoint(Endpoint::bulk(rx_dir.with_queue_len(RECV_QUEUE_DEPTH as u32)))
                .with_endpoint(Endpoint::bulk(tx_dir))
                .with_os_ext_compat(OsExtCompat::winusb()),
        )
        .build();

    Ok((handle, FastbootUsbRuntime { custom, rx, tx, download, serial: serial.to_owned() }))
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

pub async fn serve(udc_name: String, mut runtime: FastbootUsbRuntime) {
    log::info!("[{udc_name}] servicing fastboot-usb upload function");

    let mut state = FastbootUsbState::new(
        runtime.rx,
        runtime.tx,
        runtime.download,
        runtime.serial,
    );

    state.run(&udc_name, &mut runtime.custom).await;
}
