//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

use super::{parse_command, DownloadKind, ParsedCommand};

#[test]
fn parse_command_preserves_trailing_payload() {
    let data = b"download:%00000004\0ABC";
    let (command, consumed) = parse_command(data).expect("command should parse");
    assert!(matches!(
        command,
        ParsedCommand::Download {
            len: 4,
            kind: DownloadKind::Fastboot
        }
    ));
    assert_eq!(consumed, b"download:%00000004".len());
    assert_eq!(&data[consumed..], b"\0ABC");
}

#[test]
fn parse_typo_download_preserves_trailing_payload() {
    let data = b"donwload:00000004ABC";
    let (command, consumed) = parse_command(data).expect("command should parse");
    assert!(matches!(
        command,
        ParsedCommand::Download {
            len: 4,
            kind: DownloadKind::Plain
        }
    ));
    assert_eq!(consumed, b"donwload:00000004".len());
    assert_eq!(&data[consumed..], b"ABC");
}

#[test]
fn parse_download_commands() {
    for (cmd, fastboot) in [
        (b"download:%00000004\r\n".as_slice(), true),
        (b"download:00000004".as_slice(), false),
        (b"donwload:00000004".as_slice(), false),
    ] {
        match parse_command(cmd).expect("command should parse").0 {
            ParsedCommand::Download { len, kind } => {
                assert_eq!(len, 4, "{cmd:?}");
                assert_eq!(matches!(kind, DownloadKind::Fastboot), fastboot, "{cmd:?}");
            }
            _ => panic!("download command should parse: {cmd:?}"),
        }
    }
}
