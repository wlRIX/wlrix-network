// SPDX-License-Identifier: GPL-3.0-or-later
//! NetworkManager, read over the system bus.
//!
//! # Read it all again
//!
//! NetworkManager announces changes with `PropertiesChanged` on whichever object changed -- the
//! manager, a device, an access point, an IP configuration -- and the objects point at each other
//! by path, so following each signal to the part of the picture it changes means keeping a copy
//! of NetworkManager's object graph and patching it. Instead, any signal means "read the state
//! again": the manager, its Wi-Fi devices, the access point each is on, and that device's
//! address. It is a dozen round trips on a local bus, and the main loop lets a burst of signals
//! settle before asking (a scan alone sends dozens), so it is cheap and cannot drift.
//!
//! The same reasoning as the tray's "every change signal means read it all again", for the same
//! reason: five slightly different patch paths are five places to be wrong.
//!
//! # What is read
//!
//! What the indicator and the menu show: the radio, each Wi-Fi device's state, the network it is
//! on and what its last scan found, and the saved connections it could use. Nothing else of
//! nothing written. Those come with the menu.

use std::collections::HashMap;

use zbus::MatchRule;
use zbus::message::Type;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::prompt::Security;

/// NetworkManager's bus name and root object.
pub const NAME: &str = "org.freedesktop.NetworkManager";
const ROOT: &str = "/org/freedesktop/NetworkManager";
const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const ACCESS_POINT: &str = "org.freedesktop.NetworkManager.AccessPoint";
const IP4_CONFIG: &str = "org.freedesktop.NetworkManager.IP4Config";
const SETTINGS_CONNECTION: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const DBUS: &str = "org.freedesktop.DBus";

/// `NM_DEVICE_TYPE_WIFI`.
const DEVICE_TYPE_WIFI: u32 = 2;
/// `NM_802_11_AP_FLAGS_PRIVACY`: WEP, or a key of some kind.
const AP_PRIVACY: u32 = 0x1;
/// `NM_802_11_AP_SEC_KEY_MGMT_*`: how an access point says what its keys are.
const KEY_MGMT_PSK: u32 = 0x100;
/// Enterprise Wi-Fi: a username and a certificate rather than a password.
const KEY_MGMT_802_1X: u32 = 0x200;
/// WPA3 Personal.
const KEY_MGMT_SAE: u32 = 0x400;
/// Enhanced Open: encrypted, but with no password -- and its transition mode.
const KEY_MGMT_OWE: u32 = 0x800 | 0x1000;

/// The agent manager, which a secret agent registers with.
const AGENT_MANAGER_PATH: &str = "/org/freedesktop/NetworkManager/AgentManager";
const AGENT_MANAGER: &str = "org.freedesktop.NetworkManager.AgentManager";
/// What this daemon registers as. Reverse-DNS, as NetworkManager requires of an identifier.
const AGENT_IDENTIFIER: &str = "com.wlrix.network";

/// Where a device is, from `NMDeviceState`, folded into what the indicator distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    /// Unmanaged, or not ready -- no firmware, rfkilled, or a radio being brought up.
    Unavailable,
    Disconnected,
    /// Anywhere between choosing a network and having an address: preparing, configuring,
    /// waiting for a password, getting an address, checking it, or bringing up secondaries.
    Connecting,
    Connected,
    /// Taking a connection down.
    Disconnecting,
    /// The last attempt failed. NetworkManager moves on to disconnected shortly after.
    Failed,
}

impl DeviceState {
    /// `NMDeviceState`'s numbers. Anything not listed -- a newer NetworkManager's addition -- is
    /// read as unavailable, which shows the quietest icon rather than a wrong one.
    pub fn from_code(code: u32) -> Self {
        match code {
            30 => Self::Disconnected,
            40..=90 => Self::Connecting,
            100 => Self::Connected,
            110 => Self::Disconnecting,
            120 => Self::Failed,
            _ => Self::Unavailable,
        }
    }
}

/// The internet as NetworkManager last checked it, from `NMConnectivityState`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Connectivity {
    /// Not checked, or checking is turned off.
    #[default]
    Unknown,
    None,
    /// A captive portal: connected, but every request goes to a sign-in page.
    Portal,
    /// Connected to a network that does not reach the internet.
    Limited,
    Full,
}

impl Connectivity {
    pub fn from_code(code: u32) -> Self {
        match code {
            1 => Self::None,
            2 => Self::Portal,
            3 => Self::Limited,
            4 => Self::Full,
            _ => Self::Unknown,
        }
    }
}

