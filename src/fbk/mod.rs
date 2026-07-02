//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! Minimal FBK-compatible custom function.
//!
//! This module implements a narrow clean-room subset required for host-to-device
//! file upload (`FBK:UCP local t:remote`) and streams the uploaded bytes into
//! the same download session used by DFU.
//! It also accepts a minimal fastboot-compatible subset for host tools:
//! `getvar`, `fetch`, `download:%08x`, and `flash:update|swu`.
//!
//! The module is split into:
//! * [`protocol`] — reply tokens, command parsers, and reply-send helpers.
//! * [`recv_queue`] — the bounded bulk-OUT receive queue.
//! * [`state`] — the [`FbkState`] command/data state machine and its loop.

mod protocol;
mod recv_queue;
mod state;

use usb_gadget::function::custom::{
    Custom, Endpoint, EndpointDirection, EndpointReceiver, EndpointSender, Interface, OsExtCompat,
};
use usb_gadget::function::Handle;
use usb_gadget::Class;

use crate::config::{ConfigError, DfuFnConfig};
use crate::swupdate::SwupdateParams;
use crate::swupdate::SwupdateSink;

use state::FbkState;
/// Fixed capacity used for every receive buffer for the entire lifetime of a
/// bound FBK function. Command and firmware-data bytes share this size; it is
/// never changed mid-session. Switching buffer size on the fly requires
/// cancelling in-flight buffers, which silently discards any data the host
/// already streamed into them before we could fetch it (the kernel AIO
/// completion queue is drained and dropped) — causing byte-stream corruption
/// / desync well after the "quiescent" point we thought we were resizing at.
/// A fixed size sized for throughput avoids that hazard entirely.
const RECV_BUFFER_SIZE: usize = 128 * 1024;
/// Number of receive buffers kept simultaneously submitted to the kernel AIO
/// queue.
///
/// FBK is a plain byte stream with no per-buffer sequence numbers. FunctionFS
/// AIO completions are consumed in completion order, so keeping multiple bulk
/// OUT reads in flight risks observing later stream bytes before earlier ones
/// if the kernel completes requests out of submission order. Keep the queue at
/// depth 1 so every completion is consumed strictly in stream order.
const RECV_QUEUE_DEPTH: usize = 1;

/// Runtime pieces for the FBK custom function.
#[derive(Debug)]
pub struct FbkRuntime {
    custom: Custom,
    rx: EndpointReceiver,
    tx: EndpointSender,
    download: SwupdateParams,
    serial: String,
}

/// Build the FBK custom function and return its gadget handle plus runtime.
pub fn build(cfg: &DfuFnConfig, serial: &str) -> Result<(Handle, FbkRuntime), ConfigError> {
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

/// Serve the FBK command/data loop for one bound gadget.
pub async fn serve(udc_name: String, mut runtime: FbkRuntime) {
    log::info!("[{udc_name}] servicing FBK upload function");

    let mut state = FbkState::new(
        runtime.rx,
        runtime.tx,
        SwupdateSink::new(runtime.download),
        runtime.serial,
    );

    state.run(&udc_name, &mut runtime.custom).await;
}
