//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

use super::{download_kind, parse_command, parse_download_len, split_command, DownloadKind, ParsedCommand};

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
fn percent_prefixed_download_sets_fastboot_state() {
    assert!(matches!(download_kind(b"download:%00000004"), Some(DownloadKind::Fastboot)));
}

#[test]
fn plain_download_is_classified_as_plain() {
    match parse_command(b"download:00000004") {
        ParsedCommand::Download { len, kind: DownloadKind::Plain } => assert_eq!(len, 4),
        _ => panic!("plain download should parse as a session-dependent download"),
    }
}

#[test]
fn typo_download_is_classified_as_plain() {
    match parse_command(b"donwload:00000004") {
        ParsedCommand::Download { len, kind: DownloadKind::Plain } => assert_eq!(len, 4),
        _ => panic!("typo download should parse as a session-dependent download"),
    }
}
