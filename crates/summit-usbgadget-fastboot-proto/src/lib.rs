//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! Transport-agnostic FBK / fastboot wire-protocol helpers.
//!
//! Reply tokens, command parsers, and reply-formatting helpers shared by every
//! FBK / fastboot transport (USB bulk endpoints and TCP framing). All parsing
//! is byte-oriented and tolerant of the whitespace/NUL padding that host tools
//! append to commands. Nothing here touches a transport, so both the USB
//! function and the TCP service reuse exactly the same command vocabulary.

use summit_usbgadget_swupdate::sysinfo::{boot_info, fuse_serial, system_info_json};

/// Fixed reply / info tokens shared by every fastboot transport. Grouped into
/// one module so callers can `use fastboot_proto::reply::*;` instead of
/// naming each token individually.
pub mod reply {
    /// Reply token: the requested command completed successfully.
    pub const OKAY: &[u8] = b"OKAY";
    /// Reply token: the reported download size did not match the bytes received.
    pub const FAIL_BADSIZE: &[u8] = b"FAILbad size";
    /// Reply token: closing / finishing the download failed.
    pub const FAIL_CLOSE: &[u8] = b"FAILclose";
    /// Reply token: the command was not recognized.
    pub const FAIL_CMD: &[u8] = b"FAILunknown command";
    /// Reply token: writing the payload to the sink failed.
    pub const FAIL_EPIPE: &[u8] = b"FAILwrite failed";
    /// Reply token: a flash was requested before any download.
    pub const FAIL_FLASH: &[u8] = b"FAILflash before download";
    /// Reply token: no download session is open.
    pub const FAIL_NOTOPEN: &[u8] = b"FAILnot open";
    /// Reply token: opening the download session failed.
    pub const FAIL_OPEN: &[u8] = b"FAILopen failed";
    /// Reply token: the requested partition is unknown.
    pub const FAIL_UNKNOWN_PART: &[u8] = b"FAILpartition does not exist";
    /// Info token: the device is waiting for SWUpdate to finish installing.
    pub const INFO_WAIT_SWUPDATE: &[u8] = b"INFOwaiting for SWUpdate";
}

/// Whether a `download:` request was issued as a fastboot (`download:%`) or a
/// plain FBK (`download:`) transfer.
pub enum DownloadKind {
    /// Plain FBK download (`download:` / `donwload:`).
    Plain,
    /// Fastboot download (`download:%`).
    Fastboot,
}

/// Target of a fastboot `fetch:` request.
pub enum FetchTarget {
    /// `fetch:sysinfo`
    Sysinfo,
    /// `fetch:sysinfo.json`
    SysinfoJson,
}

/// Target partition of a fastboot `flash:` request.
pub enum FlashTarget {
    /// `flash:update`
    Update,
    /// `flash:swu`
    Swu,
}

/// A parsed FBK / fastboot command.
pub enum ParsedCommand {
    /// `WOpen:` — open an FBK download session.
    WOpen,
    /// `getvar:<name>` — read a device variable.
    GetVar(String),
    /// `fetch:<target>` — upload device information.
    Fetch(FetchTarget),
    /// `fetch:<unknown>` — upload of an unsupported partition.
    FetchUnknownPart,
    /// `download:<len>` — begin a data-phase transfer of `len` bytes.
    Download {
        /// Number of bytes the host will send in the data phase.
        len: usize,
        /// Whether the transfer is a fastboot or plain FBK download.
        kind: DownloadKind,
    },
    /// `flash:<target>` — install the previously downloaded image.
    Flash(FlashTarget),
    /// `flash:<unknown>` — flash of an unsupported partition.
    FlashUnknownPart,
    /// `Close` — finish an FBK download session.
    Close,
    /// A command this device does not implement.
    Unsupported,
}

const DOWNLOAD_SIZE_WIDTH: usize = 8;

fn is_padding_byte(b: u8) -> bool {
    b == b'\0' || b.is_ascii_whitespace()
}

