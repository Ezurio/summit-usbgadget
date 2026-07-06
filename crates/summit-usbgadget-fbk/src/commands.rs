use bytes::{Bytes, BytesMut};

use summit_usbgadget_swupdate::SwupdateSession;
use summit_usbgadget_swupdate::sysinfo::SystemInfo;

use super::protocol::{
    fastboot_getvar_reply, parse_command, send_data_header, send_static, DownloadKind,
    FetchTarget, FlashTarget, ParsedCommand, split_command,
};
use super::{
    EndpointAction, FbkState, FAIL_BADSIZE, FAIL_CLOSE, FAIL_CMD, FAIL_FLASH,
    FAIL_OPEN, FAIL_UNKNOWN_PART, INFO_WAIT_SWUPDATE, OKAY,
};

async fn begin_download_command(state: &mut FbkState, udc_name: &str, label: &str) -> bool {
    let Some(download) = state.download.as_mut() else {
        let _ = send_static(&mut state.tx, FAIL_OPEN).await;
        return false;
    };

    if let Err(err) = download.open().await {
        log::error!("[{udc_name}] {label} begin failed: {err}");
        let _ = send_static(&mut state.tx, FAIL_OPEN).await;
        return false;
    }
    true
}

async fn start_finish_download_command(
    state: &mut FbkState,
    udc_name: &str,
    label: &str,
) -> bool {
    let Some(download) = state.download.as_mut() else {
        let _ = send_static(&mut state.tx, FAIL_CLOSE).await;
        return false;
    };

    if !download.is_open() {
        log::error!("[{udc_name}] {label} failed: download not started");
        let _ = send_static(&mut state.tx, FAIL_CLOSE).await;
        return false;
    }

    // No more data will be sent: close the feed (EOF). The run loop now
    // observes the status signal for swupdate's verdict.
    download.eof();
    state.finish_pending = true;
    true
}

async fn handle_wopen_command(state: &mut FbkState, udc_name: &str) -> bool {
    if !begin_download_command(state, udc_name, "FBK WOpen").await {
        return false;
    }

    state.fbk_session_open = true;
    state.fastboot_pending_flash = false;
    log::warn!("[{udc_name}] FBK reply tx: OKAY (WOpen)");
    let _ = send_static(&mut state.tx, OKAY).await;
    true
}

async fn handle_fetch_command(state: &mut FbkState, udc_name: &str, target: FetchTarget) -> bool {
    let payload = SystemInfo::collect(Some(state.serial.clone())).to_json_bytes();
    let len = payload.len();
    let target = match target {
        FetchTarget::Sysinfo => "sysinfo",
        FetchTarget::SysinfoJson => "sysinfo.json",
    };
    log::warn!("[{udc_name}] fastboot reply tx: DATA{:08X} (fetch {target})", len);
    if send_data_header(&mut state.tx, len).await.is_err() {
        return false;
    }
    if state.tx.send_async(Bytes::from(payload)).await.is_err() {
        return false;
    }
    log::warn!("[{udc_name}] fastboot reply tx: OKAY (fetch)");
    let _ = send_static(&mut state.tx, OKAY).await;
    true
}

async fn start_download_transfer(
    state: &mut FbkState,
    udc_name: &str,
    len: usize,
    kind: DownloadKind,
) -> bool {
    if !state.download.as_ref().is_some_and(SwupdateSession::is_open)
        && !begin_download_command(state, udc_name, "FBK/fastboot").await
    {
        return false;
    }
    if send_data_header(&mut state.tx, len).await.is_err() {
        state.reset(udc_name, EndpointAction::Cancel, None).await;
        return false;
    }

    let fastboot_download = match kind {
        DownloadKind::Fastboot => true,
        DownloadKind::Plain => !state.fbk_session_open,
    };

    state.download_size = len;
    state.downloaded_size = 0;
    state.fastboot_pending_flash = fastboot_download;
    log::warn!(
        "[{udc_name}] {} reply tx: DATA{:08X} (expecting {} bytes)",
        if state.fastboot_pending_flash { "fastboot" } else { "FBK" },
        len,
        len
    );
    if len == 0 {
        state.download_size = 0;
        state.downloaded_size = 0;
        log::warn!("[{udc_name}] reply tx: OKAY (zero-length download)");
        let _ = send_static(&mut state.tx, OKAY).await;
    }
    true
}

async fn finish_flash_download(state: &mut FbkState, udc_name: &str, target: FlashTarget) -> bool {
    if !state.fastboot_pending_flash {
        let _ = send_static(&mut state.tx, FAIL_FLASH).await;
        return false;
    }
    if state.download_active() {
        let _ = send_static(&mut state.tx, FAIL_BADSIZE).await;
        return false;
    }

    let target = match target {
        FlashTarget::Update => "update",
        FlashTarget::Swu => "swu",
    };
    log::warn!("[{udc_name}] fastboot flash command received: {target}");
    let _ = send_static(&mut state.tx, INFO_WAIT_SWUPDATE).await;
    if !start_finish_download_command(state, udc_name, "fastboot flash/finish").await {
        state.fastboot_pending_flash = false;
        return false;
    }

    true
}

async fn handle_close_command(state: &mut FbkState, udc_name: &str) -> bool {
    if !start_finish_download_command(state, udc_name, "FBK close/finish").await {
        return false;
    }

    true
}

pub(super) async fn handle_command_chunk(
    state: &mut FbkState,
    udc_name: &str,
    chunk: &mut BytesMut,
) -> bool {
    let Some((cmd, consumed)) = split_command(chunk.as_ref()) else {
        return false;
    };
    let cmd = cmd.to_vec();
    let _ = chunk.split_to(consumed);

    log::warn!("[{udc_name}] FBK command rx: {}", String::from_utf8_lossy(&cmd));

    match parse_command(&cmd) {
        ParsedCommand::WOpen => handle_wopen_command(state, udc_name).await,
        ParsedCommand::GetVar => {
            if let Some(reply) = fastboot_getvar_reply(&cmd, &state.serial) {
                log::warn!("[{udc_name}] fastboot reply tx: {}", String::from_utf8_lossy(&reply));
                let _ = state.tx.send_async(Bytes::from(reply)).await;
                true
            } else {
                log::warn!("[{udc_name}] unsupported FBK command: {}", String::from_utf8_lossy(&cmd));
                let _ = send_static(&mut state.tx, FAIL_CMD).await;
                false
            }
        }
        ParsedCommand::Fetch(target) => handle_fetch_command(state, udc_name, target).await,
        ParsedCommand::FetchUnknownPart => {
            let _ = send_static(&mut state.tx, FAIL_UNKNOWN_PART).await;
            false
        }
        ParsedCommand::Download { len, kind } => start_download_transfer(state, udc_name, len, kind).await,
        ParsedCommand::Flash(target) => finish_flash_download(state, udc_name, target).await,
        ParsedCommand::FlashUnknownPart => {
            let _ = send_static(&mut state.tx, FAIL_UNKNOWN_PART).await;
            false
        }
        ParsedCommand::Close => handle_close_command(state, udc_name).await,
        ParsedCommand::Unsupported => {
            log::warn!("[{udc_name}] unsupported FBK command: {}", String::from_utf8_lossy(&cmd));
            let _ = send_static(&mut state.tx, FAIL_CMD).await;
            false
        }
    }
}
