//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
// Copyright (C) 2026 Ezurio LLC.
//
//! Wire protocol types for the SWUpdate IPC interface.
//!
//! These are generated from SWUpdate's own C headers (`network_ipc.h`,
//! `progress_ipc.h`, `swupdate_status.h`) by bindgen (see `build.rs`). The
//! checked-in default bindings support builds without SWUpdate headers; setting
//! `SWUPDATE_INCLUDE_DIR` regenerates them for the target daemon. This avoids
//! hand-mirrored definitions, so the in-memory layout is byte-for-byte
//! identical to the supported daemon ABI
//! — including things like the `packed` attribute on `struct progress_msg`,
//! which bindgen picks up automatically from the header instead of relying
//! on a human to notice and copy it. (A hand-written `ProgressMsg` missing
//! `#[repr(C, packed)]` previously corrupted progress/status parsing this
//! way and hung completion detection.)
//!
//! # Safety note on enum-typed fields
//!
//! bindgen represents true C enums (`sourcetype`, `run_type`) as real Rust
//! enums wherever the header uses them directly. Constructing one of these
//! enums with a value outside its defined variants is undefined behavior.
//! That's fine for [`SwupdateRequest`] fields, since we always fill those
//! ourselves before sending. But [`IpcMessage`] is read wholesale off the
//! control socket via [`IpcMessage::as_bytes_mut`] as an opaque byte blob,
//! so a corrupt/unexpected reply could in principle leave an enum-typed
//! sub-field of [`MsgData`] holding an invalid discriminant. Only read
//! `data.status` (plain `c_int`s) back out of a received [`IpcMessage`];
//! don't read the enum-typed fields of `data.instmsg`/`data.procmsg`
//! (`source`, `dry_run`) from a message that came off the wire.
//! `progress_msg::status` avoids this entirely: bindgen represents it as a
//! plain `u32`, not `RECOVERY_STATUS`, specifically because that field IS
//! populated from the wire; decode it with [`decode_recovery_status`].

// Generated enum variants retain their C names to keep this crate's public
// protocol API stable (for example, `MsgType::REQ_INSTALL`).
#![allow(non_camel_case_types)]

use std::ffi::{CStr, c_char};
use std::mem::{MaybeUninit, size_of};
use std::slice;

use crate::error::{Error, Result};

include!(concat!(env!("OUT_DIR"), "/swupdate_sys.rs"));

/// Magic string carried in the progress connect acknowledgement.
pub const PROGRESS_CONNECT_ACK_MAGIC: &[u8] = b"ACK";

impl RecoveryStatus {
    /// Returns `true` once the installer has reached a terminal state.
    pub fn is_terminal(self) -> bool {
        matches!(self, RecoveryStatus::SUCCESS | RecoveryStatus::FAILURE)
    }
}

/// Decodes a raw `progress_msg::status` value. bindgen leaves that field as
/// a plain `u32` rather than `RECOVERY_STATUS` directly (see the
/// module-level safety note), since it's populated straight from the wire.
pub fn decode_recovery_status(raw: u32) -> std::result::Result<RecoveryStatus, u32> {
    match raw {
        0 => Ok(RecoveryStatus::IDLE),
        1 => Ok(RecoveryStatus::START),
        2 => Ok(RecoveryStatus::RUN),
        3 => Ok(RecoveryStatus::SUCCESS),
        4 => Ok(RecoveryStatus::FAILURE),
        5 => Ok(RecoveryStatus::DOWNLOAD),
        6 => Ok(RecoveryStatus::DONE),
        7 => Ok(RecoveryStatus::SUBPROCESS),
        8 => Ok(RecoveryStatus::PROGRESS),
        other => Err(other),
    }
}

impl SwupdateRequest {
    /// Builds a request pre-filled with default values, equivalent to the C
    /// `swupdate_prepare_req()` helper.
    pub fn prepare() -> Self {
        let mut req: Self = unsafe { MaybeUninit::zeroed().assume_init() };
        req.apiversion = SWUPDATE_API_VERSION;
        req.dry_run = RunType::RUN_DEFAULT;
        req
    }

