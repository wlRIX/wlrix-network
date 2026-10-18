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
//! Only what the indicator shows. Step 1 is read-only: no scan list, no saved connections, and
//! nothing written. Those come with the menu.

use std::collections::HashMap;

use zbus::MatchRule;
use zbus::message::Type;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

/// NetworkManager's bus name and root object.
pub const NAME: &str = "org.freedesktop.NetworkManager";
const ROOT: &str = "/org/freedesktop/NetworkManager";
const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const ACCESS_POINT: &str = "org.freedesktop.NetworkManager.AccessPoint";
const IP4_CONFIG: &str = "org.freedesktop.NetworkManager.IP4Config";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const DBUS: &str = "org.freedesktop.DBus";

/// `NM_DEVICE_TYPE_WIFI`.
const DEVICE_TYPE_WIFI: u32 = 2;

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

/// One Wi-Fi device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiDevice {
    pub interface: String,
    pub state: DeviceState,
    pub access_point: Option<AccessPoint>,
    /// The first IPv4 address, as `address/prefix`.
    pub ipv4: Option<String>,
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
    pub fn primary(&self) -> Option<&WifiDevice> {
        let rank = |device: &&WifiDevice| match device.state {
            DeviceState::Connected => 0,
            DeviceState::Connecting => 1,
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
        wifi.push(WifiDevice {
            interface: take(&device, "Interface").unwrap_or_default(),
            state,
            access_point,
            ipv4,
        });
    }

    Ok(Snapshot {
        wireless_enabled: take(&manager, "WirelessEnabled").unwrap_or(false),
        hardware_enabled: take(&manager, "WirelessHardwareEnabled").unwrap_or(true),
        connectivity: Connectivity::from_code(take(&manager, "Connectivity").unwrap_or(0)),
        wifi,
    })
}

/// An access point's properties, read into what is shown.
fn access_point(properties: &HashMap<String, OwnedValue>) -> AccessPoint {
    let ssid: Vec<u8> = take(properties, "Ssid").unwrap_or_default();
    let flag = |name: &str| take::<u32>(properties, name).unwrap_or(0);
    AccessPoint {
        ssid: ssid_text(&ssid),
        strength: take::<u8>(properties, "Strength").unwrap_or(0).min(100),
        // Privacy (WEP) in `Flags`, or any WPA/RSN key management at all.
        secured: flag("Flags") & 0x1 != 0 || flag("WpaFlags") != 0 || flag("RsnFlags") != 0,
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

/// Ask the bus for every signal that can change what the indicator shows: property changes on
/// NetworkManager's objects, its own signals (`DeviceAdded`, `DeviceRemoved`, `StateChanged`),
/// and its name coming and going, which is what a restart of NetworkManager looks like.
pub fn subscribe(connection: &zbus::blocking::Connection) -> zbus::Result<()> {
    let bus = zbus::blocking::fdo::DBusProxy::new(connection)?;
    bus.add_match_rule(
        MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(NAME)?
            .interface(PROPERTIES)?
            .member("PropertiesChanged")?
            .path_namespace(ROOT)?
            .build(),
    )?;
    bus.add_match_rule(
        MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(NAME)?
            .interface(NAME)?
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
fn owned<'a>(value: impl Into<zbus::zvariant::Value<'a>>) -> OwnedValue {
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

    #[test]
    fn the_connected_radio_is_the_one_described() {
        let device = |interface: &str, state| WifiDevice {
            interface: interface.into(),
            state,
            access_point: None,
            ipv4: None,
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
    }
}
