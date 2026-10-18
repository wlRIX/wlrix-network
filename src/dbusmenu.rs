// SPDX-License-Identifier: GPL-3.0-or-later
//! The menu, as `com.canonical.dbusmenu` -- what the tray reads to draw it.
//!
//! The interface is served from the [`Row`] tree [`crate::menu`] builds. The tree lives behind a
//! mutex the main loop replaces; this side only reads it, and turns what the tray asks into
//! [`MenuEvent`]s for the loop. No NetworkManager calls are made here: they would run on the
//! session connection's dispatch thread, and a slow one would stall the tray's next request.
//!
//! # What the tray uses, and the rest
//!
//! `wlrix-tray` calls `AboutToShow`, `GetLayout` and `Event`. The rest of the interface --
//! `GetGroupProperties`, `GetProperty`, `EventGroup`, `AboutToShowGroup` -- is served too, so the
//! menu works from another desktop's tray, which may use any of them.
//!
//! # A changed menu
//!
//! Whenever the tree changes the loop bumps the revision and emits `LayoutUpdated`, and a tray
//! with the menu open fetches it again. That is how networks found by the scan an open menu asked
//! for appear in it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use zbus::object_server::SignalEmitter;
use zbus::zvariant::{Dict, OwnedValue, Signature, StructureBuilder, Value};

use crate::menu::{Action, Row, Toggle};

/// Where the menu is served, and what the item's `Menu` property names.
pub const MENU_PATH: &str = "/MenuBar";
const MENU_INTERFACE: &str = "com.canonical.dbusmenu";

/// What the menu tells the main loop.
#[derive(Debug, PartialEq, Eq)]
pub enum MenuEvent {
    /// The menu is about to be shown: a good moment to scan.
    Opening,
    /// A row was chosen.
    Chosen(Action),
}

/// The tree and its revision, shared with the main loop.
#[derive(Debug)]
pub struct Layout {
    pub root: Row,
    pub revision: u32,
}

/// The interface object.
pub struct Menu {
    pub layout: Arc<Mutex<Layout>>,
    pub events: calloop::channel::Sender<MenuEvent>,
}

impl Menu {
    fn layout(&self) -> std::sync::MutexGuard<'_, Layout> {
        self.layout
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Act on one event. `false` when there is no such row.
    fn handle(&self, id: i32, event_id: &str) -> bool {
        let action = {
            let layout = self.layout();
            let Some(row) = layout.root.find(id) else {
                return false;
            };
            row.action.clone().filter(|_| row.enabled)
        };
        // "clicked" is the one that means chosen. "hovered", "opened" and "closed" are there to
        // be ignored.
        if event_id == "clicked"
            && let Some(action) = action
        {
            let _ = self.events.send(MenuEvent::Chosen(action));
        }
        true
    }
}

/// A row's properties, as dbusmenu spells them. Defaults are left out, as the specification
/// allows: `enabled` only when false, no toggle keys on a row without one.
fn properties(row: &Row) -> HashMap<&'static str, Value<'static>> {
    let mut map = HashMap::new();
    if row.separator {
        map.insert("type", Value::from("separator"));
        return map;
    }
    map.insert("label", Value::from(row.wire_label()));
    if !row.enabled {
        map.insert("enabled", Value::from(false));
    }
    match row.toggle {
        Some(Toggle::Check(on)) => {
            map.insert("toggle-type", Value::from("checkmark"));
            map.insert("toggle-state", Value::from(i32::from(on)));
        }
        Some(Toggle::Radio(on)) => {
            map.insert("toggle-type", Value::from("radio"));
            map.insert("toggle-state", Value::from(i32::from(on)));
        }
        None => {}
    }
    if !row.children.is_empty() {
        map.insert("children-display", Value::from("submenu"));
    }
    map
}

/// Only the properties a caller asked for. An empty list means all of them.
fn wanted<'a>(
    all: HashMap<&'static str, Value<'a>>,
    names: &[String],
) -> HashMap<&'static str, Value<'a>> {
    if names.is_empty() {
        return all;
    }
    all.into_iter()
        .filter(|(key, _)| names.iter().any(|name| name == key))
        .collect()
}

