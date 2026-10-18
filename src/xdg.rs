// SPDX-License-Identifier: GPL-3.0-or-later
//! Where the daemon's settings file lives.
//!
//! The convention is the compositor's, unchanged (see its `config.rs`): a hand-edited file
//! under `$XDG_CONFIG_HOME/wlrix/`, with `/etc/wlrix/` behind it for a system default. The
//! daemon writes nothing back, so unlike `wlrix-desktop` there is no state file and no
//! `$XDG_STATE_HOME` half of this module.

use std::path::{Path, PathBuf};

/// The hand-edited settings file, relative to a config directory.
pub const CONFIG_NAME: &str = "wlrix/network.toml";
/// Consulted when the user has no config of their own, as the compositor does.
const SYSTEM_CONFIG_DIR: &str = "/etc";

/// `$HOME`, or `None` when even that is unset.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// `$XDG_CONFIG_HOME`, or `~/.config` as the spec says to assume.
pub fn user_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    home().map(|home| home.join(".config"))
}

/// Where to look for the settings file, most specific first.
pub fn config_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = user_config_dir() {
        dirs.push(dir.join(CONFIG_NAME));
    }
    dirs.push(Path::new(SYSTEM_CONFIG_DIR).join(CONFIG_NAME));
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_file_is_always_a_candidate() {
        // Even with no HOME at all -- a session started by something that scrubbed the
        // environment still gets the administrator's defaults rather than nothing.
        let paths = config_paths();
        assert!(paths.last().unwrap().ends_with("wlrix/network.toml"));
        assert!(paths.last().unwrap().starts_with("/etc"));
    }
}
