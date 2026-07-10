//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
// Copyright (C) 2026 Ezurio LLC.
//
//! Async (tokio) SWUpdate IPC client.
//!
//! These are the `async` counterparts of the [`crate::blocking`] API, built on
//! `tokio::net::UnixStream`. The wire protocol is identical; only the I/O model
//! differs.

use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::Stream;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::{sleep, timeout};

use crate::error::{Error, Result};
use crate::proto::{
    IPC_MAGIC, IpcMessage, MsgType, ProgressConnectAck, ProgressMsg, SwupdateRequest, write_c_string,
};
use crate::socket::{ctrl_socket_path, progress_socket_path};

const PROGRESS_ACK_TIMEOUT: Duration = Duration::from_secs(5);
const PROGRESS_RECONNECT_DELAY: Duration = Duration::from_millis(500);
const PROGRESS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Delay between control-socket status polls in [`await_install_result`].
///
/// SWUpdate closes the control socket after each `GET_STATUS` reply, so every
/// poll must reopen it. This interval keeps that overhead bounded instead of
/// reopening the socket back-to-back.
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(250);

async fn connect_ctrl() -> Result<UnixStream> {
    Ok(UnixStream::connect(ctrl_socket_path()).await?)
}

async fn write_message(stream: &mut UnixStream, msg: &IpcMessage) -> Result<()> {
    stream.write_all(msg.as_bytes()).await?;
    Ok(())
}

async fn read_message(stream: &mut UnixStream) -> Result<IpcMessage> {
    let mut msg = IpcMessage::zeroed();
    read_exact(stream, msg.as_bytes_mut()).await?;
    Ok(msg)
}

async fn read_exact(stream: &mut UnixStream, buf: &mut [u8]) -> Result<()> {
    match stream.read_exact(buf).await {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(Error::Closed),
        Err(e) => Err(Error::from(e)),
    }
}

/// An open async install connection. Image data is streamed by writing to it.
#[derive(Debug)]
pub struct InstallConn {
    stream: UnixStream,
}

impl InstallConn {
    /// Sends a chunk of image data.
    pub async fn send_data(&mut self, buf: &[u8]) -> Result<()> {
        self.stream.write_all(buf).await?;
        Ok(())
    }

    /// Closes the connection.
    pub async fn end(mut self) -> Result<()> {
        self.stream.shutdown().await?;
        Ok(())
    }

    /// Consumes the connection and returns the raw stream for direct streaming.
    pub fn into_stream(self) -> UnixStream {
        self.stream
    }

    /// Streams an entire async source into SWUpdate, then closes
    /// the connection.
    ///
    /// The transfer runs through a single [`tokio::io::copy`] loop, so the
    /// caller hands off the whole image in one call instead of writing it block
    /// by block. This is the most efficient path when the firmware is already
    /// available as an [`AsyncRead`](tokio::io::AsyncRead) (e.g. a file).
    pub async fn send_from<R>(mut self, mut src: R) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        tokio::io::copy(&mut src, &mut self.stream).await?;
        self.end().await
    }
}

impl AsyncWrite for InstallConn {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

/// Starts an install with an explicit request. On success the returned
/// connection is ready to receive the image stream.
pub async fn inst_start_ext(req: &SwupdateRequest) -> Result<InstallConn> {
    let mut stream = connect_ctrl().await?;
    let mut msg = IpcMessage::new(MsgType::ReqInstall);
    msg.data.instmsg.req = *req;
    write_message(&mut stream, &msg).await?;
    let reply = read_message(&mut stream).await?;
    if reply.type_ != MsgType::Ack as i32 {
        return Err(Error::Nack);
    }
    Ok(InstallConn { stream })
}

/// Starts an install with default request values.
pub async fn inst_start() -> Result<InstallConn> {
    inst_start_ext(&SwupdateRequest::prepare()).await
}

/// Queries the current installer status.
pub async fn get_status() -> Result<IpcMessage> {
    let mut stream = connect_ctrl().await?;
    let request = IpcMessage::new(MsgType::GetStatus);
    write_message(&mut stream, &request).await?;
    read_message(&mut stream).await
}

/// Queries the installer status with a receive timeout. Returns `Ok(None)` when
/// the timeout elapses.
pub async fn get_status_timeout(duration: Duration) -> Result<Option<IpcMessage>> {
    let mut stream = connect_ctrl().await?;
    let request = IpcMessage::new(MsgType::GetStatus);
    write_message(&mut stream, &request).await?;
    match timeout(duration, read_message(&mut stream)).await {
        Ok(result) => result.map(Some),
        Err(_) => Ok(None),
    }
}

/// Queries the installer status with a receive timeout and decodes the current
/// and last-result recovery states.
pub async fn get_status_values_timeout(
    duration: Duration,
) -> Result<Option<(Option<crate::RecoveryStatus>, Option<crate::RecoveryStatus>)>> {
    let Some(msg) = get_status_timeout(duration).await? else {
        return Ok(None);
    };

    // SAFETY: get_status replies always use the `status` union member.
    let (current_raw, last_result_raw) = unsafe {
        (msg.data.status.current, msg.data.status.last_result)
    };

    Ok(Some((
        crate::RecoveryStatus::try_from(current_raw).ok(),
        crate::RecoveryStatus::try_from(last_result_raw).ok(),
    )))
}

/// Waits for a terminal SWUpdate install result within `timeout`, using the
/// control status socket.
///
/// Returns `Ok(())` once SWUpdate has reported success, `Err(Error::InstallFailed)`
/// on terminal failure, and
/// `Err(Error::Timeout)` if the deadline elapses.
pub async fn await_install_result(timeout: Duration) -> Result<()> {
    use crate::RecoveryStatus;

    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(Error::Timeout);
        }

