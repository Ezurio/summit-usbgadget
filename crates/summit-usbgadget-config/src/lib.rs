//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//

//! Common configuration subsystem for `summit-usbgadget`.
//!
//! A single TOML configuration file is shared by every anchor and plugin. This
//! crate owns loading that file and handing each plugin its own subtree; it has
//! no knowledge of any particular plugin's schema. Plugins deserialize their
//! own section (or the whole document, for the USB gadget) through
//! [`ConfigDocument`], keeping the file format common while each plugin remains
//! responsible for its own configuration.
//!
//! It also owns the startup [`Service`] registry: each anchor plugin registers
//! one service with [`declare_service!`], and the composing binary starts every
//! registered service through [`run_services`] without knowing which plugins
//! are compiled in.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde::de::DeserializeOwned;
use toml::{Table, Value};

pub mod sysinfo;

/// The future produced by a registered [`Service`].
pub type ServiceFuture = Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>>>>;

/// A cloneable shutdown signal shared by every running service.
///
/// Services run for the lifetime of the process; each awaits [`Shutdown::wait`]
/// and returns only once shutdown has been requested (via a termination signal)
/// or it fails.
#[derive(Clone)]
pub struct Shutdown {
    rx: tokio::sync::watch::Receiver<bool>,
}

impl Shutdown {
    /// Creates a shutdown signal that fires on `SIGTERM`/`SIGINT`.
    pub fn from_signals() -> Self {
        let (tx, rx) = tokio::sync::watch::channel(false);
        // Detach a task that flips the flag on the first termination signal.
        drop(tokio::spawn(async move {
            wait_for_signal().await;
            log::info!("termination signal received, requesting shutdown");
            let _ = tx.send(true);
        }));
        Self { rx }
    }

