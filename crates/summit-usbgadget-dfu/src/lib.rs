//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

//! USB Device Firmware Upgrade (DFU 1.1) protocol implementation.

use bytes::Bytes;
use serde::Deserialize;
use summit_usbgadget_usb::registry::{FunctionBuildContext, GadgetService, RegisteredFunctionConfig};
use summit_usbgadget_swupdate::sysinfo::SystemInfo;
use summit_usbgadget_swupdate::SwupdateConfig;

mod functionfs;
mod handler;

pub mod config;
pub mod gadget;
pub mod protocol;

pub use config::{DfuConfig, DfuConfigError, UploadSource};
pub use gadget::{build_dfu, serve, DfuRuntime};
pub use handler::{is_dfu_request, Dfu};
pub use protocol::{request, GetStatus, State, Status};

/// DFU function configuration as it appears in the main TOML file.
#[derive(Debug, Clone, Deserialize)]
pub struct DfuFnConfig {
	#[serde(flatten)]
	pub swupdate: SwupdateConfig,
	/// What `DFU_UPLOAD` serves: `"sysinfo"` to report the
	/// device's system information.
	pub upload: Option<String>,
	/// Maximum bytes per DFU control-write transaction.
	pub transfer_size: Option<u16>,
	/// `bwPollTimeout` reported in `DFU_GETSTATUS`, in milliseconds.
	pub poll_timeout_ms: Option<u32>,
}

impl DfuFnConfig {
	/// Converts the parsed configuration into the runtime [`DfuConfig`].
	pub fn to_config(&self, serial: &str) -> Result<DfuConfig, DfuConfigError> {
		let transfer_size = self.transfer_size.unwrap_or(config::DEFAULT_TRANSFER_SIZE);
		let poll_timeout_ms = self.poll_timeout_ms.unwrap_or(10);

		let upload = match self.upload.as_deref() {
			None => None,
			Some("sysinfo") => {
				let info = SystemInfo::collect(Some(serial.to_string()));
				Some(UploadSource::Data(Bytes::from(info.to_bytes())))
			}
			Some(other) => return Err(DfuConfigError::UnsupportedUploadTarget(other.to_string())),
		};

		let download = self.swupdate.to_params()?;

		Ok(DfuConfig { download, upload, transfer_size, poll_timeout_ms })
	}
}

impl RegisteredFunctionConfig for DfuFnConfig {
	fn kind(&self) -> &'static str {
		"dfu"
	}

	fn build(&self, serial: &str, context: &mut FunctionBuildContext) -> Result<Option<usb_gadget::function::Handle>, Box<dyn std::error::Error>> {
		context.claim_singleton(RegisteredFunctionConfig::kind(self))?;

		let (handle, runtime) = match gadget::build(self, serial) {
			Ok(result) => result,
			Err(err) => {
				log::error!("ignoring unsupported DFU function section: {err}");
				return Ok(None);
			}
		};

		context.push_service(DfuService(runtime));
		Ok(Some(handle))
	}

	fn clone_box(&self) -> Box<dyn RegisteredFunctionConfig> {
		Box::new(self.clone())
	}

	fn as_any(&self) -> &dyn std::any::Any {
		self
	}
}

struct DfuService(DfuRuntime);

impl GadgetService for DfuService {
	fn spawn(self: Box<Self>, udc_name: String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
		let runtime = self.0;
		Box::pin(async move { serve(udc_name, runtime).await })
	}
}

summit_usbgadget_usb::declare_usb_function!("dfu" => DfuFnConfig);
