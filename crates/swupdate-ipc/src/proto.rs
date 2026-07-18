//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
// Copyright (C) 2026 Ezurio LLC.
//
//! Wire protocol types and constants for the SWUpdate IPC interface.
//!
//! These mirror the C definitions in `network_ipc.h`, `progress_ipc.h`, and
//! `swupdate_status.h` from SWUpdate. Every struct uses `#[repr(C)]` so that the
//! in-memory layout is byte-for-byte identical to the structures the SWUpdate
//! daemon reads and writes over the Unix domain sockets.

use std::ffi::c_char;
use std::mem::{MaybeUninit, size_of};
use std::slice;

/// Magic number stamped on every control-socket frame (`IPC_MAGIC`).
pub const IPC_MAGIC: i32 = 0x1405_2001;

/// Version of the install request structure understood by this client.
pub const SWUPDATE_API_VERSION: u32 = 0x1;

// Progress API versioning (see `progress_ipc.h`).
/// Major version of the progress protocol implemented here.
pub const PROGRESS_API_MAJOR: u32 = 2;
/// Minor version of the progress protocol implemented here.
pub const PROGRESS_API_MINOR: u32 = 0;
/// Patch version of the progress protocol implemented here.
pub const PROGRESS_API_PATCH: u32 = 0;
/// Combined progress API version word sent in every progress frame.
pub const PROGRESS_API_VERSION: u32 =
    ((PROGRESS_API_MAJOR & 0xFFFF) << 16) | ((PROGRESS_API_MINOR & 0xFF) << 8) | (PROGRESS_API_PATCH & 0xFF);

/// Magic string carried in the progress connect acknowledgement.
pub const PROGRESS_CONNECT_ACK_MAGIC: &[u8] = b"ACK";

/// Message types exchanged on the control socket (`msgtype`).
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgType {
    /// Request to install an image.
    ReqInstall = 0,
    /// Positive acknowledgement.
    Ack = 1,
    /// Negative acknowledgement.
    Nack = 2,
    /// Query the current installer status.
    GetStatus = 3,
    /// Run a post-update action.
    PostUpdate = 4,
    /// Forward a command to a SWUpdate subprocess (for example, suricatta).
    SwupdateSubprocess = 5,
    /// Set the AES decryption key.
    SetAesKey = 6,
    /// Set the bootloader update state.
    SetUpdateState = 7,
    /// Get the bootloader update state.
    GetUpdateState = 8,
    /// Extended install request.
    ReqInstallExt = 9,
    /// Set the accepted version range.
    SetVersionsRange = 10,
    /// Open a notification stream.
    NotifyStream = 11,
    /// Get the hardware revision.
    GetHwRevision = 12,
    /// Set SWUpdate variables.
    SetSwupdateVars = 13,
    /// Get SWUpdate variables.
    GetSwupdateVars = 14,
}

/// Commands forwarded to a SWUpdate subprocess (the `cmd` field of `ProcMsg`).
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubprocessCmd {
    /// Query whether software can be activated.
    Activation = 0,
    /// Configure the subprocess.
    Config = 1,
    /// Enable or disable suricatta mode.
    Enable = 2,
    /// Query subprocess status.
    GetStatus = 3,
    /// Set the download URL.
    SetDownloadUrl = 4,
}

/// Dry-run selector used by an install request (`enum run_type`).
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunType {
    /// Use the daemon's command-line default.
    Default = 0,
    /// Validate the image without writing it.
    DryRun = 1,
    /// Perform a real installation.
    Install = 2,
}

/// Installer status reported by the daemon (`RECOVERY_STATUS`).
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStatus {
    /// No update running.
    Idle = 0,
    /// Update has started.
    Start = 1,
    /// Update is running.
    Run = 2,
    /// Update finished successfully.
    Success = 3,
    /// Update failed.
    Failure = 4,
    /// Image is being downloaded.
    Download = 5,
    /// Update is done (post-processing).
    Done = 6,
    /// A subprocess produced the status.
    Subprocess = 7,
    /// Progress notification.
    Progress = 8,
}

impl RecoveryStatus {
    /// Returns `true` once the installer has reached a terminal state.
    pub fn is_terminal(self) -> bool {
        matches!(self, RecoveryStatus::Success | RecoveryStatus::Failure)
    }
}

impl TryFrom<i32> for RecoveryStatus {
    type Error = i32;
    fn try_from(value: i32) -> std::result::Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Idle),
            1 => Ok(Self::Start),
            2 => Ok(Self::Run),
            3 => Ok(Self::Success),
            4 => Ok(Self::Failure),
            5 => Ok(Self::Download),
            6 => Ok(Self::Done),
            7 => Ok(Self::Subprocess),
            8 => Ok(Self::Progress),
            other => Err(other),
        }
    }
}

/// Interface that triggered an update (`sourcetype`).
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceType {
    /// Unknown source.
    Unknown = 0,
    /// Integrated web server.
    Webserver = 1,
    /// Suricatta (hawkBit) client.
    Suricatta = 2,
    /// Generic downloader.
    Downloader = 3,
    /// Local installation.
    Local = 4,
    /// Chunked downloader.
    ChunksDownloader = 5,
}

