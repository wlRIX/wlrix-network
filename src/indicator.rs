// SPDX-License-Identifier: GPL-3.0-or-later
//! What the tray is told to draw, worked out from what NetworkManager says.
//!
//! No bus here: a [`Snapshot`] goes in and a [`View`] -- status, icon name, tooltip -- comes out,
//! so every case the icon can be in is a unit test.
//!
//! # The icons
//!
//! The freedesktop names GNOME's and KDE's indicators use, `network-wireless-*-symbolic`. The
//! tray looks them up in the `wlrix` icon theme, which inherits Adwaita, so all of them resolve
//! today; IRIX-styled art can replace them in `wlrix-assets` without anything here changing.
//!
//! Signal strength is cut into GNOME's buckets, so a network reads as the same number of bars
//! here as it would anywhere else on the same machine.
//!
//! # Hidden, and when
//!
//! `Passive` is how an item asks a tray not to show it, and the tray's default hides passive
//! items. The item is passive when there is no Wi-Fi to describe -- no wireless device, or
//! NetworkManager not running -- and when the person said `indicator = "hidden"`. There is no
//! wired indicator: a machine on a cable shows nothing, which is what a desktop wants.

use crate::config::Indicator;
use crate::i18n::{Language, tr};
use crate::nm::{Connectivity, DeviceState, Mode, Snapshot};

/// `Status`, as the StatusNotifierItem specification spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Passive,
    Active,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Passive => "Passive",
            Status::Active => "Active",
        }
    }
}

/// Everything the item publishes that can change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub status: Status,
    pub icon: &'static str,
    /// The tooltip's heading: the network's name when there is one, else what the radio is doing.
    pub title: String,
    /// The rest of the tooltip, one fact per line.
    pub body: String,
}

/// The icon for a radio that is off.
const DISABLED: &str = "network-wireless-disabled-symbolic";
/// Off at the hardware switch, which the software switch cannot undo.
const HARDWARE_DISABLED: &str = "network-wireless-hardware-disabled-symbolic";
const OFFLINE: &str = "network-wireless-offline-symbolic";
const ACQUIRING: &str = "network-wireless-acquiring-symbolic";
/// Connected, but NetworkManager's check says the internet is not reachable.
const NO_ROUTE: &str = "network-wireless-no-route-symbolic";
/// Sharing this computer's connection as a network of its own.
const HOTSPOT: &str = "network-wireless-hotspot-symbolic";

/// The bars for a signal strength, in GNOME's buckets.
fn signal(strength: u8) -> &'static str {
    match strength {
        0..20 => "network-wireless-signal-none-symbolic",
        20..40 => "network-wireless-signal-weak-symbolic",
        40..50 => "network-wireless-signal-ok-symbolic",
        50..80 => "network-wireless-signal-good-symbolic",
        _ => "network-wireless-signal-excellent-symbolic",
    }
}

