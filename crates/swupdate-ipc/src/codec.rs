//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
// Copyright (C) 2026 Ezurio LLC.
//
//! Native Rust models and conversions for the SWUpdate IPC wire protocol.

use crate::error::Result;
use crate::proto::{IpcMessage, MsgType, RecoveryStatus, RunType, SourceType, SwupdateRequest};

/// Source that initiated an install.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InstallSource {
    #[default]
    Unknown,
    Webserver,
    Suricatta,
    Downloader,
    Local,
    ChunksDownloader,
}

impl From<InstallSource> for SourceType {
    fn from(source: InstallSource) -> Self {
        match source {
            InstallSource::Unknown => Self::SOURCE_UNKNOWN,
            InstallSource::Webserver => Self::SOURCE_WEBSERVER,
            InstallSource::Suricatta => Self::SOURCE_SURICATTA,
            InstallSource::Downloader => Self::SOURCE_DOWNLOADER,
            InstallSource::Local => Self::SOURCE_LOCAL,
            InstallSource::ChunksDownloader => Self::SOURCE_CHUNKS_DOWNLOADER,
        }
    }
}

/// Mode used to run an install.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InstallMode {
    #[default]
    Default,
    DryRun,
    Install,
}

impl From<InstallMode> for RunType {
    fn from(mode: InstallMode) -> Self {
        match mode {
            InstallMode::Default => Self::RUN_DEFAULT,
            InstallMode::DryRun => Self::RUN_DRYRUN,
            InstallMode::Install => Self::RUN_INSTALL,
        }
    }
}

/// Rust-native install parameters, encoded by the IPC client before sending.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstallRequest {
    pub software_set: String,
    pub running_mode: String,
    pub source: InstallSource,
    pub mode: InstallMode,
    pub disable_store_swu: bool,
}

impl InstallRequest {
    /// Encodes this request as a raw SWUpdate IPC frame.
    pub fn encode(&self) -> Result<IpcMessage> {
        let request = SwupdateRequest::try_from(self)?;
        let mut message = IpcMessage::new(MsgType::REQ_INSTALL);
        message.set_install_request(request);
        Ok(message)
    }
}

impl TryFrom<&InstallRequest> for SwupdateRequest {
    type Error = crate::Error;

    fn try_from(request: &InstallRequest) -> Result<Self> {
        let mut raw = Self::prepare();
        raw.source = request.source.into();
        raw.dry_run = request.mode.into();
        raw.disable_store_swu = request.disable_store_swu;
        raw.set_software_set(&request.software_set)?;
        raw.set_running_mode(&request.running_mode)?;
        Ok(raw)
    }
}

/// Decoded control-socket installer status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallStatus {
    pub current: Option<RecoveryStatus>,
    pub last_result: Option<RecoveryStatus>,
    pub description: String,
}

impl InstallStatus {
    /// Decodes a status reply received from the control socket.
    pub fn decode(message: &IpcMessage) -> Self {
        let (current, last_result) = message.install_statuses();
        let (_, description) = message.install_status_description();
        Self {
            current,
            last_result,
            description,
        }
    }
}