    /// Sets the `software_set` selection.
    pub fn set_software_set(&mut self, value: &str) -> Result<()> {
        write_c_string(&mut self.software_set, value)
    }

    /// Sets the `running_mode` selection.
    pub fn set_running_mode(&mut self, value: &str) -> Result<()> {
        write_c_string(&mut self.running_mode, value)
    }
}

impl IpcMessage {
    /// Returns an all-zero frame.
    pub fn zeroed() -> Self {
        unsafe { MaybeUninit::zeroed().assume_init() }
    }

    /// Returns a zeroed frame stamped with [`IPC_MAGIC`] and the given type.
    pub fn new(type_: MsgType) -> Self {
        let mut msg = Self::zeroed();
        msg.magic = IPC_MAGIC as i32;
        msg.type_ = type_ as i32;
        msg
    }

    /// Returns whether this frame has the given IPC message type.
    pub fn has_type(&self, type_: MsgType) -> bool {
        self.type_ == type_ as i32
    }

    /// Stores an install request in the request payload.
    pub fn set_install_request(&mut self, request: SwupdateRequest) {
        self.data.instmsg.req = request;
    }

    /// Returns the current and most recent install status from a status reply.
    pub fn install_statuses(&self) -> (Option<RecoveryStatus>, Option<RecoveryStatus>) {
        let (current, last_result) =
            unsafe { (self.data.status.current, self.data.status.last_result) };
        (
            decode_recovery_status(current as u32).ok(),
            decode_recovery_status(last_result as u32).ok(),
        )
    }

    /// Returns the raw current status and optional description from a status reply.
    pub fn install_status_description(&self) -> (i32, String) {
        let (current, description) = unsafe {
            let status = &self.data.status;
            (status.current, read_c_string(&status.desc))
        };
        (current, description)
    }

    /// Stores the post-update information payload, truncating it to the C
    /// buffer size.
    pub fn set_postupdate_info(&mut self, info: &[u8]) {
        unsafe {
            let procmsg = &mut self.data.procmsg;
            let len = info.len().min(procmsg.buf.len());
            for (slot, &byte) in procmsg.buf.iter_mut().zip(info).take(len) {
                *slot = byte as c_char;
            }
            procmsg.len = len as u32;
        }
    }

    /// Stores the AES key and initialization vector payload.
    pub fn set_aes_key(&mut self, key: &str, ivt: &str) -> Result<()> {
        unsafe {
            write_c_string(&mut self.data.aeskeymsg.key_ascii, key)?;
            write_c_string(&mut self.data.aeskeymsg.ivt_ascii, ivt)?;
        }
        Ok(())
    }

    /// Stores the optional accepted version range payload.
    pub fn set_version_range(
        &mut self,
        min_version: Option<&str>,
        max_version: Option<&str>,
        current_version: Option<&str>,
    ) -> Result<()> {
        unsafe {
            let versions = &mut self.data.versions;
            if let Some(value) = min_version {
                write_c_string(&mut versions.minimum_version, value)?;
            }
            if let Some(value) = max_version {
                write_c_string(&mut versions.maximum_version, value)?;
            }
            if let Some(value) = current_version {
                write_c_string(&mut versions.current_version, value)?;
            }
        }
        Ok(())
    }

    /// View of the frame as raw bytes for transmission.
    pub fn as_bytes(&self) -> &[u8] {
        unsafe { slice::from_raw_parts((self as *const Self).cast::<u8>(), size_of::<Self>()) }
    }

    /// Mutable view of the frame as raw bytes for reception.
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut((self as *mut Self).cast::<u8>(), size_of::<Self>()) }
    }
}

impl ProgressMsg {
    /// Returns an all-zero progress frame.
    pub fn zeroed() -> Self {
        unsafe { MaybeUninit::zeroed().assume_init() }
    }

