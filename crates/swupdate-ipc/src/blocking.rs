//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
// Copyright (C) 2026 Ezurio LLC.
//
//! Blocking (synchronous) SWUpdate IPC client built on `std::os::unix::net`.
//!
//! This is a pure-Rust reimplementation of the SWUpdate client library
//! (`network_ipc.c`, `network_ipc-if.c`, and `progress_ipc.c`). No C library is
//! linked; the client speaks the IPC protocol directly over Unix sockets.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::proto::{
    IpcMessage, MsgType, ProgressConnectAck, ProgressMsg, RecoveryStatus, SwupdateRequest, write_c_string,
};
use crate::socket::{ctrl_socket_path, progress_socket_path};

const PROGRESS_ACK_TIMEOUT: Duration = Duration::from_secs(5);
const PROGRESS_RECONNECT_DELAY: Duration = Duration::from_millis(500);
const PROGRESS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Delay between control-socket status polls in [`await_install_result`].
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(250);

fn connect_ctrl() -> Result<UnixStream> {
    Ok(UnixStream::connect(ctrl_socket_path())?)
}

fn write_message(stream: &mut UnixStream, msg: &IpcMessage) -> Result<()> {
    stream.write_all(msg.as_bytes())?;
    Ok(())
}

fn read_message(stream: &mut UnixStream) -> Result<IpcMessage> {
    let mut msg = IpcMessage::zeroed();
    stream.read_exact(msg.as_bytes_mut())?;
    Ok(msg)
}

/// An open install connection. Image data is streamed by writing to it; the
/// connection is closed when dropped or via [`InstallConn::end`].
#[derive(Debug)]
pub struct InstallConn {
    stream: UnixStream,
}

impl InstallConn {
    /// Sends a chunk of image data, equivalent to `ipc_send_data`.
    pub fn send_data(&mut self, buf: &[u8]) -> Result<()> {
        self.stream.write_all(buf)?;
        Ok(())
    }

    /// Closes the connection, equivalent to `ipc_end`.
    pub fn end(self) {}
}

impl Write for InstallConn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stream.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

/// Starts an install with an explicit request, equivalent to
/// `ipc_inst_start_ext`. On success the returned connection is ready to receive
/// the image stream.
pub fn inst_start_ext(req: &SwupdateRequest) -> Result<InstallConn> {
    let mut stream = connect_ctrl()?;
    let mut msg = IpcMessage::new(MsgType::ReqInstall);
    msg.data.instmsg.req = *req;
    write_message(&mut stream, &msg)?;
    let reply = read_message(&mut stream)?;
    if reply.type_ != MsgType::Ack as i32 {
        return Err(Error::Nack);
    }
    Ok(InstallConn { stream })
}

/// Starts an install with default request values, equivalent to
/// `ipc_inst_start`.
pub fn inst_start() -> Result<InstallConn> {
    inst_start_ext(&SwupdateRequest::prepare())
}

/// Queries the current installer status, equivalent to `ipc_get_status`.
pub fn get_status() -> Result<IpcMessage> {
    let mut stream = connect_ctrl()?;
    let request = IpcMessage::new(MsgType::GetStatus);
    write_message(&mut stream, &request)?;
    read_message(&mut stream)
}

