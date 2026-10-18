// SPDX-License-Identifier: GPL-3.0-or-later
//! The main loop.
//!
//! Five sources on one calloop loop: "NetworkManager said something" from [`crate::nm::watch`],
//! "a tray appeared" from [`crate::sni::watch_for_tray`], what was chosen in the menu from
//! [`crate::dbusmenu`], and the reload and quit pings from [`crate::signals`]. The loop owns the
//! last snapshot and the config, is the only thing that changes what the item and its menu show,
//! and is where NetworkManager is asked to do things.
//!
//! # Actions are fire and forget
//!
//! Joining a network, disconnecting and switching the radio are each one call, which
//! NetworkManager answers as soon as it has *started*. What happens next -- connecting, failing,
//! the radio coming up -- arrives as signals like any other change, and the icon and the menu
//! follow it from there. Only a refusal (no permission, no such network) is reported here, as a
//! notification, since nothing else would ever show it.
//!
//! # Letting a burst settle
//!
//! NetworkManager is chatty. Joining a network moves the device through half a dozen states in a
//! second, and a background scan updates every access point in range, each its own
//! `PropertiesChanged`. A read per signal would be dozens of reads for one visible change, so a
//! signal only *arms* a short timer, and the read happens when it fires. Signals that arrive while
//! it is armed are already covered by the read to come.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use calloop::EventLoop;
use calloop::timer::{TimeoutAction, Timer};

use crate::config::Config;
use crate::dbusmenu::MenuEvent;
use crate::indicator::{self, Status, View};
use crate::menu::{self, Action, Ids};
use crate::nm::{self, Snapshot};
use crate::sni::{self, Published};

/// How long after the first of a burst of NetworkManager signals the state is read. Short enough
/// that the icon changes as the network does; long enough to cover the run of state changes a
/// single connection attempt makes.
const SETTLE: Duration = Duration::from_millis(250);

/// The least time between scans asked for by opening the menu.
///
/// Not only politeness to NetworkManager, which refuses scans asked for too often anyway. A scan
/// changes the menu, a changed menu makes the tray fetch it again, and fetching it sends
/// `AboutToShow` -- so a scan on every `AboutToShow` would keep scanning for as long as the menu
/// was open.
const SCAN_COOLDOWN: Duration = Duration::from_secs(10);

struct Daemon {
    config: Config,
    system: zbus::blocking::Connection,
    published: Published,
    /// What NetworkManager said last. `None` when it did not answer.
    snapshot: Option<Snapshot>,
    /// Whether a read is already on its way.
    settling: bool,
    /// What the log last said was showing, so a line is written when that changes and not for
    /// every read.
    logged: Option<(String, Status)>,
    /// The ids networks have in the menu; see [`crate::menu`].
    ids: Ids,
    /// When the menu last asked for a scan.
    last_scan: Option<Instant>,
    exit: bool,
}

impl Daemon {
    /// Read NetworkManager again and show the result.
    fn refresh(&mut self) {
        self.snapshot = match nm::read(&self.system) {
            Ok(snapshot) => Some(snapshot),
            Err(err) => {
                // Reported once per change of answer rather than once per read.
                if self.snapshot.is_some() {
                    eprintln!("wlrix-network: NetworkManager is not answering: {err}");
                }
                None
            }
        };
        self.show();
    }

    fn show(&mut self) {
        let view = self.view();
        self.log(&view);
        self.published.show(view);
        let root = menu::build(self.snapshot.as_ref(), &mut self.ids);
        self.published.set_menu(root);
    }

    fn on_menu(&mut self, event: MenuEvent) {
        match event {
            MenuEvent::Opening => self.scan(),
            MenuEvent::Chosen(action) => self.act(action),
        }
    }

    /// Ask every Wi-Fi device to scan, unless one was asked recently.
    fn scan(&mut self) {
        if self
            .last_scan
            .is_some_and(|last| last.elapsed() < SCAN_COOLDOWN)
        {
            return;
        }
        self.last_scan = Some(Instant::now());
        for device in self.snapshot.iter().flat_map(|snapshot| &snapshot.wifi) {
            nm::request_scan(&self.system, &device.path);
        }
    }