impl TryFrom<i32> for SourceType {
    type Error = i32;
    fn try_from(value: i32) -> std::result::Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Unknown),
            1 => Ok(Self::Webserver),
            2 => Ok(Self::Suricatta),
            3 => Ok(Self::Downloader),
            4 => Ok(Self::Local),
            5 => Ok(Self::ChunksDownloader),
            other => Err(other),
        }
    }
}

/// Install request structure (`struct swupdate_request`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SwupdateRequest {
    /// API version; must equal [`SWUPDATE_API_VERSION`].
    pub apiversion: u32,
    /// Source interface (`sourcetype`).
    pub source: i32,
    /// Dry-run selector (`enum run_type`).
    pub dry_run: i32,
    /// Length of the data in `info`.
    pub len: usize,
    /// Free-form information forwarded to the progress interface.
    pub info: [c_char; 512],
    /// Selected software set.
    pub software_set: [c_char; 256],
    /// Selected running mode.
    pub running_mode: [c_char; 256],
    /// When `true`, the daemon must not persist the received SWU.
    pub disable_store_swu: bool,
}

impl SwupdateRequest {
    /// Builds a request pre-filled with default values, equivalent to the C
    /// `swupdate_prepare_req()` helper.
    pub fn prepare() -> Self {
        let mut req: Self = unsafe { MaybeUninit::zeroed().assume_init() };
        req.apiversion = SWUPDATE_API_VERSION;
        req.dry_run = RunType::Default as i32;
        req
    }

    /// Sets the `software_set` selection (truncated to fit, NUL-terminated).
    pub fn set_software_set(&mut self, value: &str) {
        write_c_string(&mut self.software_set, value);
    }

    /// Sets the `running_mode` selection (truncated to fit, NUL-terminated).
    pub fn set_running_mode(&mut self, value: &str) {
        write_c_string(&mut self.running_mode, value);
    }
}

/// Status payload of an `ipc_message` (`status` union member).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StatusMsg {
    /// Current installer status (`RECOVERY_STATUS`).
    pub current: i32,
    /// Result of the last completed installation.
    pub last_result: i32,
    /// Error code, if any.
    pub error: i32,
    /// Human-readable description.
    pub desc: [c_char; 2048],
}

/// Notification payload of an `ipc_message` (`notify` union member).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NotifyMsg {
    /// Status code.
    pub status: i32,
    /// Error code.
    pub error: i32,
    /// Log level.
    pub level: i32,
    /// Message text.
    pub msg: [c_char; 2048],
}

/// Install payload of an `ipc_message` (`instmsg` union member).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InstMsg {
    /// The install request.
    pub req: SwupdateRequest,
    /// Length of data valid in `buf`.
    pub len: u32,
    /// Source-specific extra data.
    pub buf: [c_char; 2048],
}

/// Subprocess command payload of an `ipc_message` (`procmsg` union member).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ProcMsg {
    /// Source that triggered the update (`sourcetype`).
    pub source: i32,
    /// Optional encoded command.
    pub cmd: i32,
    /// Timeout in seconds if an answer is expected.
    pub timeout: i32,
    /// Length of data valid in `buf`.
    pub len: u32,
    /// Source-specific extra data.
    pub buf: [c_char; 2048],
}

/// AES key payload of an `ipc_message` (`aeskeymsg` union member).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AesKeyMsg {
    /// 256-bit key as a 64-character ASCII string plus NUL.
    pub key_ascii: [c_char; 65],
    /// IV as a 32-character ASCII string plus NUL.
    pub ivt_ascii: [c_char; 33],
}

/// Version-range payload of an `ipc_message` (`versions` union member).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VersionsMsg {
    /// Minimum accepted version.
    pub minimum_version: [c_char; 256],
    /// Maximum accepted version.
    pub maximum_version: [c_char; 256],
    /// Current version.
    pub current_version: [c_char; 256],
}

/// Hardware revision payload of an `ipc_message` (`revisions` union member).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RevisionsMsg {
    /// Board name.
    pub boardname: [c_char; 256],
    /// Board revision.
    pub revision: [c_char; 256],
}

/// SWUpdate variable payload of an `ipc_message` (`vars` union member).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VarsMsg {
    /// Variable namespace.
    pub varnamespace: [c_char; 256],
    /// Variable name.
    pub varname: [c_char; 256],
    /// Variable value.
    pub varvalue: [c_char; 256],
}

/// Union of all `ipc_message` payloads (`msgdata`).
#[repr(C)]
#[derive(Clone, Copy)]
pub union MsgData {
    /// Raw message bytes.
    pub msg: [c_char; 128],
    /// Status payload.
    pub status: StatusMsg,
    /// Notification payload.
    pub notify: NotifyMsg,
    /// Install payload.
    pub instmsg: InstMsg,
    /// Subprocess command payload.
    pub procmsg: ProcMsg,
    /// AES key payload.
    pub aeskeymsg: AesKeyMsg,
    /// Version-range payload.
    pub versions: VersionsMsg,
    /// Hardware revision payload.
    pub revisions: RevisionsMsg,
    /// SWUpdate variable payload.
    pub vars: VarsMsg,
}

