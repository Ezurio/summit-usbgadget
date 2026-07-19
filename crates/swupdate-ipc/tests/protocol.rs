//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
// Copyright (C) 2026 Ezurio LLC.
//
//! Protocol-level unit tests for the SWUpdate IPC wire types.

use swupdate_ipc::proto::{
    self, IPC_MAGIC, IpcMessage, MsgType, PROGRESS_API_VERSION, ProgressConnectAck, ProgressMsg,
    RecoveryStatus, SwupdateRequest, decode_recovery_status, read_c_string, write_c_string,
};
use swupdate_ipc::{InstallMode, InstallRequest, InstallSource, InstallStatus};

#[test]
fn ipc_message_new_stamps_magic_and_type() {
    let msg = IpcMessage::new(MsgType::GET_STATUS);
    assert_eq!(msg.magic, IPC_MAGIC as i32);
    assert_eq!(msg.type_, MsgType::GET_STATUS as i32);
    assert!(msg.has_type(MsgType::GET_STATUS));
    assert!(!msg.has_type(MsgType::ACK));
    assert_eq!(msg.as_bytes().len(), std::mem::size_of::<IpcMessage>());
}

#[test]
fn ipc_message_safe_payload_helpers_fill_generated_union_members() {
    let mut install = IpcMessage::new(MsgType::REQ_INSTALL);
    let mut request = SwupdateRequest::prepare();
    request.set_software_set("stable").unwrap();
    install.set_install_request(request);

    let mut postupdate = IpcMessage::new(MsgType::POST_UPDATE);
    postupdate.set_postupdate_info(b"reboot");

    let mut aes = IpcMessage::new(MsgType::SET_AES_KEY);
    aes.set_aes_key("a", "b").unwrap();

    let mut versions = IpcMessage::new(MsgType::SET_VERSIONS_RANGE);
    versions
        .set_version_range(Some("1.0"), Some("2.0"), Some("1.5"))
        .unwrap();

    unsafe {
        assert_eq!(
            read_c_string(&install.data.instmsg.req.software_set),
            "stable"
        );
        assert_eq!(postupdate.data.procmsg.len, 6);
        assert_eq!(read_c_string(&aes.data.aeskeymsg.key_ascii), "a");
        assert_eq!(
            read_c_string(&versions.data.versions.maximum_version),
            "2.0"
        );
    }
}

#[test]
fn native_install_request_encodes_and_status_decodes() {
    let request = InstallRequest {
        software_set: "stable".to_owned(),
        running_mode: "full-b".to_owned(),
        source: InstallSource::Local,
        mode: InstallMode::Install,
        disable_store_swu: true,
    };
    let message = request.encode().unwrap();
    assert!(message.has_type(MsgType::REQ_INSTALL));

    unsafe {
        assert_eq!(
            message.data.instmsg.req.source,
            proto::SourceType::SOURCE_LOCAL
        );
        assert_eq!(
            message.data.instmsg.req.dry_run,
            proto::RunType::RUN_INSTALL
        );
        assert!(message.data.instmsg.req.disable_store_swu);
        assert_eq!(
            read_c_string(&message.data.instmsg.req.software_set),
            "stable"
        );

        let mut status_message = IpcMessage::zeroed();
        status_message.data.status.current = RecoveryStatus::RUN as i32;
        status_message.data.status.last_result = RecoveryStatus::SUCCESS as i32;
        write_c_string(&mut status_message.data.status.desc, "complete").unwrap();

        let status = InstallStatus::decode(&status_message);
        assert_eq!(status.current, Some(RecoveryStatus::RUN));
        assert_eq!(status.last_result, Some(RecoveryStatus::SUCCESS));
        assert_eq!(status.description, "complete");
    }
}

#[test]
fn prepared_request_has_default_fields() {
    let req = SwupdateRequest::prepare();
    assert_eq!(req.apiversion, proto::SWUPDATE_API_VERSION);
    assert_eq!(req.dry_run, proto::RunType::RUN_DEFAULT);
    assert_eq!(req.source, proto::SourceType::SOURCE_UNKNOWN);
    assert!(req.info.iter().all(|&c| c == 0));
}

#[test]
fn c_string_round_trips_and_rejects_overflow() {
    let mut buf = [0 as std::os::raw::c_char; 8];
    write_c_string(&mut buf, "hello").unwrap();
    assert_eq!(read_c_string(&buf), "hello");
    assert!(write_c_string(&mut buf, "0123456789").is_err());
    assert_eq!(read_c_string(&buf), "hello");
}