    /// Do what a menu row says.
    fn act(&mut self, action: Action) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let Some(device) = snapshot.primary() else {
            return;
        };
        let result = match &action {
            Action::Wireless(on) => nm::set_wireless(&self.system, *on),
            Action::Disconnect => nm::disconnect(&self.system, &device.path),
            Action::Join(ssid) => {
                let current = device.access_point.as_ref().map(|ap| ap.ssid.as_str());
                if current == Some(ssid.as_str()) {
                    // Choosing the network already in use is choosing to stay on it.
                    return;
                }
                if let Some(saved) = device.saved.iter().find(|saved| &saved.ssid == ssid) {
                    nm::activate(&self.system, &saved.path, &device.path)
                } else if let Some(network) =
                    device.networks.iter().find(|network| &network.ssid == ssid)
                {
                    if network.enterprise {
                        self.notify(
                            &format!("{ssid} needs to be set up first"),
                            "Enterprise networks take a username and certificates, which will \
                             be set up in Network Settings. Until then, nm-connection-editor \
                             or nmcli can add it.",
                        );
                        return;
                    }
                    if network.secured {
                        // The password prompt is the next step. Until it exists, NetworkManager
                        // would ask for the key, find nobody to ask, and fail with "no secrets".
                        self.notify(
                            &format!("{ssid} needs a password"),
                            &format!(
                                "Joining a secured network for the first time needs the password \
                                 prompt, which is not finished yet. Join it once from a terminal \
                                 with: nmcli --ask device wifi connect \"{ssid}\""
                            ),
                        );
                        return;
                    }
                    nm::add_and_activate(&self.system, &device.path, &network.path)
                } else {
                    // Gone from the scan between the menu being drawn and the click.
                    return;
                }
            }
        };
        if let Err(err) = result {
            eprintln!("wlrix-network: {action:?} failed: {err}");
            self.notify("Network change failed", &refusal(&err));
        }
    }

    /// Post a notification. Not delivered is not reported beyond the log: the session's
    /// notification daemon is the only place it could be shown.
    fn notify(&self, summary: &str, body: &str) {
        let hints: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
        let actions: Vec<&str> = Vec::new();
        if let Err(err) = self.published.connection().call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "Network",
                0u32,
                "network-wireless-symbolic",
                summary,
                body,
                actions,
                hints,
                -1i32,
            ),
        ) {
            eprintln!("wlrix-network: could not post a notification ({err}): {summary}");
        }
    }

    /// One line in the session log when what is shown changes, so "the icon said disconnected at
    /// 14:02" can be checked against NetworkManager's own journal.
    fn log(&mut self, view: &View) {
        let now = (view.title.clone(), view.status);
        if self.logged.as_ref() != Some(&now) {
            eprintln!("wlrix-network: {} ({})", view.title, view.status.as_str());
            self.logged = Some(now);
        }
    }

    fn view(&self) -> View {
        indicator::view(self.snapshot.as_ref(), self.config.indicator)
    }
}