        let msg = match get_status_timeout(STATUS_POLL_INTERVAL).await {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                sleep(STATUS_POLL_INTERVAL).await;
                continue;
            }
            Err(Error::Closed) => {
                sleep(STATUS_POLL_INTERVAL).await;
                continue;
            }
            Err(err) => return Err(err),
        };

        // SAFETY: get_status replies always use the `status` union member.
        let (current_raw, last_result_raw) = unsafe {
            (msg.data.status.current, msg.data.status.last_result)
        };

        // Both fields are the SAME enum (RECOVERY_STATUS) but carry DIFFERENT
        // information, and getting this wrong has broken install detection
        // repeatedly. From SWUpdate's GET_STATUS handler (core/network_thread.c):
        //
        //   msg.data.status.current     = instp->status;
        //   msg.data.status.last_result = instp->last_install;
        //   if (a notification is queued)
        //       msg.data.status.current = notification->status;  // OVERWRITTEN
        //
        // * `current` is the installer's live progress phase, overwritten by
        //   whatever notification is drained from the queue. In practice it only
        //   reports non-terminal phases (START / RUN / DOWNLOAD / SUBPROCESS /
        //   PROGRESS); it does NOT carry the terminal verdict.
        // * `last_result` is `instp->last_install`, the authoritative terminal
        //   result: SUCCESS or FAILURE once terminal, IDLE / PROGRESS while
        //   still running.
        //
        // Rules:
        // * Failure is terminal: report it when `last_result` is FAILURE.
        // * Success is only trusted once `current` shows the install is actively
        //   running (RUN) AND `last_result` is SUCCESS, so a stale result from a
        //   previous install is never mistaken for this one.
        let current = RecoveryStatus::try_from(current_raw).ok();
        let last_result_now = RecoveryStatus::try_from(last_result_raw).ok();

        if last_result_now == Some(RecoveryStatus::Failure) {
            return Err(Error::InstallFailed);
        }

        if current == Some(RecoveryStatus::Run) && last_result_now == Some(RecoveryStatus::Success) {
            return Ok(());
        }
    }
}

/// Runs a post-update action and returns the daemon's reply frame.
pub async fn postupdate(info: &[u8]) -> Result<IpcMessage> {
    let mut stream = connect_ctrl().await?;
    let mut msg = IpcMessage::new(MsgType::PostUpdate);
    unsafe {
        let len = info.len().min(msg.data.procmsg.buf.len());
        for (slot, &byte) in msg.data.procmsg.buf.iter_mut().zip(info.iter()).take(len) {
            *slot = byte as std::ffi::c_char;
        }
        msg.data.procmsg.len = len as u32;
    }
    write_message(&mut stream, &msg).await?;
    read_message(&mut stream).await
}

/// Sends a command to a SWUpdate subprocess. The caller fills `msg.type_` and
/// the payload; the reply is written back into `msg`.
pub async fn send_cmd(msg: &mut IpcMessage) -> Result<()> {
    let mut stream = connect_ctrl().await?;
    msg.magic = IPC_MAGIC;
    write_message(&mut stream, msg).await?;
    *msg = read_message(&mut stream).await?;
    Ok(())
}

/// Sets the AES decryption key via IPC. The key must be 64 ASCII characters and
/// the IV 32 ASCII characters.
pub async fn set_aes(key: &str, ivt: &str) -> Result<()> {
    if key.len() != 64 || ivt.len() != 32 {
        return Err(Error::InvalidArgument("AES key must be 64 chars and IV 32 chars"));
    }
    let mut msg = IpcMessage::new(MsgType::SetAesKey);
    unsafe {
        write_c_string(&mut msg.data.aeskeymsg.key_ascii, key);
        write_c_string(&mut msg.data.aeskeymsg.ivt_ascii, ivt);
    }
    send_cmd(&mut msg).await
}