#[test]
fn request_setters_write_running_mode_and_software_set() {
    let mut req = SwupdateRequest::prepare();
    req.set_software_set("stable").unwrap();
    req.set_running_mode("full-b").unwrap();
    assert_eq!(read_c_string(&req.software_set), "stable");
    assert_eq!(read_c_string(&req.running_mode), "full-b");
}

#[test]
fn decode_recovery_status_round_trips() {
    for status in [
        RecoveryStatus::IDLE,
        RecoveryStatus::START,
        RecoveryStatus::RUN,
        RecoveryStatus::SUCCESS,
        RecoveryStatus::FAILURE,
        RecoveryStatus::DOWNLOAD,
        RecoveryStatus::DONE,
        RecoveryStatus::SUBPROCESS,
        RecoveryStatus::PROGRESS,
    ] {
        assert_eq!(decode_recovery_status(status as u32), Ok(status));
    }
    assert_eq!(decode_recovery_status(99), Err(99));
    assert!(RecoveryStatus::SUCCESS.is_terminal());
    assert!(RecoveryStatus::FAILURE.is_terminal());
    assert!(!RecoveryStatus::RUN.is_terminal());
}

#[test]
fn terminal_status_identification_is_stable() {
    assert!(RecoveryStatus::SUCCESS.is_terminal());
    assert!(RecoveryStatus::FAILURE.is_terminal());
    assert!(!RecoveryStatus::IDLE.is_terminal());
}

// NOTE: there is intentionally no test decoding an untrusted raw int into
// `SourceType`. Unlike `progress_msg::status` (populated straight from the
// wire, see `decode_recovery_status`), `SourceType`-typed fields
// (`SwupdateRequest::source`, `procmsg.source`) are always filled by this
// client itself before sending, never decoded from a received frame. See the
// module-level safety note in `proto.rs`.

#[test]
fn progress_msg_accessors_decode_fields() {
    let mut msg = ProgressMsg::zeroed();
    msg.apiversion = PROGRESS_API_VERSION;
    msg.status = RecoveryStatus::RUN as u32;
    write_c_string(&mut msg.cur_image, "rootfs.ext4").unwrap();
    write_c_string(&mut msg.hnd_name, "raw_handler").unwrap();
    let info = b"installing";
    for (slot, &byte) in msg.info.iter_mut().zip(info.iter()) {
        *slot = byte as std::os::raw::c_char;
    }
    msg.infolen = info.len() as u32;
    msg.cur_step = 2;
    msg.nsteps = 3;
    msg.cur_percent = 50;

    assert_eq!(msg.status(), Ok(RecoveryStatus::RUN));
    assert_eq!(msg.current_step(), 2);
    assert_eq!(msg.total_steps(), 3);
    assert_eq!(msg.current_percent(), 50);
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
    #[cfg(swupdate_progress_msg_packed)]
    {
        assert_eq!(std::mem::size_of::<ProgressMsg>(), 2408);
        assert_eq!(std::mem::align_of::<ProgressMsg>(), 1);
    }
    #[cfg(not(swupdate_progress_msg_packed))]
    {
        assert_eq!(std::mem::size_of::<ProgressMsg>(), 2416);
        assert_eq!(std::mem::align_of::<ProgressMsg>(), 8);
    }
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
    assert_eq!(
        swupdate_ipc::ctrl_socket_path(),
        std::path::Path::new("/tmp/sockinstctrl")
    );
    assert_eq!(
        swupdate_ipc::progress_socket_path(),
        std::path::Path::new("/tmp/swupdateprog")
    );

    unsafe {
        std::env::set_var("TMPDIR", "/run/tmpdir");
    }
    assert_eq!(
        swupdate_ipc::ctrl_socket_path(),
        std::path::Path::new("/run/tmpdir/sockinstctrl")
    );

    unsafe {
        std::env::set_var("RUNTIME_DIRECTORY", "/run/swupdate");
    }
    assert_eq!(
        swupdate_ipc::progress_socket_path(),
        std::path::Path::new("/run/swupdate/swupdateprog")
    );

    unsafe {
        std::env::remove_var("RUNTIME_DIRECTORY");
        std::env::remove_var("TMPDIR");
    }
}
