// SPDX-License-Identifier: GPL-3.0-or-later
//! The menu the Wi-Fi icon opens, as a tree of rows -- built from a snapshot, no bus.
//!
//! ```text
//! Wi-Fi: Connected to HomeNet        (a heading; cannot be chosen)
//! [x] Wireless Enabled
//! ---------------------------------
//! (*) HomeNet         ▂▄▆█
//! ( ) Neighbor-5G     ▂▄▆
//! ( ) CoffeeShop      ▂▄   (open)
//! More Networks                 >    (past the first eight)
//! ---------------------------------
//! Join Other Network...
//! Disconnect
//! ---------------------------------
//! Network Settings...
//! ```
//!
//! The tray draws it -- as a 4Dwm menu, from `com.canonical.dbusmenu`, which [`crate::dbusmenu`]
//! serves from this tree.
//!
//! # Ids are stable
//!
//! A click arrives as a row id, some time after the tray fetched the layout, and a scan may have
//! reordered the list in between. If ids were positions, a click on the third network could join
//! whichever network had moved into third place. So each network keeps the id it was first given
//! for as long as the daemon runs ([`Ids`]), and the fixed rows have fixed ids: an id always
//! means the same thing, and a click on a row that has since gone is simply ignored.
//!
//! The last row, under every state of the radio, opens Network Settings
//! (`wlrix-settings-network`), where the saved networks are looked after.

use std::collections::HashMap;

use crate::nm::{ApSecurity, DeviceState, Network, Snapshot};

/// What choosing a row does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Turn the radio on or off, to this.
    Wireless(bool),
    /// Join the network with this name.
    Join(String),
    /// Join a hidden network, named in the prompt.
    JoinOther,
    Disconnect,
    /// Open the Network Settings panel.
    OpenSettings,
}

/// A row's check box or radio button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    Check(bool),
    Radio(bool),
}

/// One row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: i32,
    /// As shown -- not yet escaped for dbusmenu's mnemonics; see [`Row::wire_label`].
    pub label: String,
    pub enabled: bool,
    pub separator: bool,
    pub toggle: Option<Toggle>,
    pub children: Vec<Row>,
    pub action: Option<Action>,
}

impl Row {
    fn new(id: i32, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
            enabled: true,
            separator: false,
            toggle: None,
            children: Vec::new(),
            action: None,
        }
    }

    fn heading(id: i32, label: impl Into<String>) -> Self {
        Self {
            enabled: false,
            ..Self::new(id, label)
        }
    }

    fn separator(id: i32) -> Self {
        Self {
            separator: true,
            ..Self::new(id, "")
        }
    }

    /// The label as dbusmenu carries it. A single `_` there marks a mnemonic and is not shown, so
    /// a network called `my_home` would be drawn as `myhome` -- doubled, it is one literal
    /// underscore. Network names are the only labels here a person did not choose.
    pub fn wire_label(&self) -> String {
        self.label.replace('_', "__")
    }

    /// This row or one under it, by id.
    pub fn find(&self, id: i32) -> Option<&Row> {
        if self.id == id {
            return Some(self);
        }
        self.children.iter().find_map(|child| child.find(id))
    }
}

/// The fixed rows' ids. The root is 0, as dbusmenu requires.
const HEADING: i32 = 1;
const WIRELESS: i32 = 2;
const NETWORKS_SEPARATOR: i32 = 3;
const NOTHING_FOUND: i32 = 4;
const MORE: i32 = 5;
const ACTIONS_SEPARATOR: i32 = 6;
const DISCONNECT: i32 = 7;
const JOIN_OTHER: i32 = 8;
const SETTINGS_SEPARATOR: i32 = 9;
const SETTINGS: i32 = 10;
/// Network ids start here, well clear of the fixed ones.
const FIRST_NETWORK: i32 = 100;

/// How many networks are listed before the rest go in "More Networks".
const TOP_LEVEL: usize = 8;
/// How many are listed at all. Past this it is a stadium, and nobody is choosing from a list.
const MOST: usize = 40;

/// The ids networks have been given, by name, for as long as the daemon runs.
#[derive(Debug, Default)]
pub struct Ids {
    given: HashMap<String, i32>,
}

