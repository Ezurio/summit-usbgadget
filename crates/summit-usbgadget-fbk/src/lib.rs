//
// SPDX-License-Identifier: MIT OR Apache-2.0
//

//! Minimal FBK-compatible custom function.

mod commands;
mod functionfs;
mod protocol;
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
pub struct FbkFnConfig {
    #[serde(flatten)]
    pub swupdate: SwupdateConfig,
}

impl FbkFnConfig {
    pub fn to_swupdate_params(&self) -> Result<SwupdateParams, SwupdateConfigError> {
        self.swupdate.to_params()
    }
}

struct FbkService(FbkRuntime);

impl GadgetService for FbkService {
    fn spawn(self: Box<Self>, udc_name: String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let runtime = self.0;
        Box::pin(async move { serve(udc_name, runtime).await })
    }
}

impl RegisteredFunctionConfig for FbkFnConfig {
    fn kind(&self) -> &'static str {
        "fbk"
    }

    fn build(&self, serial: &str, context: &mut FunctionBuildContext) -> Result<Option<Handle>, Box<dyn std::error::Error>> {
        context.claim_singleton(RegisteredFunctionConfig::kind(self))?;

        let (handle, runtime) = match build(self, serial) {
            Ok(result) => result,
            Err(err) => {
                log::error!("ignoring unsupported FBK function section: {err}");
                return Ok(None);
            }
        };

        context.push_service(FbkService(runtime));
        Ok(Some(handle))
    }

    fn clone_box(&self) -> Box<dyn RegisteredFunctionConfig> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

summit_usbgadget_usb::declare_usb_function!("fbk" => FbkFnConfig);

/// Fixed capacity used for every receive buffer for the entire lifetime of a
/// bound FBK function.
const RECV_BUFFER_SIZE: usize = 128 * 1024;
/// Number of receive buffers kept simultaneously submitted to the kernel AIO
/// queue.
const RECV_QUEUE_DEPTH: usize = 1;
const DOWNLOAD_STALL_TIMEOUT: Duration = Duration::from_secs(5);
const OKAY: &[u8] = b"OKAY";
const FAIL_BADSIZE: &[u8] = b"FAILbad size";
const FAIL_CLOSE: &[u8] = b"FAILclose";
const FAIL_CMD: &[u8] = b"FAILunknown command";
const FAIL_EPIPE: &[u8] = b"FAILwrite failed";
const FAIL_FLASH: &[u8] = b"FAILflash before download";
const FAIL_NOTOPEN: &[u8] = b"FAILnot open";
const FAIL_OPEN: &[u8] = b"FAILopen failed";
const FAIL_UNKNOWN_PART: &[u8] = b"FAILpartition does not exist";
const INFO_WAIT_SWUPDATE: &[u8] = b"INFOwaiting for SWUpdate";

#[derive(Debug)]
pub struct FbkRuntime {
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
struct FbkState {
    rx_max_packet_size: usize,
    rx: EndpointReceiver,
    tx: EndpointSender,
    download_params: SwupdateParams,
    download: Option<SwupdateSession>,
    fbk_session_open: bool,
    serial: String,
    download_size: usize,
    downloaded_size: usize,
    fastboot_pending_flash: bool,
    finish_pending: bool,
}

pub fn build(cfg: &FbkFnConfig, serial: &str) -> Result<(Handle, FbkRuntime), SwupdateConfigError> {
    let download = cfg.to_swupdate_params()?;
    let (rx, rx_dir) = EndpointDirection::host_to_device();
    let (tx, tx_dir) = EndpointDirection::device_to_host();

    let (custom, handle) = Custom::builder()
        .with_interface(
            Interface::new(Class::new(0xff, 0x42, 0x03), "FBK")
                .with_endpoint(Endpoint::bulk(rx_dir.with_queue_len(RECV_QUEUE_DEPTH as u32)))
                .with_endpoint(Endpoint::bulk(tx_dir))
                .with_os_ext_compat(OsExtCompat::winusb()),
        )
        .build();

    Ok((handle, FbkRuntime { custom, rx, tx, download, serial: serial.to_owned() }))
}

/// Builds one FBK function, storing its runtime when supported and skipping
/// unsupported sections after logging the configuration error.
pub fn build_fbk(
    cfg: &FbkFnConfig,
    serial: &str,
    runtime_slot: &mut Option<FbkRuntime>,
) -> Result<Option<Handle>, Box<dyn std::error::Error>> {
    if runtime_slot.is_some() {
        return Err("configuration declares more than one FBK function".into());
    }

    let (handle, runtime) = match build(cfg, serial) {
        Ok(result) => result,
        Err(err) => {
            log::error!("ignoring unsupported FBK function section: {err}");
            return Ok(None);
        }
    };

    *runtime_slot = Some(runtime);
    Ok(Some(handle))
}

pub async fn serve(udc_name: String, mut runtime: FbkRuntime) {
    log::info!("[{udc_name}] servicing FBK upload function");

    let mut state = FbkState::new(
        runtime.rx,
        runtime.tx,
        runtime.download,
        runtime.serial,
    );

    state.run(&udc_name, &mut runtime.custom).await;
}
