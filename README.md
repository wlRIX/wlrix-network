# wlrix-network

Network status for the wlRIX desktop: NetworkManager's Wi-Fi state, as an icon in `wlrix-tray`. Bars of signal while
connected, a tooltip naming the network and its address, a menu of nearby networks to join, and nothing at all on a
machine with no wireless device.

- **Language:** Rust
- **License:** GPL-3.0-or-later

IRIX was a workstation operating system and never had to say which network it was on. This is one of the pieces that
makes wlRIX usable on a laptop. Steps one and two of `_docs/wlrix-network-roadmap.md` in the workspace are done: the
icon, and the menu. The password prompt and the Network Settings panel are later steps.

## What it is

A daemon with no window. It reads NetworkManager on the system bus and publishes a **StatusNotifierItem** on the
session bus, which the tray draws -- the same way fcitx5's input-method indicator gets there. It needs no compositor and
no change to the tray.

| State                                | Icon                                         | Tooltip                                 |
|--------------------------------------|----------------------------------------------|-----------------------------------------|
| connected                            | `network-wireless-signal-{none…excellent}`   | the network, signal, security, address  |
| connected, no internet / captive portal | `network-wireless-no-route`               | …and "No internet access" / "Sign-in required" |
| connecting                           | `network-wireless-acquiring`                 | "Connecting to *network*"               |
| not connected, failed, unavailable   | `network-wireless-offline`                   | what happened                           |
| radio off                            | `network-wireless-disabled`                  | "Wireless is off"                       |
| off at the hardware switch           | `network-wireless-hardware-disabled`         | "Turned off by the hardware switch"     |

All `-symbolic`, looked up by the tray in the `wlrix` icon theme, which inherits Adwaita. Signal strength is cut into
GNOME's buckets, so a network shows the same bars here as anywhere else on the machine.

**The item is `Passive`, which the tray hides, when there is nothing to describe:** no Wi-Fi device, or NetworkManager
not running. There is no wired indicator; a desktop on a cable shows nothing. A USB adapter plugged in later appears
when NetworkManager reports it.

NetworkManager only. iwd works as NetworkManager's Wi-Fi backend, which hides it; standalone iwd and connman are not
supported.

## Configuration

`~/.config/wlrix/network.toml`, then `/etc/wlrix/network.toml`. The first file found wins outright, unknown keys are
refused, and `wlrix-network --check-config <path>` says whether a file would be accepted.

```toml
indicator = "auto"           # auto | hidden
```

`hidden` keeps the icon out of the tray even with a Wi-Fi card, for a machine that has one but is only used on a
cable. The daemon keeps running: later steps make it the agent NetworkManager asks for Wi-Fi passwords, and that has
to work whether the icon is shown or not.

`SIGHUP` re-reads the file and `SIGTERM` stops the daemon; the pidfile is `$XDG_RUNTIME_DIR/wlrix-network.pid`.

## Using it

`wlrix-session` starts it. `wlrix-network --status` reads NetworkManager once and prints what the icon would show, which
is the quickest way to see what it sees:

```console
$ wlrix-network --status
status:  Active
icon:    network-wireless-signal-good-symbolic
title:   HomeNet
         Signal 72%, secured
         192.168.1.20/24
radio:   on (hardware switch on)
device:  wlan0 Connected
```

The item's `Id` is `wlrix-network`, for ordering or hiding it with `[[item]]` in `tray.toml`.

## The menu

A click on the icon opens it, drawn by the tray as a 4Dwm menu:

```text
Wi-Fi: Connected to HomeNet
[x] Wireless Enabled
-------------------------
(*) HomeNet       ▂▄▆
( ) Neighbor-5G   ▂▄▆█
( ) CoffeeShop    ▂   (open)
More Networks            >
-------------------------
Disconnect
```

- **Opening it scans**, at most every ten seconds, and networks the scan finds appear in the open menu.
- **The network in use comes first**, then the ones NetworkManager has saved, then the rest, each strongest first.
  One row per network name, however many access points broadcast it. Eight at the top; the rest under More Networks.
- **Choosing a saved network** joins it through the device's own `AvailableConnections` -- NetworkManager's list of
  profiles that work on that device, so a hotspot's profile or one tied to another interface is never mistaken for a
  way to join. **Choosing an open network** creates a connection for it. **A secured one not yet saved** posts a
  notification instead, with the `nmcli` command to join it once: the password prompt is the next step, and without
  it NetworkManager would only fail asking for a key. Enterprise networks likewise.
- **Wireless Enabled** is the software radio switch, grayed out when the hardware switch is off. **Disconnect** takes
  the device off its network; on a machine sharing a hotspot it reads **Stop Hotspot**.
- A refusal -- no permission, a profile NetworkManager will not use -- is a notification, since nothing else would ever
  show it. Everything after a successful start arrives as NetworkManager signals and moves the icon and the menu.

Row ids stay with a network for as long as the daemon runs, so a click that arrives after a scan has reordered the list
still joins the network that was clicked. Network names have their `_` doubled on the wire, since dbusmenu reads a
single one as a mnemonic.

The menu is `com.canonical.dbusmenu` at `/MenuBar`, and all of it is served -- `GetGroupProperties`, `EventGroup` and the
rest -- so it works from another desktop's tray as well.

## How it stays current

**Every NetworkManager signal means "read it all again".** NetworkManager announces changes with `PropertiesChanged`
on whichever object changed, and its objects point at each other by path; following each signal to the part of the
picture it touches would mean keeping a copy of NetworkManager's object graph and patching it. Instead any signal arms a
quarter-second timer, and when it fires the state is read again -- the manager, its Wi-Fi devices, each one's access
point and address, a dozen calls on a local bus. A burst of signals (one connection attempt is half a dozen state
changes; a background scan touches every access point in range) costs one read.

The item is updated only when what it shows changed, and then emits `NewIcon`, `NewToolTip` and, if it changed,
`NewStatus`.

**The tray restarting** loses every registration. The daemon watches `org.kde.StatusNotifierWatcher` change owner and
registers again, so the icon comes back with the tray.

## Testing it

The mapping from NetworkManager's state to what is shown is unit-tested case by case, as is reading NetworkManager's
property maps -- device states, access-point security flags, SSIDs that are not UTF-8, address data.

```console
$ cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --all --check
```

A machine with no Wi-Fi can still test everything with virtual radios. `mac80211_hwsim` creates them, and
NetworkManager can make one of them an access point for the other:

```console
$ sudo modprobe mac80211_hwsim radios=2
$ nmcli connection add type wifi ifname wlan1 con-name test-open autoconnect no ssid test_open \
      802-11-wireless.mode ap ipv4.method shared ipv6.method disabled
$ nmcli connection up test-open
$ nmcli device wifi hotspot ifname wlan1 con-name test-wpa ssid test_wpa password testpass123
$ wlrix-network --status
```

Shared mode's DHCP may not answer between virtual radios, leaving the client at "getting IP configuration"; giving the
client's profile a static `10.42.0.x/24` address gets it to connected. Delete the test profiles afterwards.

The menu can be driven without a pointer: `busctl --user call <item name> /MenuBar com.canonical.dbusmenu GetLayout iias
-- 0 -1 0` lists the rows, and `Event isvu <id> clicked i 0 0` chooses one. Run that on a private bus
(`env -u WAYLAND_DISPLAY dbus-run-session`) so the live session's tray and notifications are not involved.
