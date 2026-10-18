# wlrix-network

Network status for the wlRIX desktop: NetworkManager's Wi-Fi state, as an icon in `wlrix-tray`. Bars of signal while
connected, a tooltip naming the network and its address, a menu of nearby networks to join -- secured ones included,
with a password dialog -- and nothing at all on a machine with no wireless device.

- **Language:** Rust
- **License:** GPL-3.0-or-later

IRIX was a workstation operating system and never had to say which network it was on. This is one of the pieces that
makes wlRIX usable on a laptop. Steps one to three of `_docs/wlrix-network-roadmap.md` in the workspace are done: the
icon, the menu, and passwords. The Network Settings panel is the next step.

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
  way to join. **Choosing an open network** creates a connection for it. **A secured one not yet saved** asks for its
  password first and creates the connection only once there is one, so a Cancel leaves nothing behind. Enterprise
  (802.1X) networks post a notification instead: they take a username and certificates, which belong in Network
  Settings.
- **Join Other Network...** asks for a hidden network's name, security and password, and joins it.
- **Wireless Enabled** is the software radio switch, grayed out when the hardware switch is off. **Disconnect** takes
  the device off its network; on a machine sharing a hotspot it reads **Stop Hotspot**.
- A refusal -- no permission, a profile NetworkManager will not use -- is a notification, since nothing else would ever
  show it. Everything after a successful start arrives as NetworkManager signals and moves the icon and the menu.

Row ids stay with a network for as long as the daemon runs, so a click that arrives after a scan has reordered the list
still joins the network that was clicked. Network names have their `_` doubled on the wire, since dbusmenu reads a
single one as a mnemonic.

The menu is `com.canonical.dbusmenu` at `/MenuBar`, and all of it is served -- `GetGroupProperties`, `EventGroup` and the
rest -- so it works from another desktop's tray as well.

## Passwords

**The dialog is `wlrix-network-prompt`**, an Avalonia application in `wlrix-apps` like the pinentry: one line of JSON
in, one line out, and Cancel is an exit with nothing written. It checks a key's length before anything is tried (WPA
takes 8 to 63 characters or 64 hex digits, WEP 5 or 13 characters or 10 or 26 hex digits), says so when the last key
was refused, and its Join Other Network mode asks for the name and security too. `WLRIX_NETWORK_PROMPT` names another
program to run instead, for testing a build that is not installed.

**This daemon is NetworkManager's secret agent.** It registers with NetworkManager's agent manager -- again whenever
NetworkManager restarts, which forgets every agent -- and answers `GetSecrets` for this user's connections: from the
keyring when the key is kept there, otherwise by asking. A key that stopped working comes back as a `REQUEST_NEW`,
which the dialog states rather than presenting the same empty field again. Every call to the agent is checked to come
from the owner of `org.freedesktop.NetworkManager`: the object is on the system bus, and answering anyone would hand
this user's saved passwords to any local process. (The system bus's own policy already refuses such calls; this is the
second lock on the door.)

**Where a new connection's password lives** is decided when the connection is created:

| Keyring                                     | Connection                    | Password                                  |
|---------------------------------------------|-------------------------------|-------------------------------------------|
| something serves it, default collection unlocked | this user only (`user:<name>:`) | agent-owned, in the login keyring       |
| absent, or locked                           | everyone on the machine       | in NetworkManager's system profile       |

In a wlRIX session the first row is the usual one: gnome-keyring runs as a user unit and the greeter's PAM stack unlocks
the login collection at login. A per-user connection comes up only once someone is logged in, which is the trade
accepted for a laptop's Wi-Fi. Keyring items use NetworkManager's own schema (`org.freedesktop.NetworkManager.Connection`
with `connection-uuid`, `setting-name` and `setting-key`), so a password saved from GNOME is found here and the other
way round.

**A wrong password leaves nothing behind.** A connection created here that has never connected is remembered as fresh;
if NetworkManager asks for its key again and the person cancels, the connection is deleted rather than left for
autoconnect to keep failing on.

Every call this daemon makes times out after ten seconds, so a service that stops answering mid-call -- seen with a
keyring daemon handing its name to another process -- costs a log line, not a frozen menu.

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

Shared mode's DHCP may not answer between virtual radios, leaving the client at "getting IP configuration" -- which is
past authentication, so it is as far as a password test needs to go. Giving the client's profile a static
`10.42.0.x/24` address gets it to connected. Delete the test profiles afterwards.

Passwords can be tested without typing into a window: point `WLRIX_NETWORK_PROMPT` at a script that reads the request
line and prints an answer, or exits 1 for Cancel. To test keyring storage without the real keyring, give the test
session its own `XDG_RUNTIME_DIR` and `XDG_DATA_HOME` and start an unlocked one inside it --
`echo -n test | gnome-keyring-daemon --unlock --components=secrets`. Without the separate runtime directory a keyring
D-Bus starts on a private bus finds the real one through its control socket.

The menu can be driven without a pointer: `busctl --user call <item name> /MenuBar com.canonical.dbusmenu GetLayout iias
-- 0 -1 0` lists the rows, and `Event isvu <id> clicked i 0 0` chooses one. Run that on a private bus
(`env -u WAYLAND_DISPLAY dbus-run-session`) so the live session's tray and notifications are not involved.
