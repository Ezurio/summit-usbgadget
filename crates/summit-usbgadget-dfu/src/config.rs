//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! Runtime configuration and serde schema for the DFU function.

use std::error::Error;
use std::fmt;

use bytes::Bytes;
use summit_usbgadget_swupdate::{SwupdateConfigError, SwupdateParams};
use usb_gadget::function::custom::DfuDesc;

#[derive(Debug)]
pub enum DfuConfigError {
    Swupdate(SwupdateConfigError),
    UnsupportedUploadTarget(String),
}

impl fmt::Display for DfuConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Swupdate(err) => err.fmt(f),
            Self::UnsupportedUploadTarget(target) => {
                write!(f, "unsupported insecure upload target {target:?}; only \"sysinfo\" is allowed")
            }
        }
    }
}

impl Error for DfuConfigError {}

impl From<SwupdateConfigError> for DfuConfigError {
    fn from(err: SwupdateConfigError) -> Self {
        Self::Swupdate(err)
    }
}

/// Source served to the host in response to `DFU_UPLOAD`.
#[derive(Debug, Clone)]
pub enum UploadSource {
    /// Serve an in-memory blob (e.g. a system-information report).
    Data(Bytes),
}

/// Runtime configuration for the DFU function.
#[derive(Debug, Clone)]
pub struct DfuConfig {
    /// Destination for received firmware.
    pub(crate) download: SwupdateParams,
    /// Optional source that firmware uploads are served from.
    pub upload: Option<UploadSource>,
    /// Maximum number of bytes transferred per control-write transaction.
    pub transfer_size: u16,
    /// Value reported to the host in `bwPollTimeout` of `DFU_GETSTATUS`.
    pub poll_timeout_ms: u32,
}

impl DfuConfig {
    /// Builds the DFU functional descriptor that advertises this
    /// configuration's capabilities to the host.
    pub fn descriptor(&self) -> DfuDesc {
        DfuDesc {
            can_download: true,
            can_upload: self.upload.is_some(),
            manifest_tolerant: true,
            will_detach: false,
            detach_timeout_ms: 1000,
            transfer_size: self.transfer_size,
            dfu_version: (1, 1),
        }
    }
}