/// Work out what to show. `None` for the snapshot is NetworkManager not answering.
pub fn view(snapshot: Option<&Snapshot>, setting: Indicator) -> View {
    let hidden = |title: &str| View {
        status: Status::Passive,
        icon: OFFLINE,
        title: title.to_owned(),
        body: String::new(),
    };
    let Some(snapshot) = snapshot else {
        return hidden(tr(
            "NetworkManager is not running",
            "NetworkManager が実行されていません",
        ));
    };
    let Some(device) = snapshot.primary() else {
        return hidden(tr("No wireless device", "ワイヤレスデバイスがありません"));
    };
    let status = match setting {
        Indicator::Auto => Status::Active,
        Indicator::Hidden => Status::Passive,
    };
    let wireless_off = tr("Wireless is off", "ワイヤレスはオフです");
    let view = |icon, title: String, body: Vec<String>| View {
        status,
        icon,
        title,
        body: body.join("\n"),
    };

    // The switches come first: a radio that is off reports its device as unavailable, and
    // "unavailable" would be the wrong thing to say about a radio someone turned off.
    if !snapshot.hardware_enabled {
        return view(
            HARDWARE_DISABLED,
            wireless_off.into(),
            vec![
                tr(
                    "Turned off by the hardware switch",
                    "ハードウェアスイッチでオフになっています",
                )
                .into(),
            ],
        );
    }
    if !snapshot.wireless_enabled {
        return view(DISABLED, wireless_off.into(), Vec::new());
    }

    let ssid = device.access_point.as_ref().map(|ap| ap.ssid.clone());
    if device.mode == Mode::Hotspot && device.state == DeviceState::Connected {
        return view(
            HOTSPOT,
            match ssid {
                Some(ssid) => format!("{}: {ssid}", tr("Hotspot", "ホットスポット")),
                None => tr("Hotspot", "ホットスポット").into(),
            },
            vec![
                tr(
                    "Sharing this computer's connection",
                    "このコンピュータの接続を共有しています",
                )
                .into(),
            ],
        );
    }
    match device.state {
        DeviceState::Connected => {
            let Some(ap) = device.access_point.as_ref() else {
                // Connected with no access point is a moment in the middle of roaming.
                return view(
                    ACQUIRING,
                    "Wi-Fi".into(),
                    vec![tr("Connected", "接続済み").into()],
                );
            };
            let mut body = vec![Language::current().pick(
                format!(
                    "Signal {}%{}",
                    ap.strength,
                    if ap.secured { ", secured" } else { ", open" }
                ),
                format!(
                    "信号 {}%、{}",
                    ap.strength,
                    if ap.secured {
                        "保護あり"
                    } else {
                        "保護なし"
                    }
                ),
            )];
            if let Some(address) = &device.ipv4 {
                body.push(address.clone());
            }
            let icon = match snapshot.connectivity {
                Connectivity::None | Connectivity::Limited => {
                    body.push(tr("No internet access", "インターネットに接続できません").into());
                    NO_ROUTE
                }
                Connectivity::Portal => {
                    body.push(tr("Sign-in required", "サインインが必要です").into());
                    NO_ROUTE
                }
                Connectivity::Full | Connectivity::Unknown => signal(ap.strength),
            };
            view(icon, ap.ssid.clone(), body)
        }
        DeviceState::Connecting => view(
            ACQUIRING,
            match ssid {
                Some(ssid) => Language::current()
                    .pick(format!("Connecting to {ssid}"), format!("{ssid} に接続中")),
                None => tr("Connecting", "接続中").into(),
            },
            Vec::new(),
        ),
        DeviceState::Disconnecting => {
            view(OFFLINE, tr("Disconnecting", "切断中").into(), Vec::new())
        }
        DeviceState::Failed => view(
            OFFLINE,
            tr("Connection failed", "接続に失敗しました").into(),
            Vec::new(),
        ),
        DeviceState::Disconnected => {
            view(OFFLINE, tr("Not connected", "未接続").into(), Vec::new())
        }
        DeviceState::Unavailable => view(
            OFFLINE,
            tr("Wireless unavailable", "ワイヤレスを利用できません").into(),
            Vec::new(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nm::{AccessPoint, WifiDevice};

    fn snapshot(state: DeviceState, strength: u8) -> Snapshot {
        Snapshot {
            wireless_enabled: true,
            hardware_enabled: true,
            connectivity: Connectivity::Full,
            wifi: vec![WifiDevice {
                path: "/dev/wlan0".into(),
                saved: Vec::new(),
                networks: Vec::new(),
                interface: "wlan0".into(),
                state,
                mode: Mode::Client,
                access_point: Some(AccessPoint {
                    ssid: "HomeNet".into(),
                    strength,
                    secured: true,
                }),
                ipv4: Some("192.168.1.20/24".into()),
            }],
        }
    }

    #[test]
    fn no_wifi_or_no_networkmanager_is_hidden() {
        assert_eq!(view(None, Indicator::Auto).status, Status::Passive);
        let wired = Snapshot {
            wireless_enabled: true,
            hardware_enabled: true,
            ..Snapshot::default()
        };
        assert_eq!(view(Some(&wired), Indicator::Auto).status, Status::Passive);
    }

    #[test]
    fn hidden_by_setting_still_works_out_the_rest() {
        let connected = snapshot(DeviceState::Connected, 90);
        let shown = view(Some(&connected), Indicator::Hidden);
        assert_eq!(shown.status, Status::Passive);
        // The icon and tooltip are still right, so a reload to `auto` shows something true at
        // once.
        assert_eq!(shown.title, "HomeNet");
    }

    #[test]
    fn connected_shows_bars_in_gnomes_buckets() {
        for (strength, icon) in [
            (10, "network-wireless-signal-none-symbolic"),
            (25, "network-wireless-signal-weak-symbolic"),
            (45, "network-wireless-signal-ok-symbolic"),
            (60, "network-wireless-signal-good-symbolic"),
            (95, "network-wireless-signal-excellent-symbolic"),
        ] {
            let shown = view(
                Some(&snapshot(DeviceState::Connected, strength)),
                Indicator::Auto,
            );
            assert_eq!(shown.icon, icon, "{strength}%");
            assert_eq!(shown.status, Status::Active);
        }
    }

    #[test]
    fn the_tooltip_names_the_network_and_its_address() {
        let shown = view(Some(&snapshot(DeviceState::Connected, 72)), Indicator::Auto);
        assert_eq!(shown.title, "HomeNet");
        assert_eq!(shown.body, "Signal 72%, secured\n192.168.1.20/24");
    }

    #[test]
    fn no_internet_says_so_in_the_icon_and_the_tooltip() {
        let mut limited = snapshot(DeviceState::Connected, 72);
        limited.connectivity = Connectivity::Limited;
        let shown = view(Some(&limited), Indicator::Auto);
        assert_eq!(shown.icon, NO_ROUTE);
        assert!(shown.body.ends_with("No internet access"));

        limited.connectivity = Connectivity::Portal;
        assert!(
            view(Some(&limited), Indicator::Auto)
                .body
                .ends_with("Sign-in required")
        );
    }

    #[test]
    fn connecting_names_where_it_is_going() {
        let shown = view(Some(&snapshot(DeviceState::Connecting, 0)), Indicator::Auto);
        assert_eq!(shown.icon, ACQUIRING);
        assert_eq!(shown.title, "Connecting to HomeNet");
    }

    #[test]
    fn a_switched_off_radio_says_which_switch() {
        let mut off = snapshot(DeviceState::Unavailable, 0);
        off.wireless_enabled = false;
        let shown = view(Some(&off), Indicator::Auto);
        assert_eq!(shown.icon, DISABLED);
        assert_eq!(shown.title, "Wireless is off");

        off.hardware_enabled = false;
        assert_eq!(view(Some(&off), Indicator::Auto).icon, HARDWARE_DISABLED);
    }

    #[test]
    fn a_hotspot_on_its_own_says_it_is_one() {
        let mut sharing = snapshot(DeviceState::Connected, 0);
        sharing.wifi[0].mode = Mode::Hotspot;
        let shown = view(Some(&sharing), Indicator::Auto);
        assert_eq!(shown.icon, HOTSPOT);
        assert_eq!(shown.title, "Hotspot: HomeNet");
    }

    #[test]
    fn disconnected_is_offline_not_hidden() {
        let shown = view(
            Some(&snapshot(DeviceState::Disconnected, 0)),
            Indicator::Auto,
        );
        assert_eq!(shown.icon, OFFLINE);
        assert_eq!(shown.status, Status::Active);
        assert_eq!(shown.title, "Not connected");
    }
}
