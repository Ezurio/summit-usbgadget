//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
// Copyright (C) 2026 Ezurio LLC.
//
//! Resolution of the SWUpdate Unix domain socket paths.
//!
//! Two layers of configuration (highest priority first):
//!
//! 1. **Build-time config** — set `CONFIG_SOCKET_CTRL_PATH` /
//!    `CONFIG_SOCKET_PROGRESS_PATH` in the build environment (Kconfig
//!    equivalent); baked in at compile time via `option_env!`.
//! 2. **Directory-based fallback** — directory from `$RUNTIME_DIRECTORY`,
//!    `$TMPDIR`, or `/tmp`; default filename appended.

use std::ffi::OsString;
use std::path::PathBuf;

/// Default file name of the control socket (`sockinstctrl`).
pub const SOCKET_CTRL_DEFAULT: &str = "sockinstctrl";
/// Default file name of the progress socket (`swupdateprog`).
pub const SOCKET_PROGRESS_DEFAULT: &str = "swupdateprog";

fn socket_dir() -> OsString {
    if let Some(dir) = std::env::var_os("RUNTIME_DIRECTORY") {
        if !dir.is_empty() {
            return dir;
        }
    }
    if let Some(dir) = std::env::var_os("TMPDIR") {
        if !dir.is_empty() {
            return dir;
        }
    }
    OsString::from("/tmp")
}

/// Resolves the control socket path. Equivalent to the C `get_ctrl_socket()`.
pub fn ctrl_socket_path() -> PathBuf {
    if let Some(path) = option_env!("CONFIG_SOCKET_CTRL_PATH") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    PathBuf::from(socket_dir()).join(SOCKET_CTRL_DEFAULT)
}

/// Resolves the progress socket path. Equivalent to the C `get_prog_socket()`.
pub fn progress_socket_path() -> PathBuf {
    if let Some(path) = option_env!("CONFIG_SOCKET_PROGRESS_PATH") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    PathBuf::from(socket_dir()).join(SOCKET_PROGRESS_DEFAULT)
}
