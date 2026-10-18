// SPDX-License-Identifier: GPL-3.0-or-later
//! The Wi-Fi icon, as a StatusNotifierItem on the session bus.
//!
//! The tray is `org.kde.StatusNotifierWatcher`; this registers an item with it and the tray draws
//! it, its tooltip and its menu ([`crate::dbusmenu`]). Registered **KDE-style**, by bus name, with
//! the item at `/StatusNotifierItem`: the form fcitx5 uses and the tray's first-class path.
//!
//! # Changes
//!
//! The item's properties are read from a [`View`] behind a mutex, which the main loop replaces.
//! After replacing it the loop emits `NewIcon`, `NewToolTip` and `NewStatus`. The tray treats any
//! of them as "read everything again", so one would do, but a tray that listens for only the one
//! it cares about is not wrong, and three signals cost nothing.
//!
//! # The tray going away
//!
//! A registration lasts as long as the watcher that took it. When the tray restarts -- a crash, a
//! `--replace`, an update -- the new one knows nothing of this item, so the name
//! `org.kde.StatusNotifierWatcher` getting a new owner means "register again". Without that the
//! icon would be gone until this daemon restarted too.

use std::sync::{Arc, Mutex};

use zbus::MatchRule;
use zbus::message::Type;

use crate::dbusmenu::{Layout, MENU_PATH, Menu, MenuEvent};
use crate::indicator::View;
use crate::menu::Row;

/// Where the item is served.
pub const ITEM_PATH: &str = "/StatusNotifierItem";
const ITEM_INTERFACE: &str = "org.kde.StatusNotifierItem";
const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const DBUS: &str = "org.freedesktop.DBus";

/// `a(iiay)`: an icon as raw pixmaps. Always empty here; the icon is a themed name.
type Pixmaps = Vec<(i32, i32, Vec<u8>)>;

/// The item, as served.
struct Item {
    view: Arc<Mutex<View>>,
}

