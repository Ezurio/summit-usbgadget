//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
// Copyright (C) 2026 Ezurio LLC.
//
//! Pure-Rust client for the SWUpdate IPC and progress protocols.
//!
//! This crate reimplements the SWUpdate client library (the C `network_ipc`,
//! `network_ipc-if`, and `progress_ipc` units) directly in Rust. It speaks the
//! Unix domain socket protocol natively, so no `libswupdate.so` is required.
//!
//! Two client surfaces are provided:
//!
//! * [`blocking`] — synchronous API built on `std::os::unix::net`, always
//!   available.
//! * [`r#async`] — asynchronous API built on `tokio`, gated behind the `async`
//!   feature.
//!
//! Both expose the same protocol: starting an install, streaming the image,
//! querying status, post-update actions, subprocess commands, AES key and
//! version-range configuration, the notification stream, and the progress
//! interface.
//!
//! # Feature flags
//!
//! - `async` — enables the [`r#async`] API built on `tokio`.
//! - No feature flags are required for the [`blocking`] API.
//!
//! # Socket path resolution
//!
//! Control and progress socket paths are resolved in this order:
//!
//! 1. Compile-time config (`CONFIG_SOCKET_CTRL_PATH`,
//!    `CONFIG_SOCKET_PROGRESS_PATH`)
//! 2. Runtime directory (`$RUNTIME_DIRECTORY`, then `$TMPDIR`, then `/tmp`)
//!
//! See [`ctrl_socket_path`] and [`progress_socket_path`].
//!
//! # Examples
//!
//! ## Software update (blocking)
//!
//! ```no_run
//! use std::fs::File;
//! use std::io::Read;
//! use std::time::Duration;
//!
//! use swupdate_ipc::blocking;
//! use swupdate_ipc::{RunType, SourceType, SwupdateRequest};
//!
//! fn software_update(path: &str) -> Result<(), Box<dyn std::error::Error>> {
//!     let mut req = SwupdateRequest::prepare();
//!     req.source = SourceType::Local as i32;
//!     req.dry_run = RunType::Install as i32;
//!
//!     let mut conn = blocking::inst_start_ext(&req)?;
//!     let mut file = File::open(path)?;
//!     let mut buf = [0u8; 64 * 1024];
//!
//!     loop {
//!         let n = file.read(&mut buf)?;
//!         if n == 0 {
//!             break;
//!         }
//!         conn.send_data(&buf[..n])?;
//!     }
//!     conn.end();
//!
//!     blocking::await_install_result(Duration::from_secs(120))?;
//!     Ok(())
//! }
//! ```
//!
//! ## Progress updates (blocking)
//!
//! ```no_run
//! use swupdate_ipc::blocking;
//! use swupdate_ipc::RecoveryStatus;
//!
//! fn watch_progress() -> Result<(), Box<dyn std::error::Error>> {
//!     let mut progress = blocking::progress_connect(true)?;
//!     loop {
//!         let msg = progress.receive()?;
//!         let percent = msg.overall_percent();
//!         println!("progress: {percent}% image={}", msg.cur_image());
//!
//!         match msg.status() {
//!             Ok(RecoveryStatus::Success) => return Ok(()),
//!             Ok(RecoveryStatus::Failure) => {
//!                 return Err("SWUpdate reported failure".into());
//!             }
//!             _ => {}
//!         }
//!     }
//!     Ok(())
//! }
//! ```
//!
//! ## Software update (async)
//!
//! ```no_run
//! use tokio::io::AsyncReadExt;
//! use std::time::Duration;
//!
//! use swupdate_ipc::r#async as swu;
//! use swupdate_ipc::{RunType, SourceType, SwupdateRequest};
//!
//! async fn software_update_async(path: &str) -> Result<(), Box<dyn std::error::Error>> {
//!     let mut req = SwupdateRequest::prepare();
//!     req.source = SourceType::Local as i32;
//!     req.dry_run = RunType::Install as i32;
//!
//!     let mut conn = swu::inst_start_ext(&req).await?;
//!     let mut file = tokio::fs::File::open(path).await?;
//!     let mut buf = [0u8; 64 * 1024];
//!
//!     loop {
//!         let n = file.read(&mut buf).await?;
//!         if n == 0 {
//!             break;
//!         }
//!         conn.send_data(&buf[..n]).await?;
//!     }
//!     conn.end().await?;
//!
//!     swu::await_install_result(Duration::from_secs(120)).await?;
//!     Ok(())
//! }
//! ```
//!
//! ## Progress updates (async)
//!
//! ```no_run
//! use swupdate_ipc::r#async as swu;
//! use swupdate_ipc::RecoveryStatus;
//!
//! async fn watch_progress_async() -> Result<(), Box<dyn std::error::Error>> {
//!     let mut progress = swu::progress_connect(true).await?;
//!     loop {
//!         let msg = progress.receive().await?;
//!         let percent = msg.overall_percent();
//!         println!("progress: {percent}% image={}", msg.cur_image());
//!
//!         match msg.status() {
//!             Ok(RecoveryStatus::Success) => return Ok(()),
//!             Ok(RecoveryStatus::Failure) => {
//!                 return Err("SWUpdate reported failure".into());
//!             }
//!             _ => {}
//!         }
//!     }
//! }
//! ```

mod error;
pub mod proto;
mod socket;

pub mod blocking;

#[cfg(feature = "async")]
#[path = "async_io.rs"]
pub mod r#async;

pub use error::{Error, Result};
pub use proto::{
    IPC_MAGIC, IpcMessage, MsgData, MsgType, PROGRESS_API_VERSION, ProgressConnectAck, ProgressMsg,
    RecoveryStatus, RunType, SourceType, SubprocessCmd, SwupdateRequest,
};
pub use socket::{ctrl_socket_path, progress_socket_path, SOCKET_CTRL_DEFAULT, SOCKET_PROGRESS_DEFAULT};