/// Control-socket frame (`ipc_message`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IpcMessage {
    /// Magic number; must equal [`IPC_MAGIC`].
    pub magic: i32,
    /// Message type (`MsgType`).
    pub type_: i32,
    /// Type-dependent payload.
    pub data: MsgData,
}

impl IpcMessage {
    /// Returns an all-zero frame.
    pub fn zeroed() -> Self {
        unsafe { MaybeUninit::zeroed().assume_init() }
    }

    /// Returns a zeroed frame stamped with [`IPC_MAGIC`] and the given type.
    pub fn new(type_: MsgType) -> Self {
        let mut msg = Self::zeroed();
        msg.magic = IPC_MAGIC;
        msg.type_ = type_ as i32;
        msg
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

/// Progress notification frame (`struct progress_msg`).
///
/// SWUpdate declares this struct `__attribute__((packed))` in
/// `progress_ipc.h`, so it must be `#[repr(C, packed)]` here too. Without
/// `packed`, Rust inserts 4 bytes of padding before `dwl_bytes` (a `u64`
/// needs 8-byte alignment after two `u32`s), shifting every field from
/// `nsteps` onward by 4 bytes relative to what the daemon actually sends on
/// the wire — silently corrupting `cur_step`, `cur_percent`, `cur_image`,
/// `hnd_name`, `source`, `infolen`, and `info`, and hanging completion
/// detection that depends on them. Do not drop `packed` when touching this
/// struct.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct ProgressMsg {
    /// Progress API version for compatibility checking.
    pub apiversion: u32,
    /// Update status (`RECOVERY_STATUS`).
    pub status: i32,
    /// Percentage of downloaded data.
    pub dwl_percent: u32,
    /// Total number of bytes to download.
    pub dwl_bytes: u64,
    /// Total number of installer steps.
    pub nsteps: u32,
    /// Current step index (1-based).
    pub cur_step: u32,
    /// Percentage complete within the current step.
    pub cur_percent: u32,
    /// Name of the image being installed.
    pub cur_image: [c_char; 256],
    /// Name of the running handler.
    pub hnd_name: [c_char; 64],
    /// Interface that triggered the update (`sourcetype`).
    pub source: i32,
    /// Length of valid data in `info`.
    pub infolen: u32,
    /// Additional install information.
    pub info: [c_char; 2048],
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
    pub fn status(&self) -> std::result::Result<RecoveryStatus, i32> {
        RecoveryStatus::try_from(self.status)
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
        let len = (self.infolen as usize).min(self.info.len());
        let bytes: Vec<u8> = self.info[..len].iter().map(|&c| c as u8).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Overall installation progress as a percentage (0–100).
    ///
    /// Accounts for multi-step installs by combining completed steps with
    /// the within-step percentage. Returns 100 on success and caps at 99
    /// while still running.
    pub fn overall_percent(&self) -> u32 {
        if self.status == RecoveryStatus::Success as i32 {
            return 100;
        }
        if self.nsteps > 0 && self.cur_step > 0 {
            let completed = self.cur_step.saturating_sub(1).min(self.nsteps);
            let total = completed
                .saturating_mul(100)
                .saturating_add(self.cur_percent.min(100));
            return (total / self.nsteps).min(99);
        }
        self.cur_percent.max(self.dwl_percent).min(99)
    }
}

/// Progress connect acknowledgement (`struct progress_connect_ack`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ProgressConnectAck {
    /// Progress API version reported by the daemon.
    pub apiversion: u32,
    /// NUL-terminated magic string; must be [`PROGRESS_CONNECT_ACK_MAGIC`].
    pub magic: [c_char; 4],
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

/// Copies `value` into a fixed-size C string buffer, truncating and always
/// NUL-terminating.
pub fn write_c_string(dst: &mut [c_char], value: &str) {
    dst.fill(0);
    if dst.is_empty() {
        return;
    }
    let max = dst.len() - 1;
    for (slot, &byte) in dst.iter_mut().zip(value.as_bytes().iter()).take(max) {
        *slot = byte as c_char;
    }
}

/// Reads a NUL-terminated C string buffer into an owned `String` (lossy UTF-8).
pub fn read_c_string(src: &[c_char]) -> String {
    let bytes: Vec<u8> = src.iter().map(|&c| c as u8).collect();
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

// Compile-time layout guarantees. Sizes that depend on `size_t` are checked
// only on 64-bit targets; all sockets carry whatever the local ABI produces.
// ProgressMsg is 2408 bytes (packed) for SWUpdate >= 2025.12; it was 2416
// bytes (unpacked) on SWUpdate <= 2025.05. See the doc comment on
// `ProgressMsg` before changing this.
const _: () = assert!(size_of::<ProgressMsg>() == 2408);
const _: () = assert!(size_of::<ProgressConnectAck>() == 8);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<SwupdateRequest>() == 1056);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<MsgData>() == 3112);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<IpcMessage>() == 3120);