impl Ids {
    fn of(&mut self, ssid: &str) -> i32 {
        let next = FIRST_NETWORK + self.given.len() as i32;
        *self.given.entry(ssid.to_owned()).or_insert(next)
    }
}

/// Signal strength as bars. Block elements rather than an icon: dbusmenu rows here are text, as a
/// 4Dwm menu's are. The same five buckets as the tray icon -- none, then one to four bars -- so
/// the two agree.
fn bars(strength: u8) -> &'static str {
    match strength {
        0..20 => "▁",
        20..40 => "▂",
        40..50 => "▂▄",
        50..80 => "▂▄▆",
        _ => "▂▄▆█",
    }
}

/// One network's row. Secured is the usual case and is not marked; an open network is, since
/// everything sent over it can be read by anyone nearby.
fn network_row(network: &Network, connected: Option<&str>, ids: &mut Ids) -> Row {
    let mut label = format!("{}   {}", network.ssid, bars(network.strength));
    match network.security {
        ApSecurity::Open => label.push_str("   (open)"),
        ApSecurity::Enterprise => label.push_str("   (enterprise)"),
        _ => {}
    }
    Row {
        toggle: Some(Toggle::Radio(connected == Some(network.ssid.as_str()))),
        action: Some(Action::Join(network.ssid.clone())),
        ..Row::new(ids.of(&network.ssid), label)
    }
}

/// Build the menu's root, whose children are the rows.
pub fn build(snapshot: Option<&Snapshot>, ids: &mut Ids) -> Row {
    let mut root = Row::new(0, "");
    let Some(snapshot) = snapshot else {
        root.children
            .push(Row::heading(HEADING, "NetworkManager is not running"));
        return root;
    };
    let Some(device) = snapshot.primary() else {
        root.children
            .push(Row::heading(HEADING, "No wireless device"));
        return root;
    };
    let on = snapshot.wireless_enabled && snapshot.hardware_enabled;
    let connected = match device.state {
        DeviceState::Connected | DeviceState::Connecting => {
            device.access_point.as_ref().map(|ap| ap.ssid.as_str())
        }
        _ => None,
    };

    if device.mode == crate::nm::Mode::Hotspot && device.state == DeviceState::Connected {
        // The only Wi-Fi device is sharing a hotspot, so there is nothing to join from it. The
        // switch stays, and Disconnect is how a hotspot is stopped.
        root.children.push(Row::heading(
            HEADING,
            format!("Wi-Fi: Hotspot {}", connected.unwrap_or_default()),
        ));
        root.children.push(Row {
            toggle: Some(Toggle::Check(on)),
            action: Some(Action::Wireless(!snapshot.wireless_enabled)),
            enabled: snapshot.hardware_enabled,
            ..Row::new(WIRELESS, "Wireless Enabled")
        });
        root.children.push(Row::separator(ACTIONS_SEPARATOR));
        root.children.push(Row {
            action: Some(Action::Disconnect),
            ..Row::new(DISCONNECT, "Stop Hotspot")
        });
        settings_rows(&mut root);
        return root;
    }

    let heading = if !snapshot.hardware_enabled {
        "Wi-Fi: Off at the hardware switch".to_owned()
    } else if !on {
        "Wi-Fi: Off".to_owned()
    } else {
        match (device.state, connected) {
            (DeviceState::Connected, Some(ssid)) => format!("Wi-Fi: Connected to {ssid}"),
            (DeviceState::Connecting, Some(ssid)) => format!("Wi-Fi: Connecting to {ssid}"),
            (DeviceState::Connecting, None) => "Wi-Fi: Connecting".to_owned(),
            _ => "Wi-Fi: Not connected".to_owned(),
        }
    };
    root.children.push(Row::heading(HEADING, heading));
    root.children.push(Row {
        toggle: Some(Toggle::Check(on)),
        action: Some(Action::Wireless(!snapshot.wireless_enabled)),
        // The software switch cannot overrule the hardware one, so offering it would be a lie.
        enabled: snapshot.hardware_enabled,
        ..Row::new(WIRELESS, "Wireless Enabled")
    });
    if !on {
        settings_rows(&mut root);
        return root;
    }

    root.children.push(Row::separator(NETWORKS_SEPARATOR));
    // The network in use first, then the ones NetworkManager already knows how to join, then the
    // rest -- each group strongest first, which is the order the scan arrives in.
    let known = |network: &&Network| device.saved.iter().any(|saved| saved.ssid == network.ssid);
    let mut ordered: Vec<&Network> = Vec::new();
    ordered.extend(
        device
            .networks
            .iter()
            .filter(|n| Some(n.ssid.as_str()) == connected),
    );
    ordered.extend(
        device
            .networks
            .iter()
            .filter(|n| Some(n.ssid.as_str()) != connected)
            .filter(known),
    );
    ordered.extend(
        device
            .networks
            .iter()
            .filter(|n| Some(n.ssid.as_str()) != connected)
            .filter(|n| !known(n)),
    );
    ordered.truncate(MOST);

    if ordered.is_empty() {
        root.children
            .push(Row::heading(NOTHING_FOUND, "No networks found"));
    }
    let (top, rest) = ordered.split_at(ordered.len().min(TOP_LEVEL));
    for network in top {
        root.children.push(network_row(network, connected, ids));
    }
    if !rest.is_empty() {
        let mut more = Row::new(MORE, "More Networks");
        more.children = rest
            .iter()
            .map(|network| network_row(network, connected, ids))
            .collect();
        root.children.push(more);
    }

    root.children.push(Row::separator(ACTIONS_SEPARATOR));
    root.children.push(Row {
        action: Some(Action::JoinOther),
        ..Row::new(JOIN_OTHER, "Join Other Network...")
    });
    if matches!(
        device.state,
        DeviceState::Connected | DeviceState::Connecting
    ) {
        root.children.push(Row {
            action: Some(Action::Disconnect),
            ..Row::new(DISCONNECT, "Disconnect")
        });
    }
    settings_rows(&mut root);
    root
}

