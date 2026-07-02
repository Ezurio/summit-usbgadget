//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! FBK / fastboot wire-protocol helpers.
//!
//! Reply tokens, command parsers, and small reply-send helpers shared by the
//! FBK state machine. All parsing is byte-oriented and tolerant of the
//! whitespace/NUL padding that host tools append to commands.

use std::io;

use bytes::Bytes;
use usb_gadget::function::custom::EndpointSender;

pub(super) const OKAY: &[u8] = b"OKAY";
pub(super) const FAIL_CMD: &[u8] = b"FAILCMD";
pub(super) const FAIL_CLOSE: &[u8] = b"FAILCLOSE";
pub(super) const FAIL_EPIPE: &[u8] = b"FAILEPIPE";
pub(super) const FAIL_NOTOPEN: &[u8] = b"FAILNOTOPEN";
pub(super) const FAIL_OPEN: &[u8] = b"FAILOPEN";
pub(super) const FAIL_BADSIZE: &[u8] = b"FAILBADSIZE";
pub(super) const FAIL_FLASH: &[u8] = b"FAILFLASH";
pub(super) const FAIL_UNKNOWN_PART: &[u8] = b"FAILUNKNOWNPART";
pub(super) const INFO_WAIT_SWUPDATE: &[u8] = b"INFOwaiting for SWUpdate";

pub(super) enum DownloadKind {
    Fbk,
    Fastboot,
}

pub(super) enum FetchTarget {
    Sysinfo,
    SysinfoJson,
}

pub(super) enum FlashTarget {
    Update,
    Swu,
}

pub(super) enum ParsedCommand {
    WOpen,
    GetVar,
    Fetch(FetchTarget),
    FetchUnknownPart,
    Download { len: usize, kind: DownloadKind },
    Flash(FlashTarget),
    FlashUnknownPart,
    Close,
    Unsupported,
}

pub(super) fn trim_command(data: &[u8]) -> &[u8] {
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

pub(super) fn split_command(data: &[u8]) -> Option<(&[u8], usize)> {
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
pub(super) fn command_arg<'a>(cmd: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    trim_command(cmd).strip_prefix(prefix)
}

pub(super) fn parse_download_len(cmd: &[u8]) -> Option<usize> {
    let payload = command_arg(cmd, b"download:%")
        .or_else(|| command_arg(cmd, b"donwload:"))
        .or_else(|| command_arg(cmd, b"download:"))?;
    let payload = std::str::from_utf8(trim_command(payload)).ok()?;
    usize::from_str_radix(payload, 16).ok()
}

pub(super) fn is_fastboot_download(cmd: &[u8]) -> bool {
    command_arg(cmd, b"download:%").is_some()
        || command_arg(cmd, b"download:").is_some()
        || command_arg(cmd, b"donwload:").is_some()
}

pub(super) fn parse_fastboot_fetch(cmd: &[u8]) -> Option<&[u8]> {
    let rest = command_arg(cmd, b"fetch:")?;
    Some(rest.split(|b| *b == b':').next().unwrap_or(rest))
}

pub(super) fn parse_command(cmd: &[u8]) -> ParsedCommand {
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
            kind: if is_fastboot_download(cmd) {
                DownloadKind::Fastboot
            } else {
                DownloadKind::Fbk
            },
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
pub(super) fn fastboot_getvar_reply(cmd: &[u8], serial: &str) -> Option<Vec<u8>> {
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
            let size = crate::sysinfo::SystemInfo::collect(Some(serial.to_owned()))
                .to_json_bytes()
                .len();
            Some(format!("OKAY{size:08X}").into_bytes())
        }
        _ => None,
    }
}

pub(super) async fn send_static(tx: &mut EndpointSender, data: &'static [u8]) -> io::Result<()> {
    tx.send_async(Bytes::from_static(data)).await
}

pub(super) async fn send_data_header(tx: &mut EndpointSender, len: usize) -> io::Result<()> {
    tx.send_async(Bytes::from(format!("DATA{len:08X}"))).await
}

#[cfg(test)]
mod tests {
    use super::{is_fastboot_download, parse_download_len, split_command};

    #[test]
    fn split_command_preserves_trailing_payload() {
        let data = b"download:%00000004ABCD";
        let (cmd, consumed) = split_command(data).expect("command should parse");
        assert_eq!(cmd, b"download:%00000004");
        assert_eq!(consumed, cmd.len());
        assert_eq!(&data[consumed..], b"ABCD");
    }

    #[test]
    fn parse_download_len_ignores_trailing_padding() {
        assert_eq!(parse_download_len(b"download:%00000004\r\n"), Some(4));
    }

    #[test]
    fn straight_fastboot_download_sets_flash_state() {
        assert!(is_fastboot_download(b"download:00000004"));
    }
}