/// `(ia{sv}av)`: the row `GetLayout` answers with.
///
/// A struct of its own rather than a variant, because the reply's signature is `u(ia{sv}av)`.
/// Handing back the row as one `OwnedValue` gave `uv`, which a tray that deserializes the reply as
/// the specification says -- `wlrix-tray` does -- refuses outright.
#[derive(Debug, serde::Serialize, zbus::zvariant::Type)]
pub struct Node(i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

/// `GetLayout`'s answer for `row`, with its children to `depth` levels (-1 for all of them).
fn layout_node(row: &Row, depth: i32, names: &[String]) -> Node {
    let properties = wanted(properties(row), names)
        .into_iter()
        .filter_map(|(key, value)| Some((key.to_owned(), OwnedValue::try_from(value).ok()?)))
        .collect();
    let children = if depth == 0 {
        Vec::new()
    } else {
        row.children
            .iter()
            .filter_map(|child| OwnedValue::try_from(node(child, depth - 1, names)).ok())
            .collect()
    };
    Node(row.id, properties, children)
}

/// A child row, as the variant an `av` of children carries: the same `(ia{sv}av)`, as a value.
fn node(row: &Row, depth: i32, names: &[String]) -> Value<'static> {
    let mut dict = Dict::new(&Signature::Str, &Signature::Variant);
    for (key, value) in wanted(properties(row), names) {
        dict.append(Value::from(key), Value::Value(Box::new(value)))
            .expect("a string key and a variant value");
    }
    let children: Vec<Value<'static>> = if depth == 0 {
        Vec::new()
    } else {
        row.children
            .iter()
            .map(|child| Value::Value(Box::new(node(child, depth - 1, names))))
            .collect()
    };
    let children = zbus::zvariant::Array::from(children);
    StructureBuilder::new()
        .append_field(Value::from(row.id))
        .append_field(Value::from(dict))
        .append_field(Value::from(children))
        .build()
        .expect("the row should build")
        .into()
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    /// The tree under `parent_id`, to `recursion_depth` levels.
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, Node)> {
        let layout = self.layout();
        let Some(row) = layout.root.find(parent_id) else {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "no menu row {parent_id}"
            )));
        };
        Ok((
            layout.revision,
            layout_node(row, recursion_depth, &property_names),
        ))
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        let layout = self.layout();
        ids.into_iter()
            .filter_map(|id| {
                let row = layout.root.find(id)?;
                let map = wanted(properties(row), &property_names)
                    .into_iter()
                    .filter_map(|(key, value)| {
                        Some((key.to_owned(), OwnedValue::try_from(value).ok()?))
                    })
                    .collect();
                Some((id, map))
            })
            .collect()
    }

    fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedValue> {
        let layout = self.layout();
        layout
            .root
            .find(id)
            .and_then(|row| properties(row).remove(name.as_str()))
            .and_then(|value| OwnedValue::try_from(value).ok())
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("row {id} has no {name}")))
    }

    fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        self.handle(id, &event_id);
    }

    /// Several events at once. Answers with the ids that named no row.
    fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        events
            .into_iter()
            .filter(|(id, event_id, _, _)| !self.handle(*id, event_id))
            .map(|(id, ..)| id)
            .collect()
    }

    /// The menu, or a submenu, is about to be shown. Answers `false` -- "no need to fetch again"
    /// -- since the layout is always current; a scan this starts brings a `LayoutUpdated` of its
    /// own when it finds something.
    fn about_to_show(&self, _id: i32) -> bool {
        let _ = self.events.send(MenuEvent::Opening);
        false
    }

    fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        let _ = self.events.send(MenuEvent::Opening);
        (Vec::new(), Vec::new())
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn text_direction(&self) -> String {
        "ltr".to_owned()
    }

    #[zbus(property)]
    fn status(&self) -> String {
        "normal".to_owned()
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(signal)]
    async fn layout_updated(
        emitter: &SignalEmitter<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;
}

