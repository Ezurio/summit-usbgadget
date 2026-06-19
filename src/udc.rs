//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//! USB device controller (UDC) discovery via udev.
//!
//! Existing controllers are enumerated through the `usb-gadget` crate, and new
//! controllers are discovered by listening to udev `add` events on the `udc`
//! subsystem (no polling). The set of controllers to bind is chosen from the
//! configuration's [`UdcSelector`].

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io;

use tokio_udev::{AsyncMonitorSocket, MonitorBuilder};
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
    pub fn from_config(selector: &Option<UdcSelector>) -> Self {
        match selector {
            None => Selection::First,
            Some(UdcSelector::One(s)) if is_all(s) => Selection::All,
            Some(UdcSelector::One(s)) => Selection::Named(HashSet::from([s.clone()])),
            Some(UdcSelector::Many(v)) if v.iter().any(|s| is_all(s)) => Selection::All,
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

/// Returns the names of the controllers currently present, sorted for
/// deterministic ordering.
pub fn existing() -> Vec<OsString> {
    let mut names: Vec<OsString> =
        udcs().map(|list| list.into_iter().map(|u| u.name().to_os_string()).collect()).unwrap_or_default();
    names.sort();
    names
}

/// Resolves a controller name to a [`Udc`].
pub fn by_name(name: &OsStr) -> Option<Udc> {
    udcs().ok()?.into_iter().find(|u| u.name() == name)
}

/// Creates an async udev monitor for `add` events on the `udc` subsystem.
pub fn monitor() -> io::Result<AsyncMonitorSocket> {
    let socket = MonitorBuilder::new()?.match_subsystem(SUBSYSTEM)?.listen()?;
    AsyncMonitorSocket::new(socket)
}