/// The network a Wi-Fi device is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessPoint {
    pub ssid: String,
    /// 0 to 100.
    pub strength: u8,
    /// Whether joining takes a key of any kind.
    pub secured: bool,
}

/// One network in range, as the menu lists it: every access point broadcasting the same name
/// folded into one, the strongest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub ssid: String,
    pub strength: u8,
    pub security: ApSecurity,
    /// The strongest access point's object, which is what NetworkManager is asked to join.
    pub path: String,
}

/// What it takes to join a network, from what its access points advertise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApSecurity {
    Open,
    /// Enhanced Open (OWE): encrypted, but anyone may join, so no password.
    EnhancedOpen,
    Wep,
    WpaPsk,
    Sae,
    /// 802.1X. Set up in Network Settings rather than from a menu.
    Enterprise,
}

impl ApSecurity {
    /// Read the three flag words an access point carries.
    ///
    /// In order of preference, because one access point may offer several: a network offering
    /// both WPA2 and WPA3 (transition mode) is joined as WPA2, which every driver can do, with the
    /// same password. Enterprise wins over a password only when no password mode is offered.
    fn from_flags(privacy: u32, wpa: u32, rsn: u32) -> Self {
        let key_mgmt = wpa | rsn;
        if key_mgmt & KEY_MGMT_PSK != 0 {
            Self::WpaPsk
        } else if key_mgmt & KEY_MGMT_SAE != 0 {
            Self::Sae
        } else if key_mgmt & KEY_MGMT_802_1X != 0 {
            Self::Enterprise
        } else if key_mgmt & KEY_MGMT_OWE != 0 {
            Self::EnhancedOpen
        } else if privacy & AP_PRIVACY != 0 {
            Self::Wep
        } else {
            Self::Open
        }
    }

    /// Whether joining takes a password typed into the prompt.
    pub fn needs_password(self) -> bool {
        self.prompt().is_some()
    }

    /// The security the prompt is asked about, for the kinds it can ask about.
    pub fn prompt(self) -> Option<Security> {
        match self {
            Self::WpaPsk => Some(Security::WpaPsk),
            Self::Sae => Some(Security::Sae),
            Self::Wep => Some(Security::Wep),
            Self::Open | Self::EnhancedOpen | Self::Enterprise => None,
        }
    }
}

/// Who may use a new connection, and so where its password lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// This user only, with the password in their keyring (agent-owned).
    User(String),
    /// Everyone on the machine, with the password in NetworkManager's own system profile.
    Everyone,
}

/// A saved Wi-Fi connection -- a network NetworkManager knows how to join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub ssid: String,
    /// Its object under `Settings`, which is what `ActivateConnection` takes.
    pub path: String,
}

/// What a Wi-Fi device is doing with its radio, from `NM80211Mode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// Joined to someone else's network, or trying to be -- the ordinary case.
    #[default]
    Client,
    /// Being the network: a hotspot sharing this computer's connection.
    Hotspot,
}

impl Mode {
    /// `NM_802_11_MODE_AP` is 3. Ad-hoc and mesh are read as client, which is what they look like
    /// from a menu.
    fn from_code(code: u32) -> Self {
        if code == 3 {
            Self::Hotspot
        } else {
            Self::Client
        }
    }
}

/// One Wi-Fi device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiDevice {
    /// Its object, which actions are addressed to.
    pub path: String,
    pub interface: String,
    pub state: DeviceState,
    pub mode: Mode,
    pub access_point: Option<AccessPoint>,
    /// The first IPv4 address, as `address/prefix`.
    pub ipv4: Option<String>,
    /// What the last scan found, strongest first. Hidden networks, which broadcast no name, are
    /// left out: there is nothing to list them as.
    pub networks: Vec<Network>,
    /// The saved Wi-Fi connections this device can use.
    ///
    /// NetworkManager's own answer -- the device's `AvailableConnections` -- rather than every
    /// saved Wi-Fi connection matched up by name. A profile tied to another interface, or one that
    /// makes a hotspot rather than joining one, shares a network's name and still cannot join it
    /// from here; trying fails with "profile is not compatible with device".
    pub saved: Vec<Saved>,
}

/// Everything the indicator is drawn from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// The software switch: `nmcli radio wifi off`, or airplane mode.
    pub wireless_enabled: bool,
    /// The hardware switch, a key or a slider on the machine. Off means the software switch
    /// cannot turn the radio back on.
    pub hardware_enabled: bool,
    pub connectivity: Connectivity,
    pub wifi: Vec<WifiDevice>,
}

