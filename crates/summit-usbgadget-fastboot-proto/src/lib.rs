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

use summit_usbgadget_swupdate::sysinfo::SystemInfo;

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
    GetVar,
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

fn trim_command(data: &[u8]) -> &[u8] {
    let start = data.iter().position(|b| !matches!(*b, b'\0' | b' ' | b'\t' | b'\r' | b'\n'));
    let Some(start) = start else {
        return &[];
    };
    let end = data
        .iter()
        .rposition(|b| !matches!(*b, b'\0' | b' ' | b'\t' | b'\r' | b'\n'))
        .expect("trim_command start implies non-empty slice");
    &data[start..=end]
}

/// Splits the first command token off the front of `data`, returning the
/// command bytes and the number of leading bytes consumed (the command plus any
/// skipped padding). A `download:` request keeps its fixed-width hex argument
/// even when the host concatenates the payload immediately after it.
pub fn split_command(data: &[u8]) -> Option<(&[u8], usize)> {
    let start = data.iter().position(|b| !matches!(*b, b'\0' | b' ' | b'\t' | b'\r' | b'\n'))?;
    let rest = &data[start..];

    for prefix in [b"download:%".as_slice(), b"download:", b"donwload:"] {
        let cmd_len = prefix.len() + 8;
        if rest.len() >= cmd_len
            && rest.starts_with(prefix)
            && rest[prefix.len()..cmd_len].iter().all(u8::is_ascii_hexdigit)
        {
            let consumed = start + cmd_len;
            return Some((&data[start..consumed], consumed));
        }
    }

    let end = rest
        .iter()
        .position(|b| matches!(*b, b'\0' | b' ' | b'\t' | b'\r' | b'\n'))
        .unwrap_or(rest.len());
    let consumed = start + end;
    Some((&data[start..start + end], consumed))
}

/// Returns the argument following a command `prefix`, with the surrounding
/// whitespace/NUL padding trimmed first. `None` if `cmd` does not start with
/// `prefix`. This is the shared shape of every FBK / fastboot command parser.
fn command_arg<'a>(cmd: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    trim_command(cmd).strip_prefix(prefix)
}

fn parse_download_len(cmd: &[u8]) -> Option<usize> {
    let payload = command_arg(cmd, b"download:%")
        .or_else(|| command_arg(cmd, b"donwload:"))
        .or_else(|| command_arg(cmd, b"download:"))?;
    let payload = std::str::from_utf8(trim_command(payload)).ok()?;
    usize::from_str_radix(payload, 16).ok()
}

fn download_kind(cmd: &[u8]) -> Option<DownloadKind> {
    if command_arg(cmd, b"download:%").is_some() {
        Some(DownloadKind::Fastboot)
    } else if command_arg(cmd, b"donwload:").is_some() || command_arg(cmd, b"download:").is_some() {
        Some(DownloadKind::Plain)
    } else {
        None
    }
}

fn parse_fastboot_fetch(cmd: &[u8]) -> Option<&[u8]> {
    let rest = command_arg(cmd, b"fetch:")?;
    Some(rest.split(|b| *b == b':').next().unwrap_or(rest))
}

/// Classifies a single FBK / fastboot command.
pub fn parse_command(cmd: &[u8]) -> ParsedCommand {
    let cmd = trim_command(cmd);

    if cmd.starts_with(b"WOpen:") {
        return ParsedCommand::WOpen;
    }

    if command_arg(cmd, b"getvar:").is_some() {
        return ParsedCommand::GetVar;
    }

    if let Some(target) = parse_fastboot_fetch(cmd) {
        return match target {
            b"sysinfo" => ParsedCommand::Fetch(FetchTarget::Sysinfo),
            b"sysinfo.json" => ParsedCommand::Fetch(FetchTarget::SysinfoJson),
            _ => ParsedCommand::FetchUnknownPart,
        };
    }

    if let Some(len) = parse_download_len(cmd) {
        return ParsedCommand::Download {
            len,
            kind: download_kind(cmd).expect("parse_download_len implies a download prefix"),
        };
    }

    if let Some(partition) = command_arg(cmd, b"flash:") {
        return match partition {
            b"update" => ParsedCommand::Flash(FlashTarget::Update),
            b"swu" => ParsedCommand::Flash(FlashTarget::Swu),
            _ => ParsedCommand::FlashUnknownPart,
        };
    }

    if cmd.eq_ignore_ascii_case(b"Close") {
        return ParsedCommand::Close;
    }

    ParsedCommand::Unsupported
}

/// Builds the reply for a fastboot `getvar:<name>` query, or `None` for
/// variables this device does not expose.
pub fn fastboot_getvar_reply(cmd: &[u8], serial: &str) -> Option<Vec<u8>> {
    match trim_command(cmd) {
        b"getvar:version" => Some(b"OKAY0.4".to_vec()),
        b"getvar:max-download-size" => Some(b"OKAY400000000".to_vec()),
        b"getvar:max-fetch-size" => Some(b"OKAY00010000".to_vec()),
        b"getvar:product" => Some(b"OKAYsummit-usbgadget".to_vec()),
        b"getvar:serialno" => Some(format!("OKAY{serial}").into_bytes()),
        b"getvar:is-userspace" => Some(b"OKAYyes".to_vec()),
        b"getvar:all" => Some(b"OKAYversion:0.4".to_vec()),
        cmd if matches!(command_arg(cmd, b"getvar:has-slot:"), Some(b"update" | b"swu")) => {
            Some(b"OKAYno".to_vec())
        }
        cmd if matches!(command_arg(cmd, b"getvar:is-logical:"), Some(b"update" | b"swu")) => {
            Some(b"OKAYno".to_vec())
        }
        cmd if matches!(command_arg(cmd, b"getvar:partition-type:"), Some(b"update" | b"swu")) => {
            Some(b"OKAYraw".to_vec())
        }
        cmd if matches!(command_arg(cmd, b"getvar:partition-size:"), Some(b"update" | b"swu")) => {
            Some(b"OKAY400000000".to_vec())
        }
        cmd if matches!(command_arg(cmd, b"getvar:partition-size:"), Some(b"sysinfo" | b"sysinfo.json")) => {
            let mut json = Vec::new();
            SystemInfo::collect(Some(serial.to_owned())).write_json(&mut json);
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
