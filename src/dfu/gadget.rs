//
// SPDX-License-Identifier: MIT OR Apache-2.0
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

use crate::config::DfuFnConfig;
use crate::functionfs::EventHandler;

use super::{Dfu, DfuConfig};

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
pub fn build(cfg: &DfuFnConfig, serial: &str) -> (Handle, DfuRuntime) {
    let config = cfg.to_config(serial);
    let (custom, handle) = Custom::builder()
        .with_interface(
            Interface::new(Class::DFU_RUNTIME, "swupdate").with_custom_desc(config.descriptor().into()),
        )
        .build();
    (handle, DfuRuntime { custom, config })
}

/// Services the DFU endpoint-zero event loop for one gadget until its task is
/// dropped (when the gadget is removed).
pub async fn serve(udc_name: String, mut runtime: DfuRuntime) {
    let mut handler = Dfu::new(runtime.config.clone());
    crate::functionfs::serve(
        udc_name,
        "DFU control requests",
        &mut runtime.custom,
        &mut handler,
    )
    .await;
}

impl EventHandler for Dfu {
    /// Dispatches a single FunctionFS event to the DFU state machine.
    async fn handle_event(&mut self, _udc_name: &str, event: Event<'_>) -> std::io::Result<()> {
        match event {
            Event::SetupHostToDevice(req) => {
                let ctrl = req.ctrl_req().clone();
                if !super::is_dfu_request(&ctrl) {
                    log::debug!("ignoring non-DFU OUT request {:#04x}", ctrl.request);
                    return Ok(());
                }
                let data = req.recv_all_async().await?;
                self.handle_out(&ctrl, &data).await
            }
            Event::SetupDeviceToHost(req) => {
                let ctrl = req.ctrl_req().clone();
                if !super::is_dfu_request(&ctrl) {
                    log::debug!("ignoring non-DFU IN request {:#04x}", ctrl.request);
                    return Ok(());
                }
                let response = self.handle_in(&ctrl).await?;
                req.send_async(response.as_slice()).await?;
                Ok(())
            }
            Event::Enable => {
                log::info!("DFU function enabled");
                Ok(())
            }
            Event::Disable => {
                log::info!("DFU function disabled");
                Ok(())
            }
            other => {
                log::debug!("event: {other:?}");
                Ok(())
            }
        }
    }
}