impl Snapshot {
    /// The device the indicator describes: the connected one if any, then one on its way to
    /// being connected, then the first. A machine with two radios is rare, and one of them in use
    /// is what a person wants to know about.
    ///
    /// A hotspot comes last. NetworkManager calls a device sharing a hotspot "activated", and
    /// ranked by state alone it would be described as connected to its own network -- at no
    /// signal at all, since there is no access point but itself.
    pub fn primary(&self) -> Option<&WifiDevice> {
        let rank = |device: &&WifiDevice| match (device.mode, device.state) {
            (Mode::Hotspot, _) => 3,
            (_, DeviceState::Connected) => 0,
            (_, DeviceState::Connecting) => 1,
            _ => 2,
        };
        self.wifi.iter().min_by_key(rank)
    }
}

/// Read the whole state. `Err` when NetworkManager is not on the bus, or stopped answering
/// halfway -- the caller treats both as "nothing to show".
pub fn read(connection: &zbus::blocking::Connection) -> zbus::Result<Snapshot> {
    let manager = get_all(connection, ROOT, NAME)?;
    let devices: Vec<OwnedObjectPath> = take(&manager, "Devices").unwrap_or_default();

    let mut wifi = Vec::new();
    for path in devices {
        // One small call per device before the big one, since most devices are not Wi-Fi --
        // this machine's Docker bridges and veths outnumber everything else.
        let kind: u32 = get(connection, path.as_str(), DEVICE, "DeviceType").unwrap_or(0);
        if kind != DEVICE_TYPE_WIFI {
            continue;
        }
        // A device that vanished between the list and this read is skipped, not an error: the
        // DeviceRemoved behind it will bring another read.
        let Ok(device) = get_all(connection, path.as_str(), DEVICE) else {
            continue;
        };
        let state = DeviceState::from_code(take(&device, "State").unwrap_or(0));
        let mode = Mode::from_code(get(connection, path.as_str(), WIRELESS, "Mode").unwrap_or(2));
        let access_point =
            get::<OwnedObjectPath>(connection, path.as_str(), WIRELESS, "ActiveAccessPoint")
                .ok()
                .filter(|ap| ap.as_str() != "/")
                .and_then(|ap| get_all(connection, ap.as_str(), ACCESS_POINT).ok())
                .map(|properties| access_point(&properties));
        let ipv4 = take::<OwnedObjectPath>(&device, "Ip4Config")
            .filter(|config| config.as_str() != "/")
            .and_then(|config| {
                get::<Vec<HashMap<String, OwnedValue>>>(
                    connection,
                    config.as_str(),
                    IP4_CONFIG,
                    "AddressData",
                )
                .ok()
            })
            .and_then(|addresses| first_address(&addresses));
        let scanned: Vec<(String, HashMap<String, OwnedValue>)> =
            get::<Vec<OwnedObjectPath>>(connection, path.as_str(), WIRELESS, "AccessPoints")
                .unwrap_or_default()
                .into_iter()
                .filter_map(|ap| {
                    let properties = get_all(connection, ap.as_str(), ACCESS_POINT).ok()?;
                    Some((ap.to_string(), properties))
                })
                .collect();
        wifi.push(WifiDevice {
            path: path.to_string(),
            interface: take(&device, "Interface").unwrap_or_default(),
            state,
            mode,
            access_point,
            ipv4,
            networks: networks(&scanned),
            saved: saved_connections(
                connection,
                &take::<Vec<OwnedObjectPath>>(&device, "AvailableConnections").unwrap_or_default(),
            ),
        });
    }

    Ok(Snapshot {
        wireless_enabled: take(&manager, "WirelessEnabled").unwrap_or(false),
        hardware_enabled: take(&manager, "WirelessHardwareEnabled").unwrap_or(true),
        connectivity: Connectivity::from_code(take(&manager, "Connectivity").unwrap_or(0)),
        wifi,
    })
}

/// Fold a scan into one entry per network name, strongest first.
///
/// A home with a mesh, an office, a campus: one name, many access points. Listing each would put
/// the same network in the menu five times, and choosing between them is NetworkManager's job --
/// it roams between them on its own. The strongest one's properties stand for the network.
fn networks(scanned: &[(String, HashMap<String, OwnedValue>)]) -> Vec<Network> {
    let mut best: HashMap<String, Network> = HashMap::new();
    for (path, properties) in scanned {
        let ssid: Vec<u8> = take(properties, "Ssid").unwrap_or_default();
        if ssid.iter().all(|&byte| byte == 0) {
            continue;
        }
        let point = access_point(properties);
        let flag = |name: &str| take::<u32>(properties, name).unwrap_or(0);
        let network = Network {
            ssid: point.ssid,
            strength: point.strength,
            security: ApSecurity::from_flags(flag("Flags"), flag("WpaFlags"), flag("RsnFlags")),
            path: path.clone(),
        };
        match best.get(&network.ssid) {
            Some(kept) if kept.strength >= network.strength => {}
            _ => {
                best.insert(network.ssid.clone(), network);
            }
        }
    }
    let mut list: Vec<Network> = best.into_values().collect();
    list.sort_by(|a, b| {
        b.strength
            .cmp(&a.strength)
            .then_with(|| a.ssid.cmp(&b.ssid))
    });
    list
}