/// Sets the accepted version range via IPC.
pub async fn set_version_range(
    min_version: Option<&str>,
    max_version: Option<&str>,
    current_version: Option<&str>,
) -> Result<()> {
    let mut msg = IpcMessage::new(MsgType::SetVersionsRange);
    unsafe {
        if let Some(v) = min_version {
            write_c_string(&mut msg.data.versions.minimum_version, v);
        }
        if let Some(v) = max_version {
            write_c_string(&mut msg.data.versions.maximum_version, v);
        }
        if let Some(v) = current_version {
            write_c_string(&mut msg.data.versions.current_version, v);
        }
    }
    send_cmd(&mut msg).await
}

/// An open async notification stream.
#[derive(Debug)]
pub struct NotifyConn {
    stream: UnixStream,
}

impl NotifyConn {
    /// Reads the next notification frame, validating its magic number.
    pub async fn receive(&mut self) -> Result<IpcMessage> {
        let msg = read_message(&mut self.stream).await?;
        if msg.magic != IPC_MAGIC {
            return Err(Error::InvalidMagic(msg.magic));
        }
        Ok(msg)
    }
}

/// Opens an async notification stream.
pub async fn notify_connect() -> Result<NotifyConn> {
    let mut stream = connect_ctrl().await?;
    let request = IpcMessage::new(MsgType::NotifyStream);
    write_message(&mut stream, &request).await?;
    let reply = read_message(&mut stream).await?;
    if reply.type_ != MsgType::Ack as i32 {
        return Err(Error::UnexpectedType(reply.type_));
    }
    Ok(NotifyConn { stream })
}

/// An open async progress connection.
#[derive(Debug)]
pub struct ProgressConn {
    stream: UnixStream,
}

impl ProgressConn {
    /// Awaits the next progress frame.
    pub async fn receive(&mut self) -> Result<ProgressMsg> {
        let mut msg = ProgressMsg::zeroed();
        read_exact(&mut self.stream, msg.as_bytes_mut()).await?;
        Ok(msg)
    }
}

async fn progress_connect_path(path: &Path, reconnect: bool) -> Result<ProgressConn> {
    let deadline = tokio::time::Instant::now() + PROGRESS_CONNECT_TIMEOUT;
    let mut stream = loop {
        match UnixStream::connect(path).await {
            Ok(stream) => break stream,
            Err(_) if reconnect => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(Error::ProgressConnectTimeout);
                }
                sleep(PROGRESS_RECONNECT_DELAY).await;
                continue;
            }
            Err(e) => return Err(Error::from(e)),
        }
    };
    wait_for_progress_ack(&mut stream).await?;
    Ok(ProgressConn { stream })
}

async fn wait_for_progress_ack(stream: &mut UnixStream) -> Result<()> {
    let mut ack = ProgressConnectAck::zeroed();
    match timeout(PROGRESS_ACK_TIMEOUT, read_exact(stream, ack.as_bytes_mut())).await {
        Ok(result) => result?,
        Err(_) => return Err(Error::InvalidProgressAck),
    }
    if !ack.is_major_compatible() {
        return Err(Error::IncompatibleProgressVersion(ack.apiversion));
    }
    if !ack.has_valid_magic() {
        return Err(Error::InvalidProgressAck);
    }
    Ok(())
}

/// Connects to the progress interface using the default socket. When
/// `reconnect` is `true`, connection attempts are retried up to
/// `PROGRESS_CONNECT_TIMEOUT` before returning `Error::ProgressConnectTimeout`.
pub async fn progress_connect(reconnect: bool) -> Result<ProgressConn> {
    progress_connect_path(&progress_socket_path(), reconnect).await
}

/// Connects to the progress interface using an explicit socket path.
pub async fn progress_connect_with_path(path: impl AsRef<Path>, reconnect: bool) -> Result<ProgressConn> {
    progress_connect_path(path.as_ref(), reconnect).await
}

/// Returns a [`Stream`] of progress frames that reconnects automatically on
/// disconnect or connect failure when `reconnect` is `true`. When `reconnect`
/// is `false`, the stream terminates on the first connect error.
///
/// API version compatibility is verified at connect time via the
/// `progress_connect_ack` handshake; per-frame version checks are not needed.
pub fn progress_stream(reconnect: bool) -> impl Stream<Item = ProgressMsg> {
    futures_util::stream::unfold(None::<ProgressConn>, move |mut conn| async move {
        loop {
            if conn.is_none() {
                match progress_connect(reconnect).await {
                    Ok(c) => conn = Some(c),
                    Err(_) => return None,
                }
            }
            match conn.as_mut() {
                Some(active) => match active.receive().await {
                    Ok(msg) => return Some((msg, conn)),
                    Err(_) => conn = None,
                },
                None => continue,
            }
        }
    })
}
