// SPDX-License-Identifier: GPL-3.0-or-later
//! wlRIX's network status.
//!
//! NetworkManager's Wi-Fi state, published as a StatusNotifierItem so `wlrix-tray` draws it: an
//! icon with bars of signal, a tooltip naming the network and its address, and a menu of nearby
//! networks to join. Steps one and two of `_docs/wlrix-network-roadmap.md`; the password prompt
//! and the Network Settings panel are later steps.
//!
//! - [`nm`] reads NetworkManager on the system bus and says when it changes.
//! - [`indicator`] turns what it read into an icon, a status and a tooltip. No bus; unit-tested.
//! - [`menu`] builds the menu from what was read. No bus; unit-tested.
//! - [`sni`] is the item on the session bus, and its registration with the tray; [`dbusmenu`]
//!   serves the menu beside it.
//! - [`daemon`] is the loop between them.

pub mod config;
pub mod daemon;
pub mod dbusmenu;
pub mod indicator;
pub mod menu;
pub mod nm;
pub mod pidfile;
pub mod signals;
pub mod sni;
pub mod xdg;