/// The Wi-Fi connections among `paths` that join a network.
///
/// One `GetSettings` each. Secrets are never returned by that call, which is as it should be:
/// nothing here needs them.
fn saved_connections(
    connection: &zbus::blocking::Connection,
    paths: &[OwnedObjectPath],
) -> Vec<Saved> {
    paths
        .iter()
        .filter_map(|path| {
            let reply = connection
                .call_method(
                    Some(NAME),
                    path.as_str(),
                    Some(SETTINGS_CONNECTION),
                    "GetSettings",
                    &(),
                )
                .ok()?;
            let settings: HashMap<String, HashMap<String, OwnedValue>> =
                reply.body().deserialize().ok()?;
            saved_wifi(&settings).map(|ssid| Saved {
                ssid,
                path: path.to_string(),
            })
        })
        .collect()
}

/// The network a saved connection joins, if it is a Wi-Fi connection that joins one. A hotspot's
/// profile names its network too, and "joining" through it would start a hotspot instead.
fn saved_wifi(settings: &HashMap<String, HashMap<String, OwnedValue>>) -> Option<String> {
    let kind: String = take(settings.get("connection")?, "type")?;
    if kind != "802-11-wireless" {
        return None;
    }
    let wireless = settings.get("802-11-wireless")?;
    let mode: String = take(wireless, "mode").unwrap_or_default();
    if mode == "ap" || mode == "adhoc" || mode == "mesh" {
        return None;
    }
    let ssid: Vec<u8> = take(wireless, "ssid")?;
    Some(ssid_text(&ssid))
}

/// Turn the software radio switch on or off -- what `nmcli radio wifi` does.
///
/// Allowed for the active local session by NetworkManager's default polkit policy; a stricter
/// policy brings up the polkit agent.
pub fn set_wireless(connection: &zbus::blocking::Connection, on: bool) -> zbus::Result<()> {
    connection.call_method(
        Some(NAME),
        ROOT,
        Some(PROPERTIES),
        "Set",
        &(NAME, "WirelessEnabled", Value::from(on)),
    )?;
    Ok(())
}

/// Join a network through a saved connection. `"/"` for the specific object lets NetworkManager
/// pick the access point, which is what roaming needs.
pub fn activate(
    connection: &zbus::blocking::Connection,
    saved: &str,
    device: &str,
) -> zbus::Result<()> {
    let object = |path: &str| ObjectPath::try_from(path.to_owned()).map_err(zbus::Error::from);
    connection.call_method(
        Some(NAME),
        ROOT,
        Some(NAME),
        "ActivateConnection",
        &(object(saved)?, object(device)?, object("/")?),
    )?;
    Ok(())
}

/// The settings for a connection to a network NetworkManager does not know yet.
///
/// Everything a hidden network needs, since nothing can be filled in from a scan; for a visible
/// one NetworkManager completes the rest from the access point named alongside. `password` is
/// ignored when there is no `security`.
///
/// The owner decides two things together, because they only make sense together: who may use the
/// connection (`connection.permissions`) and who stores its key (the secret's flags). A per-user
/// connection's key is **agent-owned** -- flag 1 -- so NetworkManager never writes it to
/// `/etc/NetworkManager/system-connections` and asks this daemon, which keeps it in the keyring,
/// whenever it needs it.
pub fn new_connection(
    ssid: &str,
    security: Option<Security>,
    password: &str,
    hidden: bool,
    owner: &Owner,
) -> HashMap<&'static str, HashMap<&'static str, OwnedValue>> {
    let value = |v: Value| OwnedValue::try_from(v).expect("a plain value is ownable");
    let mut connection = HashMap::from([
        ("id", value(Value::from(ssid.to_owned()))),
        ("type", value(Value::from("802-11-wireless"))),
    ]);
    if let Owner::User(name) = owner {
        // `user:<name>:` -- the trailing colon is the field for a reserved part, and is how
        // NetworkManager itself writes the property.
        connection.insert(
            "permissions",
            value(Value::from(vec![format!("user:{name}:")])),
        );
    }
    let mut wireless = HashMap::from([
        ("ssid", value(Value::from(ssid.as_bytes().to_vec()))),
        ("mode", value(Value::from("infrastructure"))),
    ]);
    if hidden {
        wireless.insert("hidden", value(Value::from(true)));
    }
    let mut settings = HashMap::from([("connection", connection), ("802-11-wireless", wireless)]);
    if let Some(security) = security
        && let (Some(key), Some(flags)) = (security.secret_key(), security.flags_key())
    {
        let agent_owned: u32 = match owner {
            Owner::User(_) => 1,
            Owner::Everyone => 0,
        };
        let mut section = HashMap::from([
            ("key-mgmt", value(Value::from(security.key_mgmt()))),
            (key, value(Value::from(password.to_owned()))),
            (flags, value(Value::from(agent_owned))),
        ]);
        if security == Security::Wep {
            // Key type 1, a hex or ASCII key: the only kind the prompt accepts.
            section.insert("wep-key-type", value(Value::from(1u32)));
        }
        settings.insert("802-11-wireless-security", section);
    }
    settings
}