impl Item {
    fn view(&self) -> View {
        self.view
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    /// Never called by a tray that honors `ItemIsMenu`, which opens the menu instead. Another
    /// desktop's tray may call it anyway, and there is nothing else for a click to do.
    fn activate(&self, _x: i32, _y: i32) {}

    fn secondary_activate(&self, _x: i32, _y: i32) {}

    fn context_menu(&self, _x: i32, _y: i32) {}

    fn scroll(&self, _delta: i32, _orientation: String) {}

    /// What `[[item]]` in `tray.toml` keys on, to order or hide it.
    #[zbus(property)]
    fn id(&self) -> String {
        "wlrix-network".to_owned()
    }

    #[zbus(property)]
    fn title(&self) -> String {
        "Wi-Fi".to_owned()
    }

    #[zbus(property)]
    fn category(&self) -> String {
        "Hardware".to_owned()
    }

    #[zbus(property)]
    fn status(&self) -> String {
        self.view().status.as_str().to_owned()
    }

    #[zbus(property)]
    fn icon_name(&self) -> String {
        self.view().icon.to_owned()
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Pixmaps {
        Vec::new()
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> String {
        String::new()
    }

    /// A click opens the menu: the item has no other action, and saying so is what makes the
    /// tray open it on a left click rather than only a right one.
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn menu(&self) -> zbus::zvariant::ObjectPath<'static> {
        zbus::zvariant::ObjectPath::from_static_str_unchecked(MENU_PATH)
    }

    /// `(sa(iiay)ss)`: icon name, icon pixmaps, title, body.
    #[zbus(property)]
    fn tool_tip(&self) -> (String, Pixmaps, String, String) {
        let view = self.view();
        (String::new(), Vec::new(), view.title, view.body)
    }
}

/// The item on the bus: what the main loop keeps to change it.
pub struct Published {
    connection: zbus::blocking::Connection,
    view: Arc<Mutex<View>>,
    layout: Arc<Mutex<Layout>>,
    /// The bus name the item registers under.
    name: String,
}

impl Published {
    /// Show `view`, if it differs from what is showing.
    pub fn show(&self, view: View) {
        let previous_status;
        {
            let mut current = self
                .view
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *current == view {
                return;
            }
            previous_status = current.status;
            *current = view.clone();
        }
        let emit = |signal: &str| {
            if let Err(err) = self.connection.emit_signal(
                None::<zbus::names::BusName<'_>>,
                ITEM_PATH,
                ITEM_INTERFACE,
                signal,
                &(),
            ) {
                eprintln!("wlrix-network: could not emit {signal}: {err}");
            }
        };
        emit("NewIcon");
        emit("NewToolTip");
        if view.status != previous_status
            && let Err(err) = self.connection.emit_signal(
                None::<zbus::names::BusName<'_>>,
                ITEM_PATH,
                ITEM_INTERFACE,
                "NewStatus",
                &(view.status.as_str(),),
            )
        {
            eprintln!("wlrix-network: could not emit NewStatus: {err}");
        }
    }

    /// Replace the menu, telling the tray if it changed.
    pub fn set_menu(&self, root: Row) {
        crate::dbusmenu::update(&self.connection, &self.layout, root);
    }

    /// The session bus, for the notifications a failed action posts.
    pub fn connection(&self) -> &zbus::blocking::Connection {
        &self.connection
    }

    /// Register with the tray. No tray is not an error: [`watch_for_tray`] registers again when
    /// one appears.
    pub fn register(&self) {
        match self.connection.call_method(
            Some(WATCHER_NAME),
            WATCHER_PATH,
            Some(WATCHER_NAME),
            "RegisterStatusNotifierItem",
            &(self.name.as_str(),),
        ) {
            Ok(_) => eprintln!("wlrix-network: registered with the tray"),
            Err(err) => eprintln!("wlrix-network: no tray to register with yet ({err})"),
        }
    }
}

/// Serve the item and its menu, and take a bus name for them. Nothing is registered yet.
pub fn publish(
    view: View,
    root: Row,
    events: calloop::channel::Sender<MenuEvent>,
) -> Result<Published, String> {
    let view = Arc::new(Mutex::new(view));
    let layout = Arc::new(Mutex::new(Layout { root, revision: 1 }));
    let connection = zbus::blocking::connection::Builder::session()
        .map_err(|err| format!("no session bus: {err}"))?
        // See `crate::daemon::CALL_TIMEOUT`: the keyring and the notification daemon are called on
        // this connection, from the main loop.
        .method_timeout(crate::daemon::CALL_TIMEOUT)
        .serve_at(
            ITEM_PATH,
            Item {
                view: Arc::clone(&view),
            },
        )
        .map_err(|err| format!("could not serve the item: {err}"))?
        .serve_at(
            MENU_PATH,
            Menu {
                layout: Arc::clone(&layout),
                events,
            },
        )
        .map_err(|err| format!("could not serve the menu: {err}"))?
        .build()
        .map_err(|err| format!("could not connect to the session bus: {err}"))?;
    // The specification's naming convention, which the tray does not need -- it talks to the
    // sender's unique name -- but a person running `busctl --user list` does.
    let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    connection
        .request_name(name.as_str())
        .map_err(|err| format!("could not take {name}: {err}"))?;
    Ok(Published {
        connection,
        view,
        layout,
        name,
    })
}

/// Start a thread that says when a tray appears, so the item can register with it.
pub fn watch_for_tray(
    published: &Published,
    appeared: calloop::channel::Sender<()>,
) -> Result<(), String> {
    let connection = published.connection.clone();
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(DBUS)
        .and_then(|rule| rule.interface(DBUS))
        .and_then(|rule| rule.member("NameOwnerChanged"))
        .and_then(|rule| rule.add_arg(WATCHER_NAME))
        .map_err(|err| format!("could not build the tray match rule: {err}"))?
        .build();
    let messages = zbus::blocking::MessageIterator::for_match_rule(rule, &connection, Some(8))
        .map_err(|err| format!("could not watch for the tray: {err}"))?;
    std::thread::Builder::new()
        .name("tray-watch".into())
        .spawn(move || {
            for message in messages {
                let Ok(message) = message else { continue };
                let Ok((_, _, new_owner)) =
                    message.body().deserialize::<(String, String, String)>()
                else {
                    continue;
                };
                // An empty new owner is the tray going away; there is nothing to register with
                // until the next one.
                if !new_owner.is_empty() && appeared.send(()).is_err() {
                    return;
                }
            }
        })
        .map_err(|err| format!("could not start the tray thread: {err}"))?;
    Ok(())
}