/// Queries the installer status with a receive timeout, equivalent to
/// `ipc_get_status_timeout`. Returns `Ok(None)` when the timeout elapses.
pub fn get_status_timeout(timeout: Duration) -> Result<Option<IpcMessage>> {
    let mut stream = connect_ctrl()?;
    let request = IpcMessage::new(MsgType::GetStatus);
    write_message(&mut stream, &request)?;
    stream.set_read_timeout(Some(timeout))?;
    match read_message(&mut stream) {
        Ok(msg) => Ok(Some(msg)),
        Err(Error::Io(e))
            if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Waits for a terminal SWUpdate install result within `timeout`, using the
/// control status socket.
///
/// Returns `Ok(())` on success once SWUpdate is no longer in an active install
/// state and `last_result=Success`, `Err(Error::InstallFailed)` on failure,
/// and `Err(Error::Timeout)` if the deadline elapses.
pub fn await_install_result(timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(Error::Timeout);
        }

        let poll_timeout = deadline.saturating_duration_since(now).min(STATUS_POLL_INTERVAL);

        let msg = match get_status_timeout(poll_timeout)? {
            Some(msg) => msg,
            None => {
                thread::sleep(STATUS_POLL_INTERVAL);
                continue;
            }
        };

        // SAFETY: get_status replies always use the `status` union member.
        let (current_raw, last_result_raw) = unsafe {
            (msg.data.status.current, msg.data.status.last_result)
        };

        let current = RecoveryStatus::try_from(current_raw).ok();
        let last_result = RecoveryStatus::try_from(last_result_raw).ok();

        let active = matches!(
            current,
            Some(RecoveryStatus::Start)
                | Some(RecoveryStatus::Run)
                | Some(RecoveryStatus::Download)
                | Some(RecoveryStatus::Progress)
                | Some(RecoveryStatus::Subprocess)
        );

        if !active {
            match last_result {
                Some(RecoveryStatus::Success) => return Ok(()),
                Some(RecoveryStatus::Failure) => return Err(Error::InstallFailed),
                _ => {}
            }
        }

    }
}

/// Requests a system restart through SWUpdate (`SWUPDATE_SYSRESTART`).
pub fn sysrestart() -> Result<()> {
    let mut stream = connect_ctrl()?;
    let msg = IpcMessage::new(MsgType::SysRestart);
    write_message(&mut stream, &msg)?;
    Ok(())
}

/// Runs a post-update action, equivalent to `ipc_postupdate`. The optional
/// `info` payload is forwarded in the `procmsg` buffer. Returns the daemon's
/// reply frame.
pub fn postupdate(info: &[u8]) -> Result<IpcMessage> {
    let mut stream = connect_ctrl()?;
    let mut msg = IpcMessage::new(MsgType::PostUpdate);
    unsafe {
        let len = info.len().min(msg.data.procmsg.buf.len());
        for (slot, &byte) in msg.data.procmsg.buf.iter_mut().zip(info.iter()).take(len) {
            *slot = byte as libc::c_char;
        }
        msg.data.procmsg.len = len as u32;
    }
    write_message(&mut stream, &msg)?;
    read_message(&mut stream)
}

/// Sends a command to a SWUpdate subprocess, equivalent to `ipc_send_cmd`. The
/// caller fills `msg.type_` and the relevant payload; the reply is written back
/// into `msg`.
pub fn send_cmd(msg: &mut IpcMessage) -> Result<()> {
    let mut stream = connect_ctrl()?;
    msg.magic = crate::proto::IPC_MAGIC;
    write_message(&mut stream, msg)?;
    *msg = read_message(&mut stream)?;
    Ok(())
}

/// Sets the AES decryption key via IPC, equivalent to `swupdate_set_aes`. The
/// key must be 64 ASCII characters and the IV 32 ASCII characters.
pub fn set_aes(key: &str, ivt: &str) -> Result<()> {
    if key.len() != 64 || ivt.len() != 32 {
        return Err(Error::InvalidArgument("AES key must be 64 chars and IV 32 chars"));
    }
    let mut msg = IpcMessage::new(MsgType::SetAesKey);
    unsafe {
        write_c_string(&mut msg.data.aeskeymsg.key_ascii, key);
        write_c_string(&mut msg.data.aeskeymsg.ivt_ascii, ivt);
    }
    send_cmd(&mut msg)
}

/// Sets the accepted version range via IPC, equivalent to
/// `swupdate_set_version_range`.
pub fn set_version_range(
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
    send_cmd(&mut msg)
}

/// An open notification stream, equivalent to a connected `ipc_notify_*` fd.
#[derive(Debug)]
pub struct NotifyConn {
    stream: UnixStream,
}