/// Create a connection and start it, on `device`, at `access_point` -- `"/"` to let NetworkManager
/// find it, which a hidden network needs. Returns the new connection's settings object.
pub fn add_and_activate(
    connection: &zbus::blocking::Connection,
    settings: &HashMap<&'static str, HashMap<&'static str, OwnedValue>>,
    device: &str,
    access_point: &str,
) -> zbus::Result<OwnedObjectPath> {
    let object = |path: &str| ObjectPath::try_from(path.to_owned()).map_err(zbus::Error::from);
    let reply = connection.call_method(
        Some(NAME),
        ROOT,
        Some(NAME),
        "AddAndActivateConnection",
        &(settings, object(device)?, object(access_point)?),
    )?;
    let (path, _active): (OwnedObjectPath, OwnedObjectPath) = reply.body().deserialize()?;
    Ok(path)
}

/// A saved connection's uuid and name, which is what the keyring files its secrets under.
pub fn identity(connection: &zbus::blocking::Connection, path: &str) -> Option<(String, String)> {
    let reply = connection
        .call_method(
            Some(NAME),
            path,
            Some(SETTINGS_CONNECTION),
            "GetSettings",
            &(),
        )
        .ok()?;
    let settings: HashMap<String, HashMap<String, OwnedValue>> = reply.body().deserialize().ok()?;
    let section = settings.get("connection")?;
    Some((take(section, "uuid")?, take(section, "id")?))
}

/// Remove a saved connection.
pub fn delete_connection(connection: &zbus::blocking::Connection, path: &str) -> zbus::Result<()> {
    connection.call_method(Some(NAME), path, Some(SETTINGS_CONNECTION), "Delete", &())?;
    Ok(())
}

/// Tell NetworkManager this process answers for the user's secrets. It has to be said again
/// whenever NetworkManager restarts, which forgets every agent.
pub fn register_agent(connection: &zbus::blocking::Connection) -> zbus::Result<()> {
    connection.call_method(
        Some(NAME),
        AGENT_MANAGER_PATH,
        Some(AGENT_MANAGER),
        "Register",
        &(AGENT_IDENTIFIER,),
    )?;
    Ok(())
}

/// Take a device off whatever network it is on. NetworkManager will not autoconnect it again
/// until something asks it to, which is what "Disconnect" in a menu means.
pub fn disconnect(connection: &zbus::blocking::Connection, device: &str) -> zbus::Result<()> {
    connection.call_method(Some(NAME), device, Some(DEVICE), "Disconnect", &())?;
    Ok(())
}

/// Ask a device to scan. NetworkManager refuses a scan asked for too soon after the last, which is
/// not worth reporting: the last scan's results are still there.
pub fn request_scan(connection: &zbus::blocking::Connection, device: &str) {
    let options: HashMap<&str, Value> = HashMap::new();
    let _ = connection.call_method(
        Some(NAME),
        device,
        Some(WIRELESS),
        "RequestScan",
        &(options,),
    );
}

/// An access point's properties, read into what is shown.
fn access_point(properties: &HashMap<String, OwnedValue>) -> AccessPoint {
    let ssid: Vec<u8> = take(properties, "Ssid").unwrap_or_default();
    let flag = |name: &str| take::<u32>(properties, name).unwrap_or(0);
    AccessPoint {
        ssid: ssid_text(&ssid),
        strength: take::<u8>(properties, "Strength").unwrap_or(0).min(100),
        // Privacy (WEP) in `Flags`, or any WPA/RSN key management at all.
        secured: flag("Flags") & AP_PRIVACY != 0 || flag("WpaFlags") != 0 || flag("RsnFlags") != 0,
    }
}