/// The way to the panel, at the foot of every menu that has a Wi-Fi device to talk about.
fn settings_rows(root: &mut Row) {
    root.children.push(Row::separator(SETTINGS_SEPARATOR));
    root.children.push(Row {
        action: Some(Action::OpenSettings),
        ..Row::new(SETTINGS, "Network Settings...")
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nm::{AccessPoint, Connectivity, Saved, WifiDevice};

    fn network(ssid: &str, strength: u8, secured: bool) -> Network {
        Network {
            ssid: ssid.into(),
            strength,
            security: if secured {
                ApSecurity::WpaPsk
            } else {
                ApSecurity::Open
            },
            path: format!("/ap/{ssid}"),
        }
    }

    fn snapshot(
        state: DeviceState,
        connected_to: Option<&str>,
        networks: Vec<Network>,
    ) -> Snapshot {
        Snapshot {
            wireless_enabled: true,
            hardware_enabled: true,
            connectivity: Connectivity::Full,
            wifi: vec![WifiDevice {
                path: "/dev/wlan0".into(),
                interface: "wlan0".into(),
                mode: crate::nm::Mode::Client,
                state,
                access_point: connected_to.map(|ssid| AccessPoint {
                    ssid: ssid.into(),
                    strength: 70,
                    secured: true,
                }),
                ipv4: None,
                networks,
                saved: Vec::new(),
            }],
        }
    }

    fn labels(row: &Row) -> Vec<String> {
        row.children
            .iter()
            .map(|child| child.label.clone())
            .collect()
    }

    #[test]
    fn connected_lists_the_network_in_use_first_with_its_radio_set() {
        let snapshot = snapshot(
            DeviceState::Connected,
            Some("HomeNet"),
            vec![
                network("Neighbor", 90, true),
                network("HomeNet", 70, true),
                network("Cafe", 30, false),
            ],
        );
        let menu = build(Some(&snapshot), &mut Ids::default());
        assert_eq!(
            labels(&menu),
            [
                "Wi-Fi: Connected to HomeNet",
                "Wireless Enabled",
                "",
                "HomeNet   ▂▄▆",
                "Neighbor   ▂▄▆█",
                "Cafe   ▂   (open)",
                "",
                "Join Other Network...",
                "Disconnect",
                "",
                "Network Settings...",
            ]
        );
        assert!(!menu.children[0].enabled, "the heading cannot be chosen");
        assert_eq!(menu.children[1].toggle, Some(Toggle::Check(true)));
        assert_eq!(menu.children[3].toggle, Some(Toggle::Radio(true)));
        assert_eq!(menu.children[4].toggle, Some(Toggle::Radio(false)));
        assert_eq!(
            menu.children[4].action,
            Some(Action::Join("Neighbor".into()))
        );
    }

    #[test]
    fn known_networks_come_before_strangers() {
        let mut snapshot = snapshot(
            DeviceState::Disconnected,
            None,
            vec![network("Stranger", 90, true), network("Office", 40, true)],
        );
        snapshot.wifi[0].saved.push(Saved {
            ssid: "Office".into(),
            path: "/settings/1".into(),
        });
        let menu = build(Some(&snapshot), &mut Ids::default());
        assert_eq!(menu.children[0].label, "Wi-Fi: Not connected");
        assert!(menu.children[3].label.starts_with("Office"));
        assert!(menu.children[4].label.starts_with("Stranger"));
        assert!(
            menu.find(DISCONNECT).is_none(),
            "nothing to disconnect from"
        );
    }

    #[test]
    fn a_radio_that_is_off_offers_only_the_switch() {
        let mut off = snapshot(DeviceState::Unavailable, None, vec![network("x", 50, true)]);
        off.wireless_enabled = false;
        let menu = build(Some(&off), &mut Ids::default());
        assert_eq!(
            labels(&menu),
            ["Wi-Fi: Off", "Wireless Enabled", "", "Network Settings..."]
        );
        assert_eq!(menu.children[1].toggle, Some(Toggle::Check(false)));
        assert_eq!(menu.children[1].action, Some(Action::Wireless(true)));

        off.hardware_enabled = false;
        let menu = build(Some(&off), &mut Ids::default());
        assert!(
            !menu.children[1].enabled,
            "software cannot overrule the switch"
        );
    }

    #[test]
    fn past_eight_the_rest_go_in_a_submenu() {
        let networks = (0..12)
            .map(|i| network(&format!("net{i:02}"), 90 - i as u8, true))
            .collect();
        let menu = build(
            Some(&snapshot(DeviceState::Disconnected, None, networks)),
            &mut Ids::default(),
        );
        let more = menu.find(MORE).expect("a More Networks row");
        assert_eq!(more.children.len(), 4);
        assert_eq!(
            menu.children
                .iter()
                .filter(|row| matches!(row.action, Some(Action::Join(_))))
                .count(),
            TOP_LEVEL
        );
    }

    #[test]
    fn nothing_in_range_says_so() {
        let menu = build(
            Some(&snapshot(DeviceState::Disconnected, None, Vec::new())),
            &mut Ids::default(),
        );
        assert_eq!(menu.find(NOTHING_FOUND).unwrap().label, "No networks found");
    }

    #[test]
    fn a_network_keeps_its_id_when_the_list_reorders() {
        let mut ids = Ids::default();
        let first = build(
            Some(&snapshot(
                DeviceState::Disconnected,
                None,
                vec![network("A", 90, true), network("B", 50, true)],
            )),
            &mut ids,
        );
        let second = build(
            Some(&snapshot(
                DeviceState::Disconnected,
                None,
                vec![network("B", 95, true), network("A", 40, true)],
            )),
            &mut ids,
        );
        let id_of = |menu: &Row, ssid: &str| {
            menu.children
                .iter()
                .find(|row| row.action == Some(Action::Join(ssid.into())))
                .unwrap()
                .id
        };
        assert_eq!(id_of(&first, "A"), id_of(&second, "A"));
        assert_eq!(id_of(&first, "B"), id_of(&second, "B"));
        assert_ne!(id_of(&second, "A"), id_of(&second, "B"));
    }

    #[test]
    fn underscores_survive_dbusmenus_mnemonics() {
        let row = Row::new(1, "my_home_net");
        assert_eq!(row.wire_label(), "my__home__net");
    }

    #[test]
    fn no_device_is_just_a_heading() {
        let menu = build(Some(&Snapshot::default()), &mut Ids::default());
        assert_eq!(labels(&menu), ["No wireless device"]);
        assert_eq!(
            labels(&build(None, &mut Ids::default())),
            ["NetworkManager is not running"]
        );
    }
}