/// Parses one command from the front of `data`, returning the command and the
/// bytes to remove from the stream. A download command consumes its fixed-size
/// header only, leaving any immediately following payload untouched.
pub fn parse_command(data: &[u8]) -> Option<(ParsedCommand, usize)> {
    let start = data.iter().position(|b| !is_padding_byte(*b))?;
    let rest = &data[start..];

    let (command, consumed) = if let Some(colon) = rest.iter().position(|b| *b == b':') {
        let name = &rest[..colon];
        let args = &rest[colon + 1..];
        let token_len = || {
            args.iter()
                .position(|b| is_padding_byte(*b))
                .unwrap_or(args.len())
        };

        let (command, arg_len) = match name {
            b"download" | b"donwload" => {
                // Only the correctly spelled prefix supports the fastboot `%`
                // marker. The size is a fixed 8 hex digits, and the data-phase
                // payload can follow immediately with no separator, so it must
                // be sliced directly rather than scanned for.
                let percent = name == b"download" && args.starts_with(b"%");
                let size_start = percent as usize;
                let size =
                    std::str::from_utf8(args.get(size_start..size_start + DOWNLOAD_SIZE_WIDTH)?)
                        .ok()?;
                let len = usize::from_str_radix(size, 16).ok()?;
                let kind = if percent {
                    DownloadKind::Fastboot
                } else {
                    DownloadKind::Plain
                };
                (
                    ParsedCommand::Download { len, kind },
                    size_start + DOWNLOAD_SIZE_WIDTH,
                )
            }
            b"WOpen" if args.is_empty() => (ParsedCommand::WOpen, 0),
            b"getvar" => {
                let len = token_len();
                match std::str::from_utf8(&args[..len])
                    .ok()
                    .filter(|arg| !arg.is_empty())
                {
                    Some(arg) => (ParsedCommand::GetVar(arg.to_owned()), len),
                    None => (ParsedCommand::Unsupported, len),
                }
            }
            b"fetch" => {
                let len = token_len();
                let arg = std::str::from_utf8(&args[..len]).ok()?;
                let command = match arg.split(':').next().unwrap_or_default() {
                    "sysinfo" => ParsedCommand::Fetch(FetchTarget::Sysinfo),
                    "sysinfo.json" => ParsedCommand::Fetch(FetchTarget::SysinfoJson),
                    _ => ParsedCommand::FetchUnknownPart,
                };
                (command, len)
            }
            b"flash" => {
                let len = token_len();
                let command = match &args[..len] {
                    b"update" => ParsedCommand::Flash(FlashTarget::Update),
                    b"swu" => ParsedCommand::Flash(FlashTarget::Swu),
                    _ => ParsedCommand::FlashUnknownPart,
                };
                (command, len)
            }
            _ => (ParsedCommand::Unsupported, token_len()),
        };
        (command, colon + 1 + arg_len)
    } else {
        // No colon at all: the only valid command in this form is "Close".
        let len = rest
            .iter()
            .position(|b| is_padding_byte(*b))
            .unwrap_or(rest.len());
        let command = if &rest[..len] == b"Close" {
            ParsedCommand::Close
        } else {
            ParsedCommand::Unsupported
        };
        (command, len)
    };
    Some((command, start + consumed))
}

/// Builds the reply for a fastboot `getvar:<name>` query, or `None` for
/// variables this device does not expose.
pub fn fastboot_getvar_reply(arg: &str, _serial: &str) -> Option<Vec<u8>> {
    match arg {
        "version" => Some(b"OKAY0.4".to_vec()),
        "max-download-size" => Some(b"OKAY400000000".to_vec()),
        "max-fetch-size" => Some(b"OKAY00010000".to_vec()),
        "product" => Some(b"OKAYsummit-usbgadget".to_vec()),
        "serialno" => Some(format!("OKAY{}", fuse_serial()).into_bytes()),
        "is-userspace" => Some(b"OKAYyes".to_vec()),
        "current-slot" => boot_info()
            .current_side_option()
            .map(|side| format!("OKAY{side}").into_bytes()),
        "slot-num" => {
            let num = if boot_info().is_single_slot() { 1 } else { 2 };
            Some(format!("OKAY{num}").into_bytes())
        }
        "all" => Some(b"OKAYversion:0.4".to_vec()),
        "has-slot:update" | "has-slot:swu" => Some(b"OKAYno".to_vec()),
        "is-logical:update" | "is-logical:swu" => Some(b"OKAYno".to_vec()),
        "partition-type:update" | "partition-type:swu" => Some(b"OKAYraw".to_vec()),
        "partition-size:update" | "partition-size:swu" => Some(b"OKAY400000000".to_vec()),
        "partition-size:sysinfo" | "partition-size:sysinfo.json" => {
            let json = system_info_json();
            Some(format!("OKAY{:08X}", json.len()).into_bytes())
        }
        _ => None,
    }
}

/// Formats the `DATA%08X` data-phase reply header for a payload of `len` bytes.
pub fn data_header(len: usize) -> String {
    format!("DATA{len:08X}")
}

#[cfg(test)]
mod tests;
