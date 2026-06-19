//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
// Copyright (C) 2026 Ezurio LLC.
//
//! Protocol-level unit tests for the SWUpdate IPC wire types.

use swupdate_ipc::proto::{
    self, IPC_MAGIC, IpcMessage, MsgType, PROGRESS_API_VERSION, ProgressConnectAck, ProgressMsg,
    RecoveryStatus, SourceType, SwupdateRequest, read_c_string, write_c_string,
};

#[test]
fn ipc_message_new_stamps_magic_and_type() {
    let msg = IpcMessage::new(MsgType::GetStatus);
    assert_eq!(msg.magic, IPC_MAGIC);
    assert_eq!(msg.type_, MsgType::GetStatus as i32);
    assert_eq!(msg.as_bytes().len(), std::mem::size_of::<IpcMessage>());
}

#[test]
fn prepared_request_has_default_fields() {
    let req = SwupdateRequest::prepare();
    assert_eq!(req.apiversion, proto::SWUPDATE_API_VERSION);
    assert_eq!(req.dry_run, proto::RunType::Default as i32);
    assert_eq!(req.source, 0);
    assert!(req.info.iter().all(|&c| c == 0));
}

#[test]
fn c_string_round_trips_and_truncates() {
    let mut buf = [0 as std::os::raw::c_char; 8];
    write_c_string(&mut buf, "hello");
    assert_eq!(read_c_string(&buf), "hello");
    // last byte stays NUL even when the value would overflow.
    write_c_string(&mut buf, "0123456789");
    assert_eq!(read_c_string(&buf), "0123456");
    assert_eq!(buf[7], 0);
}

#[test]
fn request_setters_write_running_mode_and_software_set() {
    let mut req = SwupdateRequest::prepare();
    req.set_software_set("stable");
    req.set_running_mode("full-b");
    assert_eq!(read_c_string(&req.software_set), "stable");
    assert_eq!(read_c_string(&req.running_mode), "full-b");
}

#[test]
fn recovery_status_try_from_round_trips() {
    for status in [
        RecoveryStatus::Idle,
        RecoveryStatus::Start,
        RecoveryStatus::Run,
        RecoveryStatus::Success,
        RecoveryStatus::Failure,
        RecoveryStatus::Download,
        RecoveryStatus::Done,
        RecoveryStatus::Subprocess,
        RecoveryStatus::Progress,
    ] {
        assert_eq!(RecoveryStatus::try_from(status as i32), Ok(status));
    }
    assert_eq!(RecoveryStatus::try_from(99), Err(99));
    assert!(RecoveryStatus::Success.is_terminal());
    assert!(RecoveryStatus::Failure.is_terminal());
    assert!(!RecoveryStatus::Run.is_terminal());
}

#[test]
fn terminal_status_identification_is_stable() {
    assert!(RecoveryStatus::Success.is_terminal());
    assert!(RecoveryStatus::Failure.is_terminal());
    assert!(!RecoveryStatus::Idle.is_terminal());
}

#[test]
fn source_type_try_from_round_trips() {
    assert_eq!(SourceType::try_from(1), Ok(SourceType::Webserver));
    assert_eq!(SourceType::try_from(4), Ok(SourceType::Local));
    assert_eq!(SourceType::try_from(42), Err(42));
}

#[test]
fn progress_msg_accessors_decode_fields() {
    let mut msg = ProgressMsg::zeroed();
    msg.apiversion = PROGRESS_API_VERSION;
    msg.status = RecoveryStatus::Run as i32;
    write_c_string(&mut msg.cur_image, "rootfs.ext4");
    write_c_string(&mut msg.hnd_name, "raw_handler");
    let info = b"installing";
    for (slot, &byte) in msg.info.iter_mut().zip(info.iter()) {
        *slot = byte as std::os::raw::c_char;
    }
    msg.infolen = info.len() as u32;

    assert_eq!(msg.status(), Ok(RecoveryStatus::Run));
    assert_eq!(msg.cur_image(), "rootfs.ext4");
    assert_eq!(msg.hnd_name(), "raw_handler");
    assert_eq!(msg.info(), "installing");
}

#[test]
fn progress_connect_ack_validates_version_and_magic() {
    let mut ack = ProgressConnectAck::zeroed();
    ack.apiversion = PROGRESS_API_VERSION;
    ack.magic[0] = b'A' as std::os::raw::c_char;
    ack.magic[1] = b'C' as std::os::raw::c_char;
    ack.magic[2] = b'K' as std::os::raw::c_char;
    assert!(ack.is_major_compatible());
    assert!(ack.has_valid_magic());

    ack.apiversion = 0x0001_0000; // major version 1
    assert!(!ack.is_major_compatible());

    ack.magic[2] = b'X' as std::os::raw::c_char;
    assert!(!ack.has_valid_magic());
}

#[test]
fn fixed_sizes_match_c_abi() {
    assert_eq!(std::mem::size_of::<ProgressMsg>(), 2416);
    assert_eq!(std::mem::size_of::<ProgressConnectAck>(), 8);
    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(std::mem::size_of::<SwupdateRequest>(), 1056);
        assert_eq!(std::mem::size_of::<IpcMessage>(), 3120);
    }
}

#[test]
fn socket_paths_honor_environment_precedence() {
    unsafe {
        std::env::remove_var("RUNTIME_DIRECTORY");
        std::env::remove_var("TMPDIR");
    }
    assert_eq!(swupdate_ipc::ctrl_socket_path(), std::path::Path::new("/tmp/sockinstctrl"));
    assert_eq!(swupdate_ipc::progress_socket_path(), std::path::Path::new("/tmp/swupdateprog"));

    unsafe { std::env::set_var("TMPDIR", "/run/tmpdir"); }
    assert_eq!(swupdate_ipc::ctrl_socket_path(), std::path::Path::new("/run/tmpdir/sockinstctrl"));

    unsafe { std::env::set_var("RUNTIME_DIRECTORY", "/run/swupdate"); }
    assert_eq!(swupdate_ipc::progress_socket_path(), std::path::Path::new("/run/swupdate/swupdateprog"));

    unsafe {
        std::env::remove_var("RUNTIME_DIRECTORY");
        std::env::remove_var("TMPDIR");
    }
}
