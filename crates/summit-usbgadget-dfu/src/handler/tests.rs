//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

use crc32fast::Hasher;
use usb_gadget::function::custom::CtrlReq;

use summit_usbgadget_swupdate::SwupdateParams;

use crate::config::DfuConfig;
use crate::protocol::{request, State, Status};

use super::{DFU_SUFFIX_LEN, Dfu, flushable_len, has_valid_dfu_suffix};

fn test_config() -> DfuConfig {
    DfuConfig {
        download: SwupdateParams::default(),
        upload: None,
        transfer_size: 4096,
        poll_timeout_ms: 10,
    }
}

fn out_req(request: u8) -> CtrlReq {
    CtrlReq { request_type: 0x21, request, value: 0, index: 0, length: 0 }
}

#[test]
fn valid_dfu_suffix_is_recognized() {
    let payload = b"firmware";
    let mut suffix = [0u8; DFU_SUFFIX_LEN];
    suffix[8..11].copy_from_slice(b"UFD");
    suffix[11] = DFU_SUFFIX_LEN as u8;
    let mut prefix_crc = Hasher::new();
    prefix_crc.update(payload);
    let mut full_crc = prefix_crc.clone();
    full_crc.update(&suffix[..DFU_SUFFIX_LEN - 4]);
    let crc = !full_crc.finalize();
    suffix[12..].copy_from_slice(&crc.to_le_bytes());
    assert!(has_valid_dfu_suffix(&prefix_crc, &suffix));
}

#[test]
fn non_suffix_tail_is_not_stripped() {
    let mut prefix_crc = Hasher::new();
    prefix_crc.update(b"firmware");
    let mut tail = [0u8; DFU_SUFFIX_LEN];
    tail[8..11].copy_from_slice(b"BAD");
    tail[11] = DFU_SUFFIX_LEN as u8;
    assert!(!has_valid_dfu_suffix(&prefix_crc, &tail));
}

#[test]
fn bad_crc_invalidates_suffix() {
    let mut prefix_crc = Hasher::new();
    prefix_crc.update(b"firmware");
    let mut suffix = [0u8; DFU_SUFFIX_LEN];
    suffix[8..11].copy_from_slice(b"UFD");
    suffix[11] = DFU_SUFFIX_LEN as u8;
    suffix[12..].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    assert!(!has_valid_dfu_suffix(&prefix_crc, &suffix));
}

#[test]
fn suffix_window_keeps_last_sixteen_bytes() {
    assert_eq!(flushable_len(DFU_SUFFIX_LEN - 1), None);
    assert_eq!(flushable_len(DFU_SUFFIX_LEN), None);
    assert_eq!(flushable_len(DFU_SUFFIX_LEN + 4), Some(4));
    assert_eq!(flushable_len(20), Some(20 - DFU_SUFFIX_LEN));
}

#[tokio::test]
async fn get_status_reports_dnbusy_when_buffer_limit_is_reached() {
    let mut dfu = Dfu::new(test_config());
    dfu.sink = None;
    dfu.state = State::DnloadSync;
    dfu.status = Status::Ok;
    assert_eq!(dfu.get_status().as_slice()[4], State::DnloadIdle as u8);
}

#[tokio::test]
async fn get_status_reports_configured_poll_timeout_while_busy() {
    let mut dfu = Dfu::new(test_config());
    dfu.state = State::DnBusy;
    dfu.status = Status::Ok;

    let status = dfu.get_status();

    assert_eq!(status.as_slice()[1], 10);
    assert_eq!(status.as_slice()[2], 0);
    assert_eq!(status.as_slice()[3], 0);
}

#[tokio::test]
async fn clrstatus_resets_stale_manifestation_state() {
    let mut dfu = Dfu::new(test_config());
    dfu.state = State::Error;
    dfu.status = Status::ErrVerify;

    dfu.handle_out(&out_req(request::CLRSTATUS)).await.expect("clrstatus should succeed");

    assert!(dfu.sink.is_some());
    assert_eq!(dfu.state, State::DfuIdle);
    assert_eq!(dfu.status, Status::Ok);
}

#[test]
fn initial_state_is_dfu_idle() {
    let dfu = Dfu::new(test_config());

    assert_eq!(dfu.state, State::DfuIdle);
    assert_eq!(dfu.status, Status::Ok);
}
