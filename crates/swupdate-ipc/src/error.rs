//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
// Copyright (C) 2026 Ezurio LLC.
//
//! Error type for the SWUpdate IPC client.

/// Errors returned by the SWUpdate IPC and progress client APIs.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Underlying socket I/O failure.
    #[error("SWUpdate IPC I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// SWUpdate rejected the request with a `NACK` (for example, an update is
    /// already in progress).
    #[error("SWUpdate rejected the request (NACK)")]
    Nack,

    /// SWUpdate replied with an unexpected message type.
    #[error("unexpected SWUpdate message type: {0}")]
    UnexpectedType(i32),

    /// A received frame carried an invalid magic number.
    #[error("invalid SWUpdate IPC magic number: {0:#010x}")]
    InvalidMagic(i32),

    /// The progress daemon advertised an incompatible major API version.
    #[error("incompatible SWUpdate progress API version: {0:#010x}")]
    IncompatibleProgressVersion(u32),

    /// The progress connect acknowledgement was malformed.
    #[error("invalid SWUpdate progress connect acknowledgement")]
    InvalidProgressAck,

    /// The peer closed the connection before the exchange completed.
    #[error("SWUpdate closed the connection")]
    Closed,

    /// A caller-supplied argument was invalid.
    #[error("invalid argument: {0}")]
    InvalidArgument(&'static str),

    /// Timed out waiting for SWUpdate to report an install result.
    #[error("timed out waiting for SWUpdate install result")]
    Timeout,

    /// Timed out waiting to establish a progress connection.
    #[error("timed out connecting to SWUpdate progress socket")]
    ProgressConnectTimeout,

    /// SWUpdate reported that the installation failed.
    #[error("SWUpdate reported installation failure")]
    InstallFailed,
}

/// Convenience result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;
