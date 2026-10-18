// SPDX-License-Identifier: GPL-3.0-or-later
//! Reloading and stopping when asked to.
//!
//! `SIGHUP` means "re-read the config" and `SIGTERM`/`SIGINT` mean "stop". Both are turned into
//! a calloop [`Ping`] fired from the handler -- an eventfd write, which is async-signal-safe --
//! whose source on the event loop does the actual work. The same shape `wlrix-idle`,
//! `wlrix-tray`, `wlrix-sound` and `wlrix-compositor` use, kept in step by hand.
//!
//! Stopping goes through the loop so the pidfile is removed on the way out. This program is not a
//! Wayland client, so unlike the tray it does not end when the compositor does: `wlrix-session`
//! stopping it with `SIGTERM` at logout is the ordinary end of its life.

use std::sync::OnceLock;

use calloop::ping::Ping;

/// The ping the quit handler fires. Set once, before the handlers are installed.
static QUIT: OnceLock<Ping> = OnceLock::new();
/// The ping the `SIGHUP` handler fires, to reload the config on the event loop.
static RELOAD: OnceLock<Ping> = OnceLock::new();

/// Install `SIGTERM`/`SIGINT` handlers that fire `quit`, so the loop can stop itself.
pub fn forward_to_loop(quit: Ping) {
    if QUIT.set(quit).is_err() {
        return;
    }
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: the handler does only async-signal-safe work -- firing the ping, which is an
        // eventfd write.
        unsafe { libc::signal(signal, handle as *const () as libc::sighandler_t) };
    }
}

/// Install a `SIGHUP` handler that fires `reload`, so the loop can re-read the config.
pub fn forward_reload_to_loop(reload: Ping) {
    if RELOAD.set(reload).is_err() {
        return;
    }
    // SAFETY: async-signal-safe work only -- firing the ping (an eventfd write).
    unsafe {
        libc::signal(
            libc::SIGHUP,
            handle_reload as *const () as libc::sighandler_t,
        )
    };
}

/// Runs in signal context; may only do async-signal-safe work.
extern "C" fn handle(_signal: libc::c_int) {
    if let Some(quit) = QUIT.get() {
        quit.ping();
    }
}

/// Runs in signal context; may only do async-signal-safe work.
extern "C" fn handle_reload(_signal: libc::c_int) {
    if let Some(reload) = RELOAD.get() {
        reload.ping();
    }
}
