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
/// `NM_802_11_AP_SEC_KEY_MGMT_802_1X`: enterprise Wi-Fi, a username and a certificate rather than
/// a password.
const KEY_MGMT_802_1X: u32 = 0x200;

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
    pub secured: bool,
    /// 802.1X. Set up in Network Settings rather than from a menu.
    pub enterprise: bool,
    /// The strongest access point's object, which is what NetworkManager is asked to join.
    pub path: String,
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
            secured: point.secured,
            enterprise: (flag("WpaFlags") | flag("RsnFlags")) & KEY_MGMT_802_1X != 0,
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

/// Join a network NetworkManager has no connection for, creating one.
///
/// The settings are left empty and the access point is named: NetworkManager fills in the rest
/// from what the access point advertises, which for an open network is everything. Where the new
/// connection is stored, and who may use it, is decided with the password prompt in the next
/// step -- until then NetworkManager's default, system-wide, applies.
pub fn add_and_activate(
    connection: &zbus::blocking::Connection,
    device: &str,
    access_point: &str,
) -> zbus::Result<()> {
    let object = |path: &str| ObjectPath::try_from(path.to_owned()).map_err(zbus::Error::from);
    let settings: HashMap<&str, HashMap<&str, Value>> = HashMap::new();
    connection.call_method(
        Some(NAME),
        ROOT,
        Some(NAME),
        "AddAndActivateConnection",
        &(settings, object(device)?, object(access_point)?),
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
            scanned(b"Campus", 30, 0x200 | 0x100, "/ap/5"),
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
        assert!(list[0].secured && !list[0].enterprise);
        assert!(!list[1].secured);
        assert!(list[2].enterprise);
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
