// SPDX-License-Identifier: GPL-3.0-or-later
//! The main loop.
//!
//! Four sources on one calloop loop: "NetworkManager said something" from [`crate::nm::watch`],
//! "a tray appeared" from [`crate::sni::watch_for_tray`], and the reload and quit pings from
//! [`crate::signals`]. The loop owns the last snapshot and the config, and is the only thing that
//! changes what the item shows.
//!
//! # Letting a burst settle
//!
//! NetworkManager is chatty. Joining a network moves the device through half a dozen states in a
//! second, and a background scan updates every access point in range, each its own
//! `PropertiesChanged`. A read per signal would be dozens of reads for one visible change, so a
//! signal only *arms* a short timer, and the read happens when it fires. Signals that arrive while
//! it is armed are already covered by the read to come.

use std::time::Duration;

use calloop::EventLoop;
use calloop::timer::{TimeoutAction, Timer};

use crate::config::Config;
use crate::indicator::{self, Status, View};
use crate::nm::{self, Snapshot};
use crate::sni::{self, Published};

/// How long after the first of a burst of NetworkManager signals the state is read. Short enough
/// that the icon changes as the network does; long enough to cover the run of state changes a
/// single connection attempt makes.
const SETTLE: Duration = Duration::from_millis(250);

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
    let published = sni::publish(first.clone())?;

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
