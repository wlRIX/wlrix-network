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
//! # Passwords
//!
//! A password is asked for in two ways, and both end in the same dialog ([`crate::prompt`]):
//!
//! - **By the menu**, before anything is created. Choosing a secured network nobody has saved, or
//!   Join Other Network, puts up the prompt first and creates the connection only once there is a
//!   key -- so a Cancel leaves nothing behind.
//! - **By NetworkManager**, through the secret agent ([`crate::agent`]): a saved network with no
//!   key, a per-user one whose key is in the keyring, and a key that did not work (`REQUEST_NEW`,
//!   which says so in the dialog).
//!
//! A connection created here that has never once connected is *fresh*. Cancelling its retry prompt
//! -- the password was wrong and the person gave up -- deletes it again, rather than leaving a
//! saved network with a bad password that autoconnect would keep trying.
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

use crate::agent::{self, AgentError, AgentEvent, SecretRequest, Settings};
use crate::config::Config;
use crate::dbusmenu::MenuEvent;
use crate::indicator::{self, Status, View};
use crate::keyring::Keyring;
use crate::menu::{self, Action, Ids};
use crate::nm::{self, DeviceState, Owner, Snapshot};
use crate::prompt::{self, Answer, Request};
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

/// A prompt on screen, and what its answer is for.
enum Pending {
    /// NetworkManager asked, through the agent.
    Agent {
        request: SecretRequest,
        reply: async_channel::Sender<Result<Settings, AgentError>>,
        child: std::process::Child,
    },
    /// The menu: a network in range that nobody has saved.
    Join {
        device: String,
        access_point: String,
        child: std::process::Child,
    },
    /// The menu: Join Other Network.
    Other {
        device: String,
        child: std::process::Child,
    },
}

impl Pending {
    fn child(&mut self) -> &mut std::process::Child {
        match self {
            Pending::Agent { child, .. }
            | Pending::Join { child, .. }
            | Pending::Other { child, .. } => child,
        }
    }
}

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
    /// Prompts on screen, by the token their answer comes back with.
    prompts: HashMap<u64, Pending>,
    next_token: u64,
    /// Where a prompt's answer is sent.
    answers: calloop::channel::Sender<(u64, Option<Answer>)>,
    /// Connections created here that have not yet connected once, by settings path, with the
    /// network's name. See the module comment.
    fresh: HashMap<String, String>,
    exit: bool,
}

impl Daemon {
    /// Read NetworkManager again and show the result.
    fn refresh(&mut self) {
        let was_running = self.snapshot.is_some();
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
        // NetworkManager restarted, and forgot every agent with it.
        if !was_running && self.snapshot.is_some() {
            self.register_agent();
        }
        self.forget_proven();
        self.show();
    }

    fn register_agent(&self) {
        match nm::register_agent(&self.system) {
            Ok(()) => eprintln!("wlrix-network: registered as NetworkManager's secret agent"),
            Err(err) => eprintln!("wlrix-network: could not register as a secret agent: {err}"),
        }
    }