    /// Resolves once shutdown has been requested.
    pub async fn wait(&self) {
        let mut rx = self.rx.clone();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let (mut term, mut int) = match (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) {
        (Ok(term), Ok(int)) => (term, int),
        _ => {
            log::error!("failed to install termination signal handlers");
            std::future::pending::<()>().await;
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    std::future::pending::<()>().await;
}

/// A top-level service contributed by a plugin, started at application startup.
///
/// Each anchor plugin registers exactly one service with [`declare_service!`];
/// the composing binary runs every registered service without knowing which
/// plugins are compiled in.
pub struct Service {
    /// Human-readable service name, used for logging.
    pub name: &'static str,
    /// Entry point: given the shared configuration file path and the shutdown
    /// signal, returns the service's run future. The service reads its own
    /// configuration section and runs until shutdown is requested.
    pub run: fn(PathBuf, Shutdown) -> ServiceFuture,
}

impl Service {
    pub const fn new(name: &'static str, run: fn(PathBuf, Shutdown) -> ServiceFuture) -> Self {
        Self { name, run }
    }
}

inventory::collect!(Service);

#[doc(hidden)]
pub use inventory::submit as __inventory_submit;

/// Registers a plugin's startup service.
///
/// `run` is an `async fn` (or function returning a future) that takes the
/// configuration file path and a [`Shutdown`] signal and returns
/// `Result<(), Box<dyn Error>>`.
#[macro_export]
macro_rules! declare_service {
    ($name:literal => $run:path) => {
        $crate::__inventory_submit! {
            $crate::Service::new($name, |path, shutdown| {
                ::std::boxed::Box::pin($run(path, shutdown))
            })
        }
    };
}

/// Starts every registered [`Service`] and runs them for the lifetime of the
/// process.
///
/// Services are independent: one failing (for example the USB gadget when no
/// controller or configfs is available) must not cancel the others (for example
/// the fastboot-over-TCP updater, which needs no USB at all). Each service is
/// therefore run to completion on its own, its error is logged, and the
/// remaining services keep running. The process returns an error only once every
/// service has stopped and at least one of them failed.
pub async fn run_services(config_path: PathBuf) -> Result<(), Box<dyn Error>> {
    let shutdown = Shutdown::from_signals();
    let futures: Vec<_> = inventory::iter::<Service>
        .into_iter()
        .map(|service| {
            log::info!("starting {} service", service.name);
            let future = (service.run)(config_path.clone(), shutdown.clone());
            async move {
                match future.await {
                    Ok(()) => Ok(()),
                    Err(err) => {
                        log::error!("{} service failed: {err}", service.name);
                        Err(err)
                    }
                }
            }
        })
        .collect();

    if futures.is_empty() {
        log::warn!("no services registered; nothing to run");
        return Ok(());
    }

    // Wait for every service regardless of individual failures so that one
    // service's error never cancels the others still doing useful work.
    let mut first_error: Option<Box<dyn Error>> = None;
    for result in futures_util::future::join_all(futures).await {
        if let Err(err) = result
            && first_error.is_none()
        {
            first_error = Some(err);
        }
    }

    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// A loaded configuration document.
///
/// Owns the raw TOML root table so each plugin can retrieve and deserialize its
/// own section from the shared file.
#[derive(Debug, Clone)]
pub struct ConfigDocument {
    root: Table,
}

impl ConfigDocument {
    /// Reads and parses a TOML configuration file from `path`.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        Self::from_toml_str(&text)
    }

    /// Parses a TOML configuration document from a string.
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let value: Value = toml::from_str(text)?;
        let root = value
            .as_table()
            .cloned()
            .ok_or_else(|| ConfigError::Structure("configuration root must be a TOML table".to_string()))?;
        Ok(Self { root })
    }

    /// Wraps an already-parsed root table.
    pub fn from_table(root: Table) -> Self {
        Self { root }
    }

    /// Returns the whole document as a TOML value.
    pub fn root_value(&self) -> Value {
        Value::Table(self.root.clone())
    }

    /// Borrows the raw root table.
    pub fn root_table(&self) -> &Table {
        &self.root
    }

    /// Returns a named top-level section as a TOML value, if present.
    pub fn section_value(&self, name: &str) -> Option<Value> {
        self.root.get(name).cloned()
    }

    /// Deserializes the whole document into a plugin configuration type.
    pub fn parse_root<T>(&self) -> Result<T, ConfigError>
    where
        T: DeserializeOwned,
    {
        self.root_value().try_into().map_err(ConfigError::Toml)
    }

    /// Deserializes a named section into a plugin configuration type.
    ///
    /// Returns `Ok(None)` when the section is absent, so a plugin can opt out of
    /// running when it has no configuration in the shared file.
    pub fn parse_section<T>(&self, name: &str) -> Result<Option<T>, ConfigError>
    where
        T: DeserializeOwned,
    {
        match self.section_value(name) {
            Some(value) => value.try_into().map(Some).map_err(ConfigError::Toml),
            None => Ok(None),
        }
    }
}

/// Convenience for plugin configuration types that deserialize from the shared
/// configuration document.
///
/// A plugin defines a `serde::Deserialize` configuration type and gains file
/// loading and document/section parsing for free by implementing this trait.
/// Leave [`PluginConfig::SECTION`] as `None` to read the whole document root
/// (for example the USB gadget definition), or set it to a table name to read
/// only that top-level section (for example `socket_source`).
pub trait PluginConfig: DeserializeOwned + Sized {
    /// The top-level section this configuration is read from, or `None` to read
    /// from the document root.
    const SECTION: Option<&'static str> = None;

    /// Loads and parses this configuration from a TOML file.
    fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        Self::from_document(&ConfigDocument::load(path)?)
    }

    /// Parses this configuration from an already-loaded shared document.
    fn from_document(document: &ConfigDocument) -> Result<Self, ConfigError> {
        match Self::SECTION {
            None => document.parse_root(),
            Some(section) => document.parse_section(section)?.ok_or_else(|| {
                ConfigError::Structure(format!("configuration is missing [{section}] section"))
            }),
        }
    }
}

/// An error produced while loading or parsing the shared configuration.
#[derive(Debug)]
pub enum ConfigError {
    /// The configuration file could not be read.
    Io(io::Error),
    /// The TOML could not be parsed.
    Toml(toml::de::Error),
    /// The root document shape is invalid.
    Structure(String),
    /// A plugin rejected its configuration subtree.
    Plugin(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "cannot read configuration: {e}"),
            ConfigError::Toml(e) => write!(f, "invalid configuration: {e}"),
            ConfigError::Structure(e) => write!(f, "invalid configuration structure: {e}"),
            ConfigError::Plugin(e) => write!(f, "invalid plugin configuration: {e}"),
        }
    }
}

impl Error for ConfigError {}

impl From<io::Error> for ConfigError {
    fn from(e: io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(e: toml::de::Error) -> Self {
        ConfigError::Toml(e)
    }
}
