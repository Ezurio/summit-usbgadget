//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! Shared streamed-download targets used by DFU and FBK.
//!
//! Both protocols receive firmware over different USB transports, but they
//! ultimately feed the same byte stream into either a file or SWUpdate.

use std::io;
use std::path::PathBuf;

use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;

use crate::swupdate::{SwupdateParams, SwupdateSink};

/// Where a streamed firmware image should be sent.
#[derive(Debug, Clone)]
pub(crate) enum DownloadTarget {
    /// Write received firmware to a file.
    File(PathBuf),
    /// Stream received firmware into SWUpdate.
    Swupdate(SwupdateParams),
}

/// Destination-specific session that consumes a streamed firmware image.
#[derive(Debug)]
enum DownloadSession {
    File(FileSink),
    Swupdate(SwupdateSink),
}

impl DownloadSession {
    pub(crate) fn new(target: DownloadTarget) -> Self {
        match target {
            DownloadTarget::File(path) => Self::File(FileSink::new(path)),
            DownloadTarget::Swupdate(params) => Self::Swupdate(SwupdateSink::new(params)),
        }
    }

    pub(crate) async fn begin(&mut self) -> io::Result<()> {
        match self {
            Self::File(s) => s.begin().await,
            Self::Swupdate(s) => s.begin().await,
        }
    }

    pub(crate) async fn write_block(&mut self, data: &[u8]) -> io::Result<()> {
        match self {
            Self::File(s) => s.write_block(data).await,
            Self::Swupdate(s) => s.write_block(data).await,
        }
    }

    pub(crate) async fn finish(&mut self) -> io::Result<()> {
        match self {
            Self::File(s) => s.finish().await,
            Self::Swupdate(s) => s.finish().await,
        }
    }

    pub(crate) async fn abort(&mut self) {
        match self {
            Self::File(s) => s.abort().await,
            Self::Swupdate(s) => s.abort().await,
        }
    }

    pub(crate) fn is_busy(&self) -> bool {
        match self {
            Self::File(_) => false,
            Self::Swupdate(s) => s.is_busy(),
        }
    }

    pub(crate) async fn poll_progress(&mut self) -> io::Result<()> {
        match self {
            Self::File(_) => Ok(()),
            Self::Swupdate(s) => s.poll_progress().await,
        }
    }
}

/// Active streamed-download state reused by DFU and FBK.
#[derive(Debug)]
pub(crate) struct ActiveDownload {
    target: DownloadTarget,
    session: Option<DownloadSession>,
}

impl ActiveDownload {
    pub(crate) fn new(target: DownloadTarget) -> Self {
        Self { target, session: None }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.session.is_some()
    }

    pub(crate) fn is_busy(&self) -> bool {
        self.session.as_ref().is_some_and(DownloadSession::is_busy)
    }

    pub(crate) async fn poll_progress(&mut self) -> io::Result<()> {
        if let Some(session) = self.session.as_mut() {
            session.poll_progress().await?;
        }
        Ok(())
    }

    pub(crate) async fn begin(&mut self) -> io::Result<()> {
        if self.session.is_none() {
            let mut session = DownloadSession::new(self.target.clone());
            session.begin().await?;
            self.session = Some(session);
        }
        Ok(())
    }

    pub(crate) async fn write_block(&mut self, data: &[u8]) -> io::Result<()> {
        self.begin().await?;
        self.session
            .as_mut()
            .expect("download session must exist after begin")
            .write_block(data)
            .await
    }

    pub(crate) async fn write_block_if_active(&mut self, data: &[u8]) -> io::Result<()> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "download not started"))?;
        session.write_block(data).await
    }

    pub(crate) async fn finish(&mut self) -> io::Result<()> {
        match self.session.take() {
            Some(mut session) => session.finish().await,
            None => Ok(()),
        }
    }

    pub(crate) async fn abort(&mut self) {
        if let Some(mut session) = self.session.take() {
            session.abort().await;
        }
    }
}

/// A sink that writes the firmware directly to a file.
#[derive(Debug)]
struct FileSink {
    path: PathBuf,
    file: Option<File>,
}

impl FileSink {
    fn new(path: PathBuf) -> Self {
        Self { path, file: None }
    }

    async fn begin(&mut self) -> io::Result<()> {
        let file =
            OpenOptions::new().create(true).write(true).truncate(true).open(&self.path).await?;
        self.file = Some(file);
        Ok(())
    }

    async fn write_block(&mut self, data: &[u8]) -> io::Result<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "download not started"))?;
        file.write_all(data).await
    }

    async fn finish(&mut self) -> io::Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.flush().await?;
        }
        self.file = None;
        Ok(())
    }

    async fn abort(&mut self) {
        self.file = None;
    }
}