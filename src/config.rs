// SPDX-License-Identifier: GPL-3.0-or-later
//! The hand-edited settings file.
//!
//! ```toml
//! # ~/.config/wlrix/network.toml
//! indicator = "auto"           # auto | hidden
//! ```
//!
//! Read from the user's config directory first, then `/etc/wlrix`; the first file found wins
//! outright, and unknown keys are an error -- the shape every wlRIX component's file has.
//!
//! One key so far. The menu's network cap, sounds and notifications arrive with the parts of the
//! program that use them (see `_docs/wlrix-network-roadmap.md`).

use serde::Deserialize;

/// Whether the Wi-Fi icon is shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Indicator {
    /// Shown whenever NetworkManager reports a wireless device, hidden otherwise -- so a desktop
    /// on a cable shows nothing at all.
    #[default]
    Auto,
    /// Never shown, for a machine that has a Wi-Fi card but is only ever used on Ethernet.
    ///
    /// Hidden, not off: the daemon goes on running, because later steps make it the secret agent
    /// NetworkManager asks for Wi-Fi passwords, and that has to keep working for connections set
    /// up from Network Settings.
    Hidden,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub indicator: Indicator,
}

impl Config {
    /// Load the first config file that exists. No file at all is the defaults.
    pub fn load() -> Self {
        for path in crate::xdg::config_paths() {
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    eprintln!("wlrix-network: could not read {}: {err}", path.display());
                    continue;
                }
            };
            match toml::from_str::<Self>(&text) {
                Ok(config) => return config,
                Err(err) => {
                    eprintln!("wlrix-network: {} is not valid: {err}", path.display());
                    return Self::default();
                }
            }
        }
        Self::default()
    }
}

/// Parse a candidate config file, for `--check-config`. `wlrix-settings-daemon` runs this before
/// renaming a file into place, so a settings panel cannot write one this program would refuse.
pub fn check(path: &std::path::Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("could not read {}: {err}", path.display()))?;
    toml::from_str::<Config>(&text)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_shows_the_indicator_when_there_is_wifi() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.indicator, Indicator::Auto);
    }

    #[test]
    fn hidden_is_spelled_as_the_settings_daemon_writes_it() {
        let config: Config = toml::from_str("indicator = \"hidden\"\n").unwrap();
        assert_eq!(config.indicator, Indicator::Hidden);
    }

    #[test]
    fn a_typo_is_refused_rather_than_ignored() {
        assert!(toml::from_str::<Config>("indicator = \"off\"\n").is_err());
        assert!(toml::from_str::<Config>("indictor = \"hidden\"\n").is_err());
    }
}
