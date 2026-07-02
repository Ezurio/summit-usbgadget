//
// SPDX-License-Identifier: MIT OR Apache-2.0
//

use std::collections::VecDeque;
use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::task::Poll;

use bytes::BytesMut;
use tokio::io::{AsyncWrite, AsyncWriteExt};

async fn write_once<W>(writer: &mut W, buf: &[u8], wait: bool) -> io::Result<usize>
where
    W: AsyncWrite + Unpin,
{
    if wait {
        poll_fn(|cx| Pin::new(&mut *writer).poll_write(cx, buf)).await
    } else {
        poll_fn(|cx| match Pin::new(&mut *writer).poll_write(cx, buf) {
            Poll::Ready(result) => Poll::Ready(result),
            Poll::Pending => Poll::Ready(Err(io::Error::new(io::ErrorKind::WouldBlock, "writer not ready"))),
        })
        .await
    }
}

pub(super) struct QueuedWriter<W> {
    writer: W,
    pending: Vec<u8>,
    pending_offset: usize,
    queued_blocks: VecDeque<BytesMut>,
    max_queued_bytes: usize,
}

impl<W> QueuedWriter<W> {
    pub(super) fn new(writer: W, max_queued_bytes: usize) -> Self {
        Self {
            writer,
            pending: Vec::new(),
            pending_offset: 0,
            queued_blocks: VecDeque::new(),
            max_queued_bytes,
        }
    }

    pub(super) fn into_inner(self) -> W {
        self.writer
    }

    fn pending_len(&self) -> usize {
        self.pending.len().saturating_sub(self.pending_offset)
    }

    fn pending_is_busy(&self) -> bool {
        self.pending_offset < self.pending.len()
    }

    pub(super) fn queued_bytes(&self) -> usize {
        self.pending_len() + self.queued_blocks.iter().map(BytesMut::len).sum::<usize>()
    }

    pub(super) fn queued_block_count(&self) -> usize {
        self.queued_blocks.len()
    }

    pub(super) fn is_busy(&self) -> bool {
        self.pending_is_busy() || !self.queued_blocks.is_empty()
    }

    pub(super) fn diagnostics(&self) -> (bool, usize, usize) {
        (self.is_busy(), self.queued_block_count(), self.queued_bytes())
    }

    pub(super) fn should_throttle(&self, reserve_bytes: usize) -> bool {
        self.queued_bytes().saturating_add(reserve_bytes) >= self.max_queued_bytes
    }

    pub(super) fn log_diagnostics(&self, label: &str) {
        let (writer_busy, queued_blocks, queued_bytes) = self.diagnostics();
        log::warn!(
            "{label}: writer_busy={} queued_blocks={} queued_bytes={}",
            writer_busy,
            queued_blocks,
            queued_bytes
        );
    }

    pub(super) fn log_diagnostics_or_default(writer: Option<&Self>, label: &str) {
        if let Some(writer) = writer {
            writer.log_diagnostics(label);
        } else {
            log::warn!("{label}: writer_busy=false queued_blocks=0 queued_bytes=0");
        }
    }
}

impl<W> QueuedWriter<W>
where
    W: AsyncWrite + Unpin,
{
    async fn advance_pending(&mut self, wait: bool) -> io::Result<()> {
        while self.pending_is_busy() {
            match write_once(&mut self.writer, &self.pending[self.pending_offset..], wait).await {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "writer closed")),
                Ok(n) => {
                    self.pending_offset += n;
                }
                Err(err) if !wait && err.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(err) => return Err(err),
            }
        }

        self.pending.clear();
        self.pending_offset = 0;
        Ok(())
    }

    async fn write_direct_or_pending(&mut self, data: &[u8]) -> io::Result<()> {
        self.advance_pending(false).await?;
        if self.pending_is_busy() {
            return Ok(());
        }

        match write_once(&mut self.writer, data, false).await {
            Ok(n) if n == data.len() => Ok(()),
            Ok(n) => {
                self.pending.extend_from_slice(&data[n..]);
                self.pending_offset = 0;
                Ok(())
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                self.pending.extend_from_slice(data);
                self.pending_offset = 0;
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    async fn submit_front_queued_block<F>(&mut self, map_err: F) -> io::Result<bool>
    where
        F: Fn(io::Error) -> io::Error + Copy,
    {
        let Some(block) = self.queued_blocks.front().cloned() else {
            return Ok(false);
        };

        self.write_direct_or_pending(&block).await.map_err(map_err)?;
        let _ = self.queued_blocks.pop_front();
        Ok(true)
    }

    async fn drain_queued_blocks<F>(&mut self, map_err: F) -> io::Result<()>
    where
        F: Fn(io::Error) -> io::Error + Copy,
    {
        while !self.pending_is_busy() {
            if !self.submit_front_queued_block(map_err).await? {
                break;
            }
        }
        Ok(())
    }

    pub(super) async fn write_block<F>(&mut self, data: BytesMut, map_err: F) -> io::Result<usize>
    where
        F: Fn(io::Error) -> io::Error + Copy,
    {
        let data_len = data.len();

        self.poll_progress(map_err).await?;
        if self.queued_bytes() + data_len > self.max_queued_bytes {
            return Err(map_err(io::Error::new(io::ErrorKind::WouldBlock, "writer queue full")));
        }

        self.queued_blocks.push_back(data);
        self.poll_progress(map_err).await?;
        Ok(data_len)
    }

    pub(super) async fn poll_progress<F>(&mut self, map_err: F) -> io::Result<()>
    where
        F: Fn(io::Error) -> io::Error + Copy,
    {
        self.advance_pending(false).await.map_err(map_err)?;
        self.drain_queued_blocks(map_err).await
    }

    pub(super) async fn wait_pending(&mut self) -> io::Result<()> {
        self.advance_pending(true).await
    }

    pub(super) async fn wait_writable<F>(&mut self, map_err: F) -> io::Result<()>
    where
        F: Fn(io::Error) -> io::Error + Copy,
    {
        if self.pending_is_busy() {
            self.wait_pending().await.map_err(map_err)?;
        }
        self.poll_progress(map_err).await
    }

    pub(super) async fn flush_queued<F>(&mut self, map_err: F) -> io::Result<bool>
    where
        F: Fn(io::Error) -> io::Error + Copy,
    {
        self.poll_progress(map_err).await?;
        if self.is_busy() {
            return Ok(false);
        }

        self.writer.flush().await.map_err(map_err)?;
        Ok(true)
    }
}