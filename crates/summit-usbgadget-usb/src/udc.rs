//
// SPDX-License-Identifier: LicenseRef-Ezurio-Clause
//
//! USB device controller (UDC) discovery via udev.
//!
//! Discovery is entirely udev-driven: the service starts, registers a udev
//! monitor on the `udc` subsystem, and then queries the controllers already
//! attached with a udev [`Enumerator`] over the same subsystem. Controllers
//! that appear later are delivered as monitor events, so no polling is needed
//! and the initial scan and the live stream share one source of truth. The set
//! of controllers to bind is chosen from the configuration's [`UdcSelector`].
//!
//! A discovered controller is resolved to a [`usb_gadget::Udc`] only when it is
//! actually bound; that object is the configfs bind backend, not part of
//! discovery.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io;

use tokio_udev::{AsyncMonitorSocket, Enumerator, MonitorBuilder};
use usb_gadget::{udcs, Udc};

use crate::config::UdcSelector;

/// The udev subsystem that USB device controllers belong to.
const SUBSYSTEM: &str = "udc";

/// Which controllers a gadget should be bound to.
#[derive(Debug)]
pub enum Selection {
    /// Bind only the first controller that appears.
    First,
    /// Bind every controller, including ones that appear later (hotplug).
    All,
    /// Bind each of the named controllers.
    Named(HashSet<String>),
}

impl Selection {
    /// Derives the selection from the configured [`UdcSelector`].
    ///
    /// When `udc` is omitted the default is [`Selection::All`]: every attached
    /// controller is bound and controllers that appear later are bound as they
    /// arrive. Use `udc = "first"` to bind only the first controller instead.
    pub fn from_config(selector: &Option<UdcSelector>) -> Self {
        match selector {
            None => Selection::All,
            Some(UdcSelector::One(s)) if is_all(s) => Selection::All,
            Some(UdcSelector::One(s)) if is_first(s) => Selection::First,
            Some(UdcSelector::One(s)) => Selection::Named(HashSet::from([s.clone()])),
            Some(UdcSelector::Many(v)) if v.iter().any(|s| is_all(s)) => Selection::All,
            Some(UdcSelector::Many(v)) if v.iter().any(|s| is_first(s)) => Selection::First,
            Some(UdcSelector::Many(v)) => Selection::Named(v.iter().cloned().collect()),
        }
    }

    /// Returns whether the controller `name` should be bound, given the set of
    /// controllers already bound.
    pub fn wants(&self, name: &OsStr, bound: &HashSet<OsString>) -> bool {
        if bound.contains(name) {
            return false;
        }
        match self {
            Selection::First => bound.is_empty(),
            Selection::All => true,
            Selection::Named(names) => names.contains(&*name.to_string_lossy()),
        }
    }

    /// Returns whether further controllers may still be bound, i.e. whether it
    /// is worth continuing to listen for udev events.
    pub fn wants_more(&self, bound: &HashSet<OsString>) -> bool {
        match self {
            Selection::First => bound.is_empty(),
            Selection::All => true,
            Selection::Named(names) => !names.iter().all(|n| bound.contains(OsStr::new(n))),
        }
    }
}

fn is_all(s: &str) -> bool {
    s.eq_ignore_ascii_case("all") || s == "*"
}

fn is_first(s: &str) -> bool {
    s.eq_ignore_ascii_case("first")
}

/// Returns the names of the controllers currently attached, discovered through
/// udev and sorted for deterministic ordering.
///
/// Enumeration failures are logged and treated as "none attached yet": the
/// service then relies purely on the udev monitor to deliver controllers as
/// they appear.
pub fn existing() -> Vec<OsString> {
    let mut names = match enumerate() {
        Ok(names) => names,
        Err(err) => {
            log::warn!("udev enumeration of {SUBSYSTEM} controllers failed: {err}");
            Vec::new()
        }
    };
    names.sort();
    names
}

/// Queries udev for every device currently attached to the `udc` subsystem.
fn enumerate() -> io::Result<Vec<OsString>> {
    let mut enumerator = Enumerator::new()?;
    enumerator.match_subsystem(SUBSYSTEM)?;
    Ok(enumerator.scan_devices()?.map(|device| device.sysname().to_os_string()).collect())
}

/// Resolves a controller name to a [`Udc`].
pub fn by_name(name: &OsStr) -> Option<Udc> {
    udcs().ok()?.into_iter().find(|u| u.name() == name)
}

/// Creates an async udev monitor used to watch controller-related udev events.
pub fn monitor() -> io::Result<AsyncMonitorSocket> {
    let socket = MonitorBuilder::new()?.match_subsystem(SUBSYSTEM)?.listen()?;
    AsyncMonitorSocket::new(socket)
}
