//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! DFU wire protocol definitions: request codes and the state/status
//! enumerations (DFU 1.1, section 6.1.2).

/// DFU class-specific request codes (DFU 1.1, section 3).
pub mod request {
    pub const DETACH: u8 = 0;
    pub const DNLOAD: u8 = 1;
    pub const UPLOAD: u8 = 2;
    pub const GETSTATUS: u8 = 3;
    pub const CLRSTATUS: u8 = 4;
    pub const GETSTATE: u8 = 5;
    pub const ABORT: u8 = 6;
}

/// DFU device states (DFU 1.1, section 6.1.2). Discriminants are the wire
/// values reported to the host in `DFU_GETSTATUS`/`DFU_GETSTATE`.
///
/// The full set defined by the specification is retained so that callers and
/// future state-machine extensions can refer to any state by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    /// Run-time mode, idle.
    AppIdle = 0,
    /// Run-time mode, a `DFU_DETACH` was received.
    AppDetach = 1,
    /// DFU mode, idle.
    DfuIdle = 2,
    /// Download in progress, awaiting `DFU_GETSTATUS`.
    DnloadSync = 3,
    /// Download in progress, device is busy.
    DnBusy = 4,
    /// Download phase, idle between blocks.
    DnloadIdle = 5,
    /// Manifestation in progress, awaiting `DFU_GETSTATUS`.
    ManifestSync = 6,
    /// Manifestation in progress.
    Manifest = 7,
    /// Manifestation complete, awaiting a USB reset.
    ManifestWaitReset = 8,
    /// Upload phase, idle between blocks.
    UploadIdle = 9,
    /// An error has occurred; `DFU_CLRSTATUS` is required to continue.
    Error = 10,
}

/// DFU status codes (DFU 1.1, section 6.1.2). Discriminants are the wire values
/// reported to the host in `DFU_GETSTATUS`.
///
/// The complete set is retained for future use even though only a subset is
/// produced by the current sinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    /// No error condition is present.
    Ok = 0x00,
    /// File is not targeted for use by this device.
    ErrTarget = 0x01,
    /// File is for this device but fails a vendor-specific verification test.
    ErrFile = 0x02,
    /// Device is unable to write memory.
    ErrWrite = 0x03,
    /// Memory erase function failed.
    ErrErase = 0x04,
    /// Memory erase check failed.
    ErrCheckErased = 0x05,
    /// Program memory function failed.
    ErrProg = 0x06,
    /// Programmed memory failed verification.
    ErrVerify = 0x07,
    /// Cannot program memory due to received address that is out of range.
    ErrAddress = 0x08,
    /// Received `DFU_DNLOAD` with `wLength` = 0, but device does not think it
    /// has all data yet.
    ErrNotDone = 0x09,
    /// Device's firmware is corrupt; it cannot return to run-time operations.
    ErrFirmware = 0x0a,
    /// iString indicates a vendor-specific error.
    ErrVendor = 0x0b,
    /// Device detected an unexpected USB reset signaling.
    ErrUsbr = 0x0c,
    /// Device detected an unexpected power-on reset.
    ErrPor = 0x0d,
    /// Something went wrong, but the device does not know what.
    ErrUnknown = 0x0e,
    /// Device stalled an unexpected request.
    ErrStalledPkt = 0x0f,
}

/// The `DFU_GETSTATUS` response payload (DFU 1.1, section 6.1.2).
///
/// This is a fixed 6-byte structure sent device-to-host:
///
/// | offset | size | field           |
/// | ------ | ---- | --------------- |
/// | 0      | 1    | `bStatus`       |
/// | 1      | 3    | `bwPollTimeout` (little-endian) |
/// | 4      | 1    | `bState`        |
/// | 5      | 1    | `iString`       |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GetStatus {
    /// Status of the most recent request (`bStatus`).
    pub status: Status,
    /// Minimum time, in milliseconds, the host should wait before the next
    /// `DFU_GETSTATUS` (`bwPollTimeout`, a 24-bit value).
    pub poll_timeout_ms: u32,
    /// State the device will enter after this request (`bState`).
    pub state: State,
    /// Index of a status-description string, or 0 for none (`iString`).
    pub string_index: u8,
}

impl GetStatus {
    /// Wire length of the response in bytes.
    pub const LEN: usize = 6;

    /// Serializes the response to its 6-byte wire representation.
    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let poll = self.poll_timeout_ms.to_le_bytes();
        [self.status as u8, poll[0], poll[1], poll[2], self.state as u8, self.string_index]
    }
}
