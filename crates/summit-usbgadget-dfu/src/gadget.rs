//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! Gadget-side integration of the DFU function.
//!
//! Builds the FunctionFS-backed custom interface that advertises the DFU
//! runtime descriptor, and runs the endpoint-zero event loop that drives the
//! [`Dfu`](super::Dfu) state machine. The DFU wire protocol itself lives in
//! [`super::protocol`] and the state machine in [`super::Dfu`].

use usb_gadget::function::custom::{Custom, Event, Interface};
use usb_gadget::function::Handle;
use usb_gadget::Class;

use summit_usbgadget_usb::functionfs::{is_closed_transport_error, serve as functionfs_serve, EventHandler};

use crate::config::DfuConfigError;
use crate::protocol::request;

use super::{Dfu, DfuConfig, DfuFnConfig};

/// The DFU runtime pieces needed to service control requests after binding.
#[derive(Debug)]
pub struct DfuRuntime {
    /// The custom endpoint-zero interface.
    pub custom: Custom,
    /// The DFU configuration used to drive the state machine.
    pub config: DfuConfig,
}

/// Builds the DFU custom function, returning its function handle and the runtime
/// used to service control requests once the gadget is bound.
pub fn build(cfg: &DfuFnConfig, serial: &str) -> Result<(Handle, DfuRuntime), DfuConfigError> {
    let config = cfg.to_config(serial)?;
    let (custom, handle) = Custom::builder()
        .with_interface(
            Interface::new(Class::DFU_MODE, "swupdate").with_custom_desc(config.descriptor().into()),
        )
        .build();
    Ok((handle, DfuRuntime { custom, config }))
}

/// Builds one DFU function, storing its runtime when supported and skipping
/// unsupported sections after logging the configuration error.
pub fn build_dfu(
    cfg: &DfuFnConfig,
    serial: &str,
    runtime_slot: &mut Option<DfuRuntime>,
) -> Result<Option<Handle>, Box<dyn std::error::Error>> {
    if runtime_slot.is_some() {
        return Err("configuration declares more than one DFU function".into());
    }

    let (handle, runtime) = match build(cfg, serial) {
        Ok(result) => result,
        Err(err) => {
            log::error!("ignoring unsupported DFU function section: {err}");
            return Ok(None);
        }
    };

    *runtime_slot = Some(runtime);
    Ok(Some(handle))
}

/// Services the DFU endpoint-zero event loop for one gadget until its task is
/// dropped (when the gadget is removed).
pub async fn serve(udc_name: String, mut runtime: DfuRuntime) {
    let mut handler = Dfu::new(runtime.config.clone());
    functionfs_serve(
        udc_name,
        "DFU control requests",
        &mut runtime.custom,
        &mut handler,
    )
    .await;
}

impl EventHandler for Dfu {
    /// Idle timeout only while a transfer is in progress; see [`Dfu::dfu_idle_timeout`].
    fn idle_timeout(&self) -> Option<std::time::Duration> {
        self.dfu_idle_timeout()
    }

    /// The host went quiet mid-transfer: reset instead of leaving it stuck.
    async fn on_idle_timeout(&mut self, _udc_name: &str) {
        self.reset_on_idle_timeout().await;
    }

    /// End the SWUpdate input stream when the host disconnects before sending
    /// the normal zero-length DFU_DNLOAD manifestation request.
    async fn on_transport_closed(&mut self, _udc_name: &str) {
        self.reset_on_idle_timeout().await;
    }

    /// Dispatches a single FunctionFS event to the DFU state machine.
    async fn handle_event(&mut self, _udc_name: &str, event: Event<'_>) -> std::io::Result<()> {
        match event {
            Event::SetupHostToDevice(req) => {
                let ctrl = req.ctrl_req().clone();
                log::debug!(
                    "DFU OUT setup: request={:#04x} value={} index={} length={}",
                    ctrl.request,
                    ctrl.value,
                    ctrl.index,
                    ctrl.length,
                );
                if !super::is_dfu_request(&ctrl) {
                    log::debug!("ignoring non-DFU OUT request {:#04x}", ctrl.request);
                    let _ = req.recv_all_async().await;
                    return Ok(());
                }
                // DFU_DNLOAD data blocks are read straight into a recycled
                // buffer to avoid an intermediate allocation and copy. Only
                // DFU_DNLOAD carries an OUT data stage.
                if ctrl.request == request::DNLOAD && !req.is_empty() {
                    return match self.receive_dnload_block(req).await {
                        Ok(()) => Ok(()),
                        Err(err) if is_closed_transport_error(&err) => {
                            self.reset_on_idle_timeout().await;
                            Ok(())
                        }
                        Err(err) => Err(err),
                    };
                }
                // Remaining DFU OUT requests carry no download payload; drain
                // the request using its declared length to complete the
                // control transfer, then dispatch. In particular, do not read
                // a probe byte for a zero-length request: real FunctionFS
                // blocks because that control request has no data stage.
                match req.recv_all_async().await {
                    Ok(_) => {}
                    Err(err) if is_closed_transport_error(&err) => {
                        self.reset_on_idle_timeout().await;
                        return Ok(());
                    }
                    Err(err) => return Err(err),
                }
                self.handle_out(&ctrl).await
            }
            Event::SetupDeviceToHost(req) => {
                let ctrl = req.ctrl_req().clone();
                log::debug!(
                    "DFU IN setup: request={:#04x} value={} index={} length={}",
                    ctrl.request,
                    ctrl.value,
                    ctrl.index,
                    ctrl.length,
                );
                if !super::is_dfu_request(&ctrl) {
                    log::debug!("ignoring non-DFU IN request {:#04x}", ctrl.request);
                    let _ = req.send_async(&[]).await;
                    return Ok(());
                }
                let response = self.handle_in(&ctrl).await?;
                if let Err(err) = req.send_async(response.as_slice()).await {
                    if is_closed_transport_error(&err) {
                        return Ok(());
                    }
                    return Err(err);
                }
                Ok(())
            }
            Event::Enable => {
                log::info!("DFU function enabled");
                Ok(())
            }
            Event::Disable => {
                log::info!("DFU function disabled");
                self.reset_on_idle_timeout().await;
                Ok(())
            }
            Event::Unbind => {
                log::info!("DFU function unbound");
                self.reset_on_idle_timeout().await;
                Ok(())
            }
            other => {
                log::debug!("event: {other:?}");
                Ok(())
            }
        }
    }
}
