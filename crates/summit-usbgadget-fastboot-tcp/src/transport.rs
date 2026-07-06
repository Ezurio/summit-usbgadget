//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! AOSP fastboot "TCP Protocol v1" framing.
//!
//! On connect both peers send a 4-byte `FB<version>` handshake; the negotiated
//! version is the minimum of the two and must be at least 1. Afterwards every
//! fastboot packet — commands, replies, and bulk download data — is framed as an
//! 8-byte big-endian length prefix followed by that many bytes.

use std::io::{self, ErrorKind};
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

/// Handshake we send: fastboot TCP protocol version 1.
const HANDSHAKE: &[u8; 4] = b"FB01";
/// Highest protocol version this device speaks.
const PROTOCOL_VERSION: u32 = 1;
/// Reject command frames larger than this; per the spec a command is a single
/// ASCII packet no greater than 4096 bytes. Data-phase frames are read through
/// [`FastbootFraming::read_header`] and are not bound by this limit.
const MAX_COMMAND_LEN: u64 = 4096;

/// Length-prefixed fastboot framing over an async byte stream.
pub(crate) struct FastbootFraming<S> {
    stream: S,
    /// Idle timeout applied to each read; `None` waits indefinitely.
    inactivity_timeout: Option<Duration>,
}

/// Applies the connection inactivity timeout to a single read future, mapping a
/// lapse into a `TimedOut` error so the session drops the idle connection.
async fn read_guard<F, T>(inactivity_timeout: Option<Duration>, fut: F) -> io::Result<T>
where
    F: std::future::Future<Output = io::Result<T>>,
{
    match inactivity_timeout {
        Some(limit) => timeout(limit, fut)
            .await
            .map_err(|_| io::Error::new(ErrorKind::TimedOut, "fastboot-tcp connection inactive"))?,
        None => fut.await,
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> FastbootFraming<S> {
    pub(crate) fn new(stream: S, inactivity_timeout: Option<Duration>) -> Self {
        Self { stream, inactivity_timeout }
    }

    /// Performs the mutual `FB01` handshake, returning an error (so the caller
    /// drops the connection) on any malformed or too-old peer handshake.
    pub(crate) async fn handshake(&mut self) -> io::Result<()> {
        self.stream.write_all(HANDSHAKE).await?;
        self.stream.flush().await?;

        let mut buf = [0u8; 4];
        let _ = read_guard(self.inactivity_timeout, self.stream.read_exact(&mut buf)).await?;
        if &buf[..2] != b"FB" {
            return Err(io::Error::new(ErrorKind::InvalidData, "malformed fastboot handshake"));
        }
        let version = std::str::from_utf8(&buf[2..])
            .ok()
            .and_then(|digits| digits.parse::<u32>().ok())
            .ok_or_else(|| {
                io::Error::new(ErrorKind::InvalidData, "malformed fastboot handshake version")
            })?;
        // The negotiated version is min(peer, ours); we only speak v1, so a peer
        // that cannot speak at least v1 is unusable.
        if version < PROTOCOL_VERSION {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("unsupported fastboot protocol version {version}"),
            ));
        }
        Ok(())
    }

    /// Writes one fastboot packet as `[len: u64 BE][data]`.
    pub(crate) async fn send_packet(&mut self, data: &[u8]) -> io::Result<()> {
        self.stream.write_all(&(data.len() as u64).to_be_bytes()).await?;
        self.stream.write_all(data).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Reads one complete command packet. Returns `Ok(None)` on a clean
    /// disconnect at a frame boundary.
    pub(crate) async fn recv_command(&mut self) -> io::Result<Option<BytesMut>> {
        let Some(len) = self.read_header().await? else {
            return Ok(None);
        };
        if len > MAX_COMMAND_LEN {
            return Err(io::Error::new(ErrorKind::InvalidData, "fastboot command frame too large"));
        }
        let mut buf = BytesMut::zeroed(len as usize);
        let _ = read_guard(self.inactivity_timeout, self.stream.read_exact(&mut buf)).await?;
        Ok(Some(buf))
    }

    /// Reads the 8-byte big-endian length prefix of the next frame. Returns
    /// `Ok(None)` when the peer closed cleanly before any header byte arrived.
    pub(crate) async fn read_header(&mut self) -> io::Result<Option<u64>> {
        let mut buf = [0u8; 8];
        let mut filled = 0;
        while filled < buf.len() {
            let read = read_guard(self.inactivity_timeout, self.stream.read(&mut buf[filled..])).await?;
            if read == 0 {
                if filled == 0 {
                    return Ok(None);
                }
                return Err(io::Error::new(ErrorKind::UnexpectedEof, "truncated fastboot frame header"));
            }
            filled += read;
        }
        Ok(Some(u64::from_be_bytes(buf)))
    }

    /// Reads up to `buf.len()` payload bytes of the current data-phase frame.
    /// A single read may return fewer bytes; callers loop until the frame and
    /// download totals are satisfied.
    pub(crate) async fn read_into(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_guard(self.inactivity_timeout, self.stream.read(buf)).await
    }
}