    /// A fresh connection that has now connected is fresh no more: its password worked.
    fn forget_proven(&mut self) {
        let Some(connected) = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .wifi
                .iter()
                // A device *being* a hotspot is "connected" to its own network, which proves
                // nothing about a password.
                .filter(|device| device.mode == nm::Mode::Client)
                .filter(|device| device.state == DeviceState::Connected)
                .find_map(|device| device.access_point.as_ref().map(|ap| ap.ssid.clone()))
        }) else {
            return;
        };
        self.fresh.retain(|_, ssid| *ssid != connected);
    }

    /// Who a new connection belongs to: this user, with its key in the keyring, when there is a
    /// keyring to keep it in; everyone, with the key in NetworkManager, when there is not.
    fn owner(&self) -> Owner {
        match Keyring::open(self.published.connection()) {
            Some(_) => match user_name() {
                Some(name) => Owner::User(name),
                None => Owner::Everyone,
            },
            None => Owner::Everyone,
        }
    }

    /// Put up the prompt. `None` when it would not start, which has been said in the log and a
    /// notification.
    fn ask(&mut self, request: &Request) -> Option<(u64, std::process::Child)> {
        self.next_token += 1;
        let token = self.next_token;
        match prompt::spawn(request, token, self.answers.clone()) {
            Ok(child) => Some((token, child)),
            Err(err) => {
                eprintln!("wlrix-network: {err}");
                self.notify(
                    "The password prompt would not start",
                    &format!("{err}. Is wlrix-network-prompt installed?"),
                );
                None
            }
        }
    }

    /// NetworkManager, through the agent.
    fn on_agent(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::GetSecrets(request, reply) => self.get_secrets(request, reply),
            AgentEvent::Cancel {
                connection_path,
                setting,
            } => {
                let tokens: Vec<u64> = self
                    .prompts
                    .iter()
                    .filter(|(_, pending)| {
                        matches!(pending, Pending::Agent { request, .. }
                            if request.connection_path == connection_path
                                && request.setting == setting)
                    })
                    .map(|(token, _)| *token)
                    .collect();
                for token in tokens {
                    if let Some(mut pending) = self.prompts.remove(&token) {
                        let _ = pending.child().kill();
                        let _ = pending.child().wait();
                        if let Pending::Agent { reply, .. } = pending {
                            let _ = reply.try_send(Err(AgentError::UserCanceled(
                                "NetworkManager stopped asking".into(),
                            )));
                        }
                    }
                }
            }
            AgentEvent::Save(connection) => {
                let Some(keyring) = Keyring::open(self.published.connection()) else {
                    return;
                };
                let request = SecretRequest::parse(&connection, "", "", 0);
                for (setting, key, value) in agent::agent_owned_secrets(&connection) {
                    if let Err(err) =
                        keyring.store(&request.id, &request.uuid, &setting, &key, &value)
                    {
                        eprintln!("wlrix-network: could not save a secret in the keyring: {err}");
                    }
                }
            }
            AgentEvent::Delete(connection) => {
                let request = SecretRequest::parse(&connection, "", "", 0);
                if let Some(keyring) = Keyring::open(self.published.connection())
                    && let Err(err) = keyring.delete(&request.uuid)
                {
                    eprintln!("wlrix-network: could not delete secrets from the keyring: {err}");
                }
            }
        }
    }

    fn get_secrets(
        &mut self,
        request: SecretRequest,
        reply: async_channel::Sender<Result<Settings, AgentError>>,
    ) {
        let refuse = |reply: &async_channel::Sender<_>, why: &str| {
            let _ = reply.try_send(Err(AgentError::NoSecrets(why.to_owned())));
        };
        let Some(security) = request.security else {
            return refuse(&reply, "not a kind of network this agent asks about");
        };
        let Some(key) = security.secret_key() else {
            return refuse(&reply, "an open network has no secrets");
        };
        // A key in the keyring is handed back without asking -- unless it is the one that just
        // failed, which is what REQUEST_NEW says.
        if request.agent_owned
            && !request.request_new
            && let Some(keyring) = Keyring::open(self.published.connection())
            && let Ok(found) = keyring.find(&request.uuid, &request.setting)
            && let Some(value) = found.get(key)
        {
            let _ = reply.try_send(Ok(agent::secrets(&request.setting, key, value)));
            return;
        }
        if !request.allow_interaction {
            return refuse(&reply, "no stored key, and no one may be asked");
        }
        let ask = Request::Password {
            ssid: request.ssid.clone(),
            security,
            retry: request.request_new,
        };
        match self.ask(&ask) {
            Some((token, child)) => {
                self.prompts.insert(
                    token,
                    Pending::Agent {
                        request,
                        reply,
                        child,
                    },
                );
            }
            None => refuse(&reply, "the prompt would not start"),
        }
    }

    /// A prompt was answered, or cancelled.
    fn on_answer(&mut self, token: u64, answer: Option<Answer>) {
        let Some(mut pending) = self.prompts.remove(&token) else {
            return;
        };
        // Closed already, since its stdout has been read to the end; this only reaps it.
        let _ = pending.child().wait();
        match pending {
            Pending::Agent { request, reply, .. } => {
                let Some(answer) = answer else {
                    let _ = reply.try_send(Err(AgentError::UserCanceled("cancelled".into())));
                    if self.fresh.remove(&request.connection_path).is_some() {
                        // A connection created here, with a password that never worked, and the
                        // person has given up on it.
                        eprintln!(
                            "wlrix-network: removing {}, which never connected",
                            request.ssid
                        );
                        let _ = nm::delete_connection(&self.system, &request.connection_path);
                    }
                    return;
                };
                let key = request
                    .security
                    .and_then(|security| security.secret_key())
                    .unwrap_or("psk");
                if request.agent_owned
                    && !request.not_saved
                    && let Some(keyring) = Keyring::open(self.published.connection())
                    && let Err(err) = keyring.store(
                        &request.id,
                        &request.uuid,
                        &request.setting,
                        key,
                        &answer.password,
                    )
                {
                    eprintln!("wlrix-network: could not save the password in the keyring: {err}");
                }
                let _ = reply.try_send(Ok(agent::secrets(&request.setting, key, &answer.password)));
            }
            Pending::Join {
                device,
                access_point,
                ..
            } => {
                if let Some(answer) = answer {
                    self.create(&answer, &device, &access_point, false);
                }
            }
            Pending::Other { device, .. } => {
                if let Some(answer) = answer {
                    self.create(&answer, &device, "/", true);
                }
            }
        }
    }

    /// Create a connection from what the prompt was told, and start it.
    fn create(&mut self, answer: &Answer, device: &str, access_point: &str, hidden: bool) {
        let owner = self.owner();
        let security = (answer.security != prompt::Security::None).then_some(answer.security);
        let settings = nm::new_connection(&answer.ssid, security, &answer.password, hidden, &owner);
        match nm::add_and_activate(&self.system, &settings, device, access_point) {
            Ok(path) => {
                self.fresh.insert(path.to_string(), answer.ssid.clone());
                // A per-user connection's key is agent-owned: NetworkManager holds this one in
                // memory for the attempt and keeps it nowhere, so it goes in the keyring now.
                if let (Owner::User(_), Some(security)) = (&owner, security)
                    && let Some(key) = security.secret_key()
                    && let Some(keyring) = Keyring::open(self.published.connection())
                    && let Some((uuid, id)) = nm::identity(&self.system, path.as_str())
                    && let Err(err) = keyring.store(
                        &id,
                        &uuid,
                        "802-11-wireless-security",
                        key,
                        &answer.password,
                    )
                {
                    eprintln!("wlrix-network: could not save the password in the keyring: {err}");
                }
            }
            Err(err) => {
                eprintln!("wlrix-network: joining {} failed: {err}", answer.ssid);
                self.notify("Network change failed", &refusal(&err));
            }
        }
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
                    // A saved network with no key, or a per-user one whose key is in the
                    // keyring: NetworkManager asks the agent, and the prompt comes from there.
                    nm::activate(&self.system, &saved.path, &device.path)
                } else if let Some(network) =
                    device.networks.iter().find(|network| &network.ssid == ssid)
                {
                    if network.security == nm::ApSecurity::Enterprise {
                        self.notify(
                            &format!("{ssid} needs to be set up first"),
                            "Enterprise networks take a username and certificates, which will \
                             be set up in Network Settings. Until then, nm-connection-editor \
                             or nmcli can add it.",
                        );
                        return;
                    }
                    let (device_path, access_point) = (device.path.clone(), network.path.clone());
                    match network.security.prompt() {
                        Some(security) => {
                            let request = Request::Password {
                                ssid: ssid.clone(),
                                security,
                                retry: false,
                            };
                            if let Some((token, child)) = self.ask(&request) {
                                self.prompts.insert(
                                    token,
                                    Pending::Join {
                                        device: device_path,
                                        access_point,
                                        child,
                                    },
                                );
                            }
                            return;
                        }
                        None => {
                            // Open, or Enhanced Open: nothing to ask.
                            let answer = Answer {
                                ssid: ssid.clone(),
                                security: prompt::Security::None,
                                password: String::new(),
                            };
                            self.create(&answer, &device_path, &access_point, false);
                            return;
                        }
                    }
                } else {
                    // Gone from the scan between the menu being drawn and the click.
                    return;
                }
            }
            Action::JoinOther => {
                let device_path = device.path.clone();
                if let Some((token, child)) = self.ask(&Request::Other) {
                    self.prompts.insert(
                        token,
                        Pending::Other {
                            device: device_path,
                            child,
                        },
                    );
                }
                return;
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

/// How long any call this daemon makes may take before it is given up on.
///
/// Without one, a call to a service that never answers waits forever, and this loop waits with it:
/// the icon stops changing and the menu stops working. That is not hypothetical -- a keyring daemon
/// that hands its bus name to another process mid-call leaves exactly such a call behind. Ten
/// seconds is far past anything NetworkManager or the keyring takes, and well short of a person
/// giving up on the menu. Calls *to* this daemon are not affected: the agent's `GetSecrets` may
/// wait as long as the person at the prompt does.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);

fn system_bus() -> zbus::Result<zbus::blocking::Connection> {
    zbus::blocking::connection::Builder::system()?
        .method_timeout(CALL_TIMEOUT)
        .build()
}

/// Run until stopped.
pub fn run() -> Result<(), String> {
    let config = Config::load();
    let system =
        system_bus().map_err(|err| format!("no system bus, so no NetworkManager to ask: {err}"))?;

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

    // The secret agent, on the system bus beside the NetworkManager reads.
    let (agent_events, agent_channel) = calloop::channel::channel();
    system
        .object_server()
        .at(
            agent::AGENT_PATH,
            agent::Agent {
                events: agent_events,
            },
        )
        .map_err(|err| format!("could not serve the secret agent: {err}"))?;
    handle
        .insert_source(agent_channel, |event, _, daemon: &mut Daemon| {
            if let calloop::channel::Event::Msg(event) = event {
                daemon.on_agent(event);
            }
        })
        .map_err(|err| format!("could not listen to the secret agent: {err}"))?;

    let (answers, answer_channel) = calloop::channel::channel();
    handle
        .insert_source(answer_channel, |event, _, daemon: &mut Daemon| {
            if let calloop::channel::Event::Msg((token, answer)) = event {
                daemon.on_answer(token, answer);
            }
        })
        .map_err(|err| format!("could not listen for answers: {err}"))?;

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
        prompts: HashMap::new(),
        next_token: 0,
        answers,
        fresh: HashMap::new(),
        exit: false,
    };
    daemon.log(&first);
    if daemon.snapshot.is_some() {
        daemon.register_agent();
    }
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
    let system = system_bus().map_err(|err| format!("no system bus: {err}"))?;
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

/// The user's login name, which a per-user connection's `permissions` names.
fn user_name() -> Option<String> {
    // SAFETY: getpwuid returns a pointer into static storage, read straight away and copied, on
    // the one thread that ever calls it.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() || (*entry).pw_name.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr((*entry).pw_name)
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}