impl NotifyConn {
    /// Reads the next notification frame, equivalent to `ipc_notify_receive`.
    /// Returns `Ok(None)` if the read was interrupted and should be retried.
    pub fn receive(&mut self) -> Result<Option<IpcMessage>> {
        let mut msg = IpcMessage::zeroed();
        match self.stream.read_exact(msg.as_bytes_mut()) {
            Ok(()) => {
                if msg.magic != crate::proto::IPC_MAGIC {
                    return Err(Error::InvalidMagic(msg.magic));
                }
                Ok(Some(msg))
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => {
                Ok(None)
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(Error::Closed),
            Err(e) => Err(Error::from(e)),
        }
    }

    /// Sets the underlying stream to non-blocking mode.
    pub fn set_nonblocking(&self, nonblocking: bool) -> Result<()> {
        self.stream.set_nonblocking(nonblocking)?;
        Ok(())
    }
}

/// Opens a notification stream, equivalent to `ipc_notify_connect`.
pub fn notify_connect() -> Result<NotifyConn> {
    let mut stream = connect_ctrl()?;
    let request = IpcMessage::new(MsgType::NotifyStream);
    write_message(&mut stream, &request)?;
    let reply = read_message(&mut stream)?;
    if reply.type_ != MsgType::Ack as i32 {
        return Err(Error::UnexpectedType(reply.type_));
    }
    Ok(NotifyConn { stream })
}

/// An open progress connection, equivalent to a connected progress fd.
#[derive(Debug)]
pub struct ProgressConn {
    stream: UnixStream,
}

impl ProgressConn {
    /// Blocks until a progress frame arrives, equivalent to
    /// `progress_ipc_receive`.
    pub fn receive(&mut self) -> Result<ProgressMsg> {
        self.stream.set_nonblocking(false)?;
        let mut msg = ProgressMsg::zeroed();
        match self.stream.read_exact(msg.as_bytes_mut()) {
            Ok(()) => Ok(msg),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(Error::Closed),
            Err(e) => Err(Error::from(e)),
        }
    }

    /// Returns the next progress frame if one is immediately available,
    /// equivalent to `progress_ipc_receive_nb`. Returns `Ok(None)` when no frame
    /// is pending.
    pub fn receive_nb(&mut self) -> Result<Option<ProgressMsg>> {
        self.stream.set_nonblocking(true)?;
        let mut msg = ProgressMsg::zeroed();
        match self.stream.read_exact(msg.as_bytes_mut()) {
            Ok(()) => Ok(Some(msg)),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => {
                Ok(None)
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(Error::Closed),
            Err(e) => Err(Error::from(e)),
        }
    }
}

fn progress_connect_path(path: &Path, reconnect: bool) -> Result<ProgressConn> {
    let deadline = Instant::now() + PROGRESS_CONNECT_TIMEOUT;
    let stream = loop {
        match UnixStream::connect(path) {
            Ok(stream) => break stream,
            Err(_) if reconnect => {
                if Instant::now() >= deadline {
                    return Err(Error::ProgressConnectTimeout);
                }
                thread::sleep(PROGRESS_RECONNECT_DELAY);
                continue;
            }
            Err(e) => return Err(Error::from(e)),
        }
    };
    wait_for_progress_ack(&stream)?;
    Ok(ProgressConn { stream })
}

fn wait_for_progress_ack(stream: &UnixStream) -> Result<()> {
    let mut ack_stream = stream.try_clone()?;
    ack_stream.set_read_timeout(Some(PROGRESS_ACK_TIMEOUT))?;
    let mut ack = ProgressConnectAck::zeroed();
    ack_stream.read_exact(ack.as_bytes_mut())?;
    if !ack.is_major_compatible() {
        return Err(Error::IncompatibleProgressVersion(ack.apiversion));
    }
    if !ack.has_valid_magic() {
        return Err(Error::InvalidProgressAck);
    }
    stream.set_read_timeout(None)?;
    Ok(())
}

/// Connects to the progress interface using the default socket, equivalent to
/// `progress_ipc_connect`. When `reconnect` is `true`, connection attempts are
/// retried up to `PROGRESS_CONNECT_TIMEOUT` before returning
/// `Error::ProgressConnectTimeout`.
pub fn progress_connect(reconnect: bool) -> Result<ProgressConn> {
    progress_connect_path(&progress_socket_path(), reconnect)
}

/// Connects to the progress interface using an explicit socket path,
/// equivalent to `progress_ipc_connect_with_path`.
pub fn progress_connect_with_path(path: impl AsRef<Path>, reconnect: bool) -> Result<ProgressConn> {
    progress_connect_path(path.as_ref(), reconnect)
}

/// Outcome of an asynchronous install driven by [`async_start`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncOutcome {
    /// The installation succeeded.
    Success,
    /// The installation failed.
    Failure,
}

/// Drives an install on a background thread, mirroring the C
/// `swupdate_async_start` helper.
///
/// * `write_data` is called repeatedly to obtain image chunks; returning an
///   empty slice (`None`) signals end of stream.
/// * `status` is invoked for each distinct status frame collected after the
///   image was sent (pass `None` to skip).
/// * `terminated` is invoked once with the final outcome.
///
/// The returned [`JoinHandle`] resolves to the final outcome.
pub fn async_start<W, S, T>(
    req: &SwupdateRequest,
    mut write_data: W,
    mut status: Option<S>,
    terminated: T,
) -> Result<JoinHandle<AsyncOutcome>>
where
    W: FnMut() -> Option<Vec<u8>> + Send + 'static,
    S: FnMut(&IpcMessage) + Send + 'static,
    T: FnOnce(AsyncOutcome) + Send + 'static,
{
    let mut conn = inst_start_ext(req)?;

    let handle = thread::spawn(move || {
        let outcome = run_async_install(&mut conn, &mut write_data);
        conn.end();

        if let Some(status_cb) = status.as_mut() {
            unstack_status(status_cb);
        }

        terminated(outcome);
        outcome
    });

    Ok(handle)
}

fn run_async_install<W>(conn: &mut InstallConn, write_data: &mut W) -> AsyncOutcome
where
    W: FnMut() -> Option<Vec<u8>>,
{
    // Connect to progress before streaming so the final result is not missed.
    let mut progress = match progress_connect(false) {
        Ok(progress) => progress,
        Err(_) => return AsyncOutcome::Failure,
    };

    let mut early: Option<AsyncOutcome> = None;
    loop {
        let chunk = match write_data() {
            Some(chunk) if !chunk.is_empty() => chunk,
            _ => break,
        };
        if conn.send_data(&chunk).is_err() {
            early = Some(AsyncOutcome::Failure);
            break;
        }
        match consume_progress(&mut progress) {
            Ok(Some(outcome)) => {
                early = Some(outcome);
                break;
            }
            Ok(None) => {}
            Err(_) => {
                early = Some(AsyncOutcome::Failure);
                break;
            }
        }
    }

    if let Some(outcome) = early {
        return outcome;
    }

    wait_for_complete(&mut progress)
}

/// Drains any pending progress frames, returning a terminal outcome if reached.
fn consume_progress(progress: &mut ProgressConn) -> Result<Option<AsyncOutcome>> {
    loop {
        match progress.receive_nb()? {
            None => return Ok(None),
            Some(msg) => match msg.status() {
                Ok(RecoveryStatus::Success) => return Ok(Some(AsyncOutcome::Success)),
                Ok(RecoveryStatus::Failure) => return Ok(Some(AsyncOutcome::Failure)),
                _ => continue,
            },
        }
    }
}

/// Blocks reading progress frames until a terminal status is reported.
fn wait_for_complete(progress: &mut ProgressConn) -> AsyncOutcome {
    loop {
        match progress.receive() {
            Ok(msg) => match msg.status() {
                Ok(RecoveryStatus::Success) => return AsyncOutcome::Success,
                Ok(RecoveryStatus::Failure) => return AsyncOutcome::Failure,
                _ => continue,
            },
            Err(_) => return AsyncOutcome::Failure,
        }
    }
}

/// Polls `get_status` until the installer returns to `IDLE`, invoking `callback`
/// on each distinct status. Mirrors the legacy status-draining loop.
fn unstack_status<S>(callback: &mut S)
where
    S: FnMut(&IpcMessage),
{
    let mut previous: Option<i32> = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    while let Ok(msg) = get_status() {
        let (current, desc_len) = unsafe {
            let status = &msg.data.status;
            let desc_len = status.desc.iter().position(|&c| c == 0).unwrap_or(status.desc.len());
            (status.current, desc_len)
        };
        if previous != Some(current) || desc_len > 0 {
            callback(&msg);
        }
        previous = Some(current);
        if current == RecoveryStatus::Idle as i32 || Instant::now() >= deadline {
            break;
        }
    }
}