/// Replace the tree, and tell the tray if it changed.
pub fn update(connection: &zbus::blocking::Connection, layout: &Mutex<Layout>, root: Row) {
    let revision = {
        let mut layout = layout
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if layout.root == root {
            return;
        }
        layout.root = root;
        layout.revision = layout.revision.wrapping_add(1);
        layout.revision
    };
    if let Err(err) = connection.emit_signal(
        None::<zbus::names::BusName<'_>>,
        MENU_PATH,
        MENU_INTERFACE,
        "LayoutUpdated",
        &(revision, 0i32),
    ) {
        eprintln!("wlrix-network: could not emit LayoutUpdated: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::{Ids, build};
    use crate::nm::{Connectivity, DeviceState, Network, Snapshot, WifiDevice};

    fn menu() -> (Menu, calloop::channel::Channel<MenuEvent>) {
        let snapshot = Snapshot {
            wireless_enabled: true,
            hardware_enabled: true,
            connectivity: Connectivity::Full,
            wifi: vec![WifiDevice {
                path: "/dev/wlan0".into(),
                interface: "wlan0".into(),
                mode: crate::nm::Mode::Client,
                state: DeviceState::Disconnected,
                access_point: None,
                ipv4: None,
                saved: Vec::new(),
                networks: vec![Network {
                    ssid: "my_net".into(),
                    strength: 90,
                    secured: true,
                    enterprise: false,
                    path: "/ap/1".into(),
                }],
            }],
        };
        let root = build(Some(&snapshot), &mut Ids::default());
        let (events, channel) = calloop::channel::channel();
        (
            Menu {
                layout: Arc::new(Mutex::new(Layout { root, revision: 1 })),
                events,
            },
            channel,
        )
    }

    /// Everything the channel holds right now.
    fn drain(channel: &calloop::channel::Channel<MenuEvent>) -> Vec<MenuEvent> {
        let mut out = Vec::new();
        while let Ok(event) = channel.try_recv() {
            out.push(event);
        }
        out
    }

    #[test]
    fn the_reply_has_the_signature_the_specification_gives() {
        use zbus::zvariant::Type;
        assert_eq!(<(u32, Node)>::SIGNATURE.to_string(), "(u(ia{sv}av))");
    }

    #[test]
    fn the_layout_is_the_shape_the_tray_parses() {
        let (menu, _channel) = menu();
        let (revision, Node(id, _, children)) = menu.get_layout(0, -1, Vec::new()).unwrap();
        assert_eq!(revision, 1);
        assert_eq!(id, 0);
        assert!(children.len() >= 4);
        // A child is the same structure, as a variant. The network row: its label escaped, a
        // radio, not checked.
        let row = children
            .iter()
            .map(|child| {
                <(i32, HashMap<String, OwnedValue>, Vec<OwnedValue>)>::try_from(
                    child.try_clone().unwrap(),
                )
                .unwrap()
            })
            .find(|(id, ..)| *id >= 100)
            .expect("a network row");
        let label: String = row.1["label"].try_clone().unwrap().try_into().unwrap();
        assert!(label.starts_with("my__net"), "{label}");
        let kind: String = row.1["toggle-type"]
            .try_clone()
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(kind, "radio");
    }

    #[test]
    fn depth_zero_is_the_row_alone() {
        let (menu, _channel) = menu();
        let (_, Node(_, _, children)) = menu.get_layout(0, 0, Vec::new()).unwrap();
        assert!(children.is_empty());
        assert!(menu.get_layout(9999, -1, Vec::new()).is_err());
    }

    #[test]
    fn a_click_becomes_an_action_and_a_heading_does_nothing() {
        let (menu, channel) = menu();
        assert!(menu.handle(100, "clicked"));
        assert!(menu.handle(1, "clicked"), "the heading exists");
        assert!(menu.handle(100, "hovered"));
        assert!(!menu.handle(4242, "clicked"), "no such row");
        assert_eq!(
            drain(&channel),
            vec![MenuEvent::Chosen(Action::Join("my_net".into()))]
        );
    }

    #[test]
    fn group_calls_answer_for_each_id() {
        let (menu, channel) = menu();
        let properties = menu.get_group_properties(vec![2, 4242], vec!["toggle-type".into()]);
        assert_eq!(properties.len(), 1);
        assert_eq!(properties[0].1.len(), 1, "only what was asked for");
        let missing = menu.event_group(vec![
            (2, "clicked".into(), OwnedValue::from(0i32), 0),
            (4242, "clicked".into(), OwnedValue::from(0i32), 0),
        ]);
        assert_eq!(missing, vec![4242]);
        assert_eq!(
            drain(&channel),
            vec![MenuEvent::Chosen(Action::Wireless(false))]
        );
        assert!(menu.get_property(2, "label".into()).is_ok());
        assert!(menu.get_property(2, "nonsense".into()).is_err());
    }
}