    /// Mutable byte view used when reading a frame from the socket.
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut((self as *mut Self).cast::<u8>(), size_of::<Self>()) }
    }

    /// Decoded installer status, if it is a value this client understands.
    pub fn status(&self) -> std::result::Result<RecoveryStatus, u32> {
        let raw = self.status;
        decode_recovery_status(raw)
    }

    /// Current installation step number.
    pub fn current_step(&self) -> u32 {
        self.cur_step
    }

    /// Total number of installation steps.
    pub fn total_steps(&self) -> u32 {
        self.nsteps
    }

    /// Completion percentage of the current installation step.
    pub fn current_percent(&self) -> u32 {
        self.cur_percent
    }

    /// Name of the image currently being installed.
    pub fn cur_image(&self) -> String {
        read_c_string(&self.cur_image)
    }

    /// Name of the running handler.
    pub fn hnd_name(&self) -> String {
        read_c_string(&self.hnd_name)
    }

    /// Additional install information (using `infolen`).
    pub fn info(&self) -> String {
        let infolen = self.infolen;
        let len = (infolen as usize).min(self.info.len());
        let bytes: Vec<u8> = self.info[..len].iter().map(|&c| c as u8).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Overall installation progress as a percentage (0–100).
    ///
    /// Accounts for multi-step installs by combining completed steps with
    /// the within-step percentage. Returns 100 on success and caps at 99
    /// while still running.
    pub fn overall_percent(&self) -> u32 {
        let (status, nsteps, cur_step, cur_percent, dwl_percent) = (
            self.status,
            self.nsteps,
            self.cur_step,
            self.cur_percent,
            self.dwl_percent,
        );
        if status == RecoveryStatus::SUCCESS as u32 {
            return 100;
        }
        if nsteps > 0 && cur_step > 0 {
            let completed = cur_step.saturating_sub(1).min(nsteps);
            let total = completed
                .saturating_mul(100)
                .saturating_add(cur_percent.min(100));
            return (total / nsteps).min(99);
        }
        cur_percent.max(dwl_percent).min(99)
    }
}

impl ProgressConnectAck {
    /// Returns an all-zero acknowledgement.
    pub fn zeroed() -> Self {
        unsafe { MaybeUninit::zeroed().assume_init() }
    }

    /// Mutable byte view used when reading the acknowledgement.
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut((self as *mut Self).cast::<u8>(), size_of::<Self>()) }
    }

    /// Returns `true` when the daemon's major version matches this client.
    pub fn is_major_compatible(&self) -> bool {
        ((self.apiversion >> 16) & 0xFFFF) == PROGRESS_API_MAJOR
    }

    /// Returns `true` when the magic field equals [`PROGRESS_CONNECT_ACK_MAGIC`].
    pub fn has_valid_magic(&self) -> bool {
        let bytes: Vec<u8> = self.magic.iter().map(|&c| c as u8).collect();
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        &bytes[..end] == PROGRESS_CONNECT_ACK_MAGIC
    }
}

/// Copies `value` into a fixed-size, NUL-terminated C string buffer.
///
/// Returns an error if `value` cannot fit with its NUL terminator.
pub fn write_c_string(dst: &mut [c_char], value: &str) -> Result<()> {
    let dst = c_char_bytes_mut(dst);
    if value.len() >= dst.len() {
        return Err(Error::InvalidArgument(
            "string does not fit in the fixed C buffer",
        ));
    }
    dst[..value.len()].copy_from_slice(value.as_bytes());
    dst[value.len()] = 0;
    Ok(())
}

/// Reads a NUL-terminated C string buffer into an owned `String` (lossy UTF-8).
pub fn read_c_string(src: &[c_char]) -> String {
    let bytes = c_char_bytes(src);
    match CStr::from_bytes_until_nul(bytes) {
        Ok(c_str) => c_str.to_string_lossy().into_owned(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Views a bindgen C-character buffer as bytes without copying.
fn c_char_bytes(src: &[c_char]) -> &[u8] {
    // `c_char` is always a one-byte signed or unsigned integer type.
    unsafe { slice::from_raw_parts(src.as_ptr().cast(), src.len()) }
}

/// Views a mutable bindgen C-character buffer as bytes without copying.
fn c_char_bytes_mut(dst: &mut [c_char]) -> &mut [u8] {
    // `c_char` is always a one-byte signed or unsigned integer type.
    unsafe { slice::from_raw_parts_mut(dst.as_mut_ptr().cast(), dst.len()) }
}