/// Run until stopped.
pub fn run() -> Result<(), String> {
    let config = Config::load();
    let system = zbus::blocking::Connection::system()
        .map_err(|err| format!("no system bus, so no NetworkManager to ask: {err}"))?;

    let snapshot = nm::read(&system).ok();
    if snapshot.is_none() {
        // Not fatal: NetworkManager may be starting, or restarting, and its name appearing is one
        // of the signals watched for.
        eprintln!("wlrix-network: NetworkManager is not on the system bus yet");
    }
    let first = indicator::view(snapshot.as_ref(), config.indicator);
    let mut ids = Ids::default();
    let root = menu::build(snapshot.as_ref(), &mut ids);
    let (menu_events, menu_channel) = calloop::channel::channel();
    let published = sni::publish(first.clone(), root, menu_events)?;

    let mut event_loop: EventLoop<Daemon> =
        EventLoop::try_new().map_err(|err| format!("could not create the event loop: {err}"))?;
    let handle = event_loop.handle();

    let (changed, changes) = calloop::channel::channel();
    nm::watch(system.clone(), changed)?;
    let settle_handle = handle.clone();
    handle
        .insert_source(changes, move |event, _, daemon: &mut Daemon| {
            if !matches!(event, calloop::channel::Event::Msg(())) || daemon.settling {
                return;
            }
            daemon.settling = true;
            let armed = settle_handle.insert_source(
                Timer::from_duration(SETTLE),
                |_, _, daemon: &mut Daemon| {
                    daemon.settling = false;
                    daemon.refresh();
                    TimeoutAction::Drop
                },
            );
            if let Err(err) = armed {
                // Read straight away instead; the burst costs a few extra reads, not the update.
                eprintln!("wlrix-network: could not arm the settle timer: {err}");
                daemon.settling = false;
                daemon.refresh();
            }
        })
        .map_err(|err| format!("could not listen to NetworkManager: {err}"))?;

    handle
        .insert_source(menu_channel, |event, _, daemon: &mut Daemon| {
            if let calloop::channel::Event::Msg(event) = event {
                daemon.on_menu(event);
            }
        })
        .map_err(|err| format!("could not listen to the menu: {err}"))?;

    let (appeared, appearances) = calloop::channel::channel();
    sni::watch_for_tray(&published, appeared)?;
    handle
        .insert_source(appearances, |event, _, daemon: &mut Daemon| {
            if let calloop::channel::Event::Msg(()) = event {
                daemon.published.register();
            }
        })
        .map_err(|err| format!("could not listen for the tray: {err}"))?;

    let (reload_ping, reload_source) = calloop::ping::make_ping()
        .map_err(|err| format!("could not create the reload ping: {err}"))?;
    handle
        .insert_source(reload_source, |_, _, daemon: &mut Daemon| {
            daemon.config = Config::load();
            daemon.show();
        })
        .map_err(|err| format!("could not watch for reloads: {err}"))?;
    crate::signals::forward_reload_to_loop(reload_ping);

    let (quit_ping, quit_source) = calloop::ping::make_ping()
        .map_err(|err| format!("could not create the quit ping: {err}"))?;
    handle
        .insert_source(quit_source, |_, _, daemon: &mut Daemon| daemon.exit = true)
        .map_err(|err| format!("could not watch for a stop: {err}"))?;
    crate::signals::forward_to_loop(quit_ping);

    // Registered last, once everything that keeps the item current is in place: the tray reads
    // the item the moment it is registered.
    published.register();

    let mut daemon = Daemon {
        config,
        system,
        published,
        snapshot,
        settling: false,
        logged: None,
        ids,
        last_scan: None,
        exit: false,
    };
    daemon.log(&first);
    let _pidfile = crate::pidfile::write();

    while !daemon.exit {
        event_loop
            .dispatch(None, &mut daemon)
            .map_err(|err| format!("event loop failed: {err}"))?;
    }
    Ok(())
}

/// Read NetworkManager once and say what the icon would show, for `--status`.
pub fn status() -> Result<(), String> {
    let config = Config::load();
    let system =
        zbus::blocking::Connection::system().map_err(|err| format!("no system bus: {err}"))?;
    let snapshot = nm::read(&system).ok();
    let view = indicator::view(snapshot.as_ref(), config.indicator);
    println!("status:  {}", view.status.as_str());
    println!("icon:    {}", view.icon);
    println!("title:   {}", view.title);
    for line in view.body.lines() {
        println!("         {line}");
    }
    match snapshot {
        None => println!("NetworkManager did not answer"),
        Some(snapshot) => {
            println!(
                "radio:   {} (hardware switch {})",
                if snapshot.wireless_enabled {
                    "on"
                } else {
                    "off"
                },
                if snapshot.hardware_enabled {
                    "on"
                } else {
                    "off"
                },
            );
            for device in &snapshot.wifi {
                println!("device:  {} {:?}", device.interface, device.state);
            }
            if snapshot.wifi.is_empty() {
                println!("device:  no Wi-Fi devices");
            }
        }
    }
    Ok(())
}

/// What NetworkManager said when it refused, without the D-Bus error name in front -- "Not
/// authorized to control networking." rather than "org.freedesktop.NetworkManager.PermissionDenied:
/// Not authorized to control networking."
fn refusal(err: &zbus::Error) -> String {
    match err {
        zbus::Error::MethodError(_, Some(description), _) => description.clone(),
        other => other.to_string(),
    }
}