/// An SSID as text. It is up to 32 arbitrary bytes, not a string: nearly always UTF-8, but a
/// router may broadcast Latin-1 or garbage, and a hidden network broadcasts nothing at all.
fn ssid_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_suffix(b"\0").unwrap_or(bytes);
    if bytes.is_empty() || bytes.iter().all(|&byte| byte == 0) {
        return "(hidden network)".to_owned();
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// The first `address/prefix` in an `AddressData` list.
fn first_address(addresses: &[HashMap<String, OwnedValue>]) -> Option<String> {
    let first = addresses.first()?;
    let address: String = take(first, "address")?;
    let prefix: u32 = take(first, "prefix").unwrap_or(0);
    Some(if prefix > 0 {
        format!("{address}/{prefix}")
    } else {
        address
    })
}

/// One property out of a `GetAll` map, converted.
fn take<T>(map: &HashMap<String, OwnedValue>, key: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    map.get(key)?.try_clone().ok()?.try_into().ok()
}

fn get_all(
    connection: &zbus::blocking::Connection,
    path: &str,
    interface: &str,
) -> zbus::Result<HashMap<String, OwnedValue>> {
    let reply =
        connection.call_method(Some(NAME), path, Some(PROPERTIES), "GetAll", &(interface,))?;
    reply.body().deserialize()
}

fn get<T>(
    connection: &zbus::blocking::Connection,
    path: &str,
    interface: &str,
    property: &str,
) -> zbus::Result<T>
where
    T: TryFrom<OwnedValue>,
    T::Error: Into<zbus::Error>,
{
    let reply = connection.call_method(
        Some(NAME),
        path,
        Some(PROPERTIES),
        "Get",
        &(interface, property),
    )?;
    let value: OwnedValue = reply.body().deserialize()?;
    T::try_from(value).map_err(Into::into)
}

/// Ask the bus for every signal that can change what the indicator or the menu shows: every
/// signal NetworkManager sends under its root -- property changes on any of its objects,
/// `DeviceAdded` and friends, a scan's `AccessPointAdded`, a saved connection's `Updated` and
/// `Removed` -- and its name coming and going, which is what a restart of NetworkManager looks
/// like.
pub fn subscribe(connection: &zbus::blocking::Connection) -> zbus::Result<()> {
    let bus = zbus::blocking::fdo::DBusProxy::new(connection)?;
    bus.add_match_rule(
        MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(NAME)?
            .path_namespace(ROOT)?
            .build(),
    )?;
    bus.add_match_rule(
        MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(DBUS)?
            .interface(DBUS)?
            .member("NameOwnerChanged")?
            .add_arg(NAME)?
            .build(),
    )?;
    Ok(())
}

/// Start the thread that turns NetworkManager's signals into "something changed" on the loop.
///
/// It does not say *what* changed -- see the module comment -- so every signal is the same unit
/// event, and the loop's settle timer collapses a burst of them into one read.
pub fn watch(
    connection: zbus::blocking::Connection,
    changed: calloop::channel::Sender<()>,
) -> Result<(), String> {
    subscribe(&connection).map_err(|err| format!("could not watch NetworkManager: {err}"))?;
    std::thread::Builder::new()
        .name("networkmanager".into())
        .spawn(move || {
            for message in zbus::blocking::MessageIterator::from(&connection) {
                let Ok(message) = message else { continue };
                // The reads share this connection, so their replies come through here too.
                if message.message_type() != Type::Signal {
                    continue;
                }
                if changed.send(()).is_err() {
                    return;
                }
            }
        })
        .map_err(|err| format!("could not start the NetworkManager thread: {err}"))?;
    Ok(())
}

/// A value in a test property map.
#[cfg(test)]
fn owned<'a>(value: impl Into<Value<'a>>) -> OwnedValue {
    value.into().try_into().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_states_fold_into_what_is_shown() {
        assert_eq!(
            DeviceState::from_code(10),
            DeviceState::Unavailable,
            "unmanaged"
        );
        assert_eq!(DeviceState::from_code(20), DeviceState::Unavailable);
        assert_eq!(DeviceState::from_code(30), DeviceState::Disconnected);
        for connecting in [40, 50, 60, 70, 80, 90] {
            assert_eq!(DeviceState::from_code(connecting), DeviceState::Connecting);
        }
        assert_eq!(DeviceState::from_code(100), DeviceState::Connected);
        assert_eq!(DeviceState::from_code(110), DeviceState::Disconnecting);
        assert_eq!(DeviceState::from_code(120), DeviceState::Failed);
        assert_eq!(DeviceState::from_code(999), DeviceState::Unavailable);
    }

    #[test]
    fn an_access_point_reads_its_name_strength_and_security() {
        let mut properties = HashMap::new();
        properties.insert("Ssid".to_owned(), owned(b"HomeNet".to_vec()));
        properties.insert("Strength".to_owned(), owned(72u8));
        properties.insert("Flags".to_owned(), owned(0u32));
        properties.insert("WpaFlags".to_owned(), owned(0u32));
        properties.insert("RsnFlags".to_owned(), owned(0x188u32));
        assert_eq!(
            access_point(&properties),
            AccessPoint {
                ssid: "HomeNet".into(),
                strength: 72,
                secured: true,
            }
        );
        properties.insert("RsnFlags".to_owned(), owned(0u32));
        assert!(!access_point(&properties).secured, "an open network");
    }

    #[test]
    fn an_ssid_is_bytes_not_a_string() {
        assert_eq!(ssid_text(b"Caf\xc3\xa9"), "Café");
        assert_eq!(
            ssid_text(b"Caf\xe9"),
            "Caf\u{fffd}",
            "Latin-1 survives as a replacement"
        );
        assert_eq!(ssid_text(b""), "(hidden network)");
        assert_eq!(ssid_text(&[0, 0, 0]), "(hidden network)");
    }

    #[test]
    fn the_first_address_is_shown_with_its_prefix() {
        let mut first = HashMap::new();
        first.insert("address".to_owned(), owned("192.168.1.20"));
        first.insert("prefix".to_owned(), owned(24u32));
        let mut second = HashMap::new();
        second.insert("address".to_owned(), owned("10.0.0.2"));
        assert_eq!(
            first_address(&[first, second]).as_deref(),
            Some("192.168.1.20/24")
        );
        assert_eq!(first_address(&[]), None);
    }

    fn scanned(
        ssid: &[u8],
        strength: u8,
        rsn: u32,
        path: &str,
    ) -> (String, HashMap<String, OwnedValue>) {
        let mut properties = HashMap::new();
        properties.insert("Ssid".to_owned(), owned(ssid.to_vec()));
        properties.insert("Strength".to_owned(), owned(strength));
        properties.insert(
            "Flags".to_owned(),
            owned(if rsn != 0 { AP_PRIVACY } else { 0 }),
        );
        properties.insert("WpaFlags".to_owned(), owned(0u32));
        properties.insert("RsnFlags".to_owned(), owned(rsn));
        (path.to_owned(), properties)
    }

    #[test]
    fn a_scan_folds_into_one_entry_per_name_strongest_first() {
        let list = networks(&[
            scanned(b"Mesh", 40, 0x188, "/ap/1"),
            scanned(b"Cafe", 55, 0, "/ap/2"),
            scanned(b"Mesh", 80, 0x188, "/ap/3"),
            scanned(b"", 99, 0, "/ap/4"),
            scanned(b"Campus", 30, 0x200, "/ap/5"),
        ]);
        let names: Vec<&str> = list.iter().map(|n| n.ssid.as_str()).collect();
        assert_eq!(
            names,
            ["Mesh", "Cafe", "Campus"],
            "the hidden one is left out"
        );
        assert_eq!(
            list[0].path, "/ap/3",
            "the strongest access point stands for the network"
        );
        assert_eq!(list[0].security, ApSecurity::WpaPsk);
        assert_eq!(list[1].security, ApSecurity::Open);
        assert_eq!(list[2].security, ApSecurity::Enterprise);
    }

    #[test]
    fn security_is_read_from_the_flags_in_order_of_preference() {
        assert_eq!(ApSecurity::from_flags(0, 0, 0), ApSecurity::Open);
        assert_eq!(ApSecurity::from_flags(1, 0, 0), ApSecurity::Wep);
        assert_eq!(ApSecurity::from_flags(1, 0, 0x188), ApSecurity::WpaPsk);
        assert_eq!(ApSecurity::from_flags(1, 0, 0x488), ApSecurity::Sae);
        // Transition mode: WPA2 and WPA3 both offered. WPA2 works everywhere, same password.
        assert_eq!(ApSecurity::from_flags(1, 0, 0x588), ApSecurity::WpaPsk);
        assert_eq!(ApSecurity::from_flags(1, 0, 0x288), ApSecurity::Enterprise);
        assert_eq!(
            ApSecurity::from_flags(1, 0, 0x888),
            ApSecurity::EnhancedOpen
        );
        assert!(!ApSecurity::EnhancedOpen.needs_password());
        assert!(!ApSecurity::Enterprise.needs_password());
    }

    fn read<T: TryFrom<OwnedValue>>(
        settings: &HashMap<&'static str, HashMap<&'static str, OwnedValue>>,
        section: &str,
        key: &str,
    ) -> T {
        match settings[section][key].try_clone().unwrap().try_into() {
            Ok(value) => value,
            Err(_) => panic!("{section}.{key} has the wrong type"),
        }
    }

    #[test]
    fn a_per_user_connection_keeps_its_key_out_of_the_system_profile() {
        let settings = new_connection(
            "HomeNet",
            Some(Security::WpaPsk),
            "secret pass",
            false,
            &Owner::User("alice".into()),
        );
        let permissions: Vec<String> = read(&settings, "connection", "permissions");
        assert_eq!(permissions, ["user:alice:"]);
        assert_eq!(
            read::<String>(&settings, "802-11-wireless-security", "key-mgmt"),
            "wpa-psk"
        );
        assert_eq!(
            read::<String>(&settings, "802-11-wireless-security", "psk"),
            "secret pass"
        );
        assert_eq!(
            read::<u32>(&settings, "802-11-wireless-security", "psk-flags"),
            1,
            "agent-owned"
        );
        assert!(!settings["802-11-wireless"].contains_key("hidden"));

        let shared = new_connection(
            "HomeNet",
            Some(Security::WpaPsk),
            "x",
            false,
            &Owner::Everyone,
        );
        assert!(!shared["connection"].contains_key("permissions"));
        assert_eq!(
            read::<u32>(&shared, "802-11-wireless-security", "psk-flags"),
            0,
            "NetworkManager keeps it"
        );
    }

    #[test]
    fn a_hidden_or_open_network_gets_what_it_needs() {
        let hidden = new_connection("Secret", None, "ignored", true, &Owner::Everyone);
        assert!(read::<bool>(&hidden, "802-11-wireless", "hidden"));
        assert!(
            !hidden.contains_key("802-11-wireless-security"),
            "open: no security at all"
        );
        assert_eq!(
            read::<Vec<u8>>(&hidden, "802-11-wireless", "ssid"),
            b"Secret"
        );

        let wep = new_connection("Old", Some(Security::Wep), "abcde", false, &Owner::Everyone);
        assert_eq!(
            read::<String>(&wep, "802-11-wireless-security", "key-mgmt"),
            "none"
        );
        assert!(wep["802-11-wireless-security"].contains_key("wep-key0"));
    }

    #[test]
    fn only_wifi_connections_are_saved_networks() {
        let section = |pairs: Vec<(&str, OwnedValue)>| {
            pairs
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect::<HashMap<_, _>>()
        };
        let mut wifi = HashMap::new();
        wifi.insert(
            "connection".to_owned(),
            section(vec![("type", owned("802-11-wireless"))]),
        );
        wifi.insert(
            "802-11-wireless".to_owned(),
            section(vec![("ssid", owned(b"HomeNet".to_vec()))]),
        );
        assert_eq!(saved_wifi(&wifi).as_deref(), Some("HomeNet"));

        let mut wired = HashMap::new();
        wired.insert(
            "connection".to_owned(),
            section(vec![("type", owned("802-3-ethernet"))]),
        );
        assert_eq!(saved_wifi(&wired), None);

        // A hotspot's profile has the network's name and is not a way to join it.
        let mut hotspot = wifi.clone();
        hotspot.insert(
            "802-11-wireless".to_owned(),
            section(vec![
                ("ssid", owned(b"HomeNet".to_vec())),
                ("mode", owned("ap")),
            ]),
        );
        assert_eq!(saved_wifi(&hotspot), None);
    }

    #[test]
    fn the_connected_radio_is_the_one_described() {
        let device = |interface: &str, state| WifiDevice {
            path: String::new(),
            interface: interface.into(),
            state,
            mode: Mode::Client,
            access_point: None,
            ipv4: None,
            networks: Vec::new(),
            saved: Vec::new(),
        };
        let snapshot = Snapshot {
            wifi: vec![
                device("wlan0", DeviceState::Disconnected),
                device("wlan1", DeviceState::Connected),
            ],
            ..Snapshot::default()
        };
        assert_eq!(snapshot.primary().unwrap().interface, "wlan1");
        assert!(Snapshot::default().primary().is_none());

        // A connected hotspot loses to a disconnected client: the client is what can be joined to
        // something.
        let mut hotspot = device("wlan1", DeviceState::Connected);
        hotspot.mode = Mode::Hotspot;
        let sharing = Snapshot {
            wifi: vec![hotspot, device("wlan0", DeviceState::Disconnected)],
            ..Snapshot::default()
        };
        assert_eq!(sharing.primary().unwrap().interface, "wlan0");
    }
}
