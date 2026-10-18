// SPDX-License-Identifier: GPL-3.0-or-later
//! NetworkManager's secret agent: who it asks when a connection needs a password.
//!
//! When NetworkManager needs a key -- a first connection to a network saved without one, a key
//! that stopped working, a per-user connection whose key lives in the keyring -- it asks the
//! agents registered by the users allowed to use that connection. With none registered the
//! connection fails with "no secrets", which is why this is not optional.
//!
//! This side only receives. Each call becomes an [`AgentEvent`] for the main loop, which looks in
//! the keyring, puts up the prompt, and answers through the one-shot channel the event carries.
//! `GetSecrets` is `async` so it can wait that long -- a person may take a minute to find a
//! password -- without holding the system bus's dispatch thread.
//!
//! # Only NetworkManager may ask
//!
//! The agent object is on the system bus, where any process on the machine, run by any user, can
//! send it a method call. Answered unchecked, a `GetSecrets` naming one of this user's connections
//! would read its password out of the keyring for whoever asked, and one naming a made-up network
//! would put a convincing password dialog on this user's screen. So every call is checked against
//! the current owner of `org.freedesktop.NetworkManager` and refused otherwise.

use std::collections::HashMap;

use zbus::message::Header;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::prompt::Security;

/// Where the agent is served. Fixed by NetworkManager, which calls exactly this path.
pub const AGENT_PATH: &str = "/org/freedesktop/NetworkManager/SecretAgent";

/// `a{sa{sv}}`: a connection's settings, section by section.
pub type Settings = HashMap<String, HashMap<String, OwnedValue>>;

/// `NM_SECRET_AGENT_GET_SECRETS_FLAG_ALLOW_INTERACTION`: a person may be asked.
const ALLOW_INTERACTION: u32 = 0x1;
/// `..._REQUEST_NEW`: the stored key did not work; ask again rather than handing it back.
const REQUEST_NEW: u32 = 0x2;
/// `NM_SETTING_SECRET_FLAG_AGENT_OWNED`.
const AGENT_OWNED: u32 = 0x1;
/// `NM_SETTING_SECRET_FLAG_NOT_SAVED`: ask every time, store nowhere.
const NOT_SAVED: u32 = 0x2;

/// The errors NetworkManager distinguishes. `UserCanceled` ends the attempt; `NoSecrets` sends it
/// on to the next agent.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.NetworkManager.SecretAgent")]
pub enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    UserCanceled(String),
    NoSecrets(String),
    NotAuthorized(String),
    InternalError(String),
}

/// One `GetSecrets`, with what the loop needs already read out of the settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRequest {
    pub connection_path: String,
    /// The section the secret is in: `802-11-wireless-security` for every case handled here.
    pub setting: String,
    pub uuid: String,
    /// The connection's name, for the keyring item's label.
    pub id: String,
    pub ssid: String,
    /// The security the prompt should ask for, when it is one it can.
    pub security: Option<Security>,
    /// The key is this user's to keep, in the keyring.
    pub agent_owned: bool,
    /// The key is never stored: ask every time.
    pub not_saved: bool,
    pub allow_interaction: bool,
    pub request_new: bool,
}

impl SecretRequest {
    pub fn parse(connection: &Settings, path: &str, setting: &str, flags: u32) -> Self {
        let text = |section: &str, key: &str| -> String {
            connection
                .get(section)
                .and_then(|values| values.get(key))
                .and_then(|value| value.try_clone().ok())
                .and_then(|value| String::try_from(value).ok())
                .unwrap_or_default()
        };
        let number = |section: &str, key: &str| -> u32 {
            connection
                .get(section)
                .and_then(|values| values.get(key))
                .and_then(|value| value.try_clone().ok())
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(0)
        };
        let ssid: Vec<u8> = connection
            .get("802-11-wireless")
            .and_then(|values| values.get("ssid"))
            .and_then(|value| value.try_clone().ok())
            .and_then(|value| Vec::<u8>::try_from(value).ok())
            .unwrap_or_default();
        let security = match text("802-11-wireless-security", "key-mgmt").as_str() {
            "wpa-psk" => Some(Security::WpaPsk),
            "sae" => Some(Security::Sae),
            // Static WEP keys have no key management at all.
            "none" => Some(Security::Wep),
            // wpa-eap, ieee8021x, owe: nothing the prompt asks for.
            _ => None,
        };
        let secret_flags = security
            .and_then(Security::flags_key)
            .map(|key| number("802-11-wireless-security", key))
            .unwrap_or(0);
        Self {
            connection_path: path.to_owned(),
            setting: setting.to_owned(),
            uuid: text("connection", "uuid"),
            id: text("connection", "id"),
            ssid: String::from_utf8_lossy(&ssid).into_owned(),
            security,
            agent_owned: secret_flags & AGENT_OWNED != 0,
            not_saved: secret_flags & NOT_SAVED != 0,
            allow_interaction: flags & ALLOW_INTERACTION != 0,
            request_new: flags & REQUEST_NEW != 0,
        }
    }
}

/// The agent-owned secrets in a connection's settings that should go in the keyring, as
/// `(setting, key, value)`. Secrets flagged "not saved", or owned by NetworkManager, are left out.
pub fn agent_owned_secrets(connection: &Settings) -> Vec<(String, String, String)> {
    let Some(section) = connection.get("802-11-wireless-security") else {
        return Vec::new();
    };
    let get = |key: &str| section.get(key).and_then(|value| value.try_clone().ok());
    let mut found = Vec::new();
    for (key, flags_key) in [
        ("psk", "psk-flags"),
        ("wep-key0", "wep-key-flags"),
        ("wep-key1", "wep-key-flags"),
        ("wep-key2", "wep-key-flags"),
        ("wep-key3", "wep-key-flags"),
        ("leap-password", "leap-password-flags"),
    ] {
        let flags = get(flags_key)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0);
        if flags & AGENT_OWNED == 0 || flags & NOT_SAVED != 0 {
            continue;
        }
        if let Some(value) = get(key).and_then(|value| String::try_from(value).ok())
            && !value.is_empty()
        {
            found.push(("802-11-wireless-security".to_owned(), key.to_owned(), value));
        }
    }
    found
}

/// What the agent tells the main loop.
#[derive(Debug)]
pub enum AgentEvent {
    GetSecrets(
        SecretRequest,
        async_channel::Sender<Result<Settings, AgentError>>,
    ),
    Cancel {
        connection_path: String,
        setting: String,
    },
    Save(Settings),
    Delete(Settings),
}

/// The interface object.
pub struct Agent {
    pub events: calloop::channel::Sender<AgentEvent>,
}

/// Refuse a caller that is not NetworkManager. See the module comment.
async fn from_networkmanager(
    connection: &zbus::Connection,
    header: &Header<'_>,
) -> Result<(), AgentError> {
    let sender = header
        .sender()
        .ok_or_else(|| AgentError::NotAuthorized("a call with no sender".into()))?;
    let bus = zbus::fdo::DBusProxy::new(connection).await?;
    let owner = bus
        .get_name_owner(crate::nm::NAME.try_into().map_err(zbus::Error::from)?)
        .await
        .map_err(|err| {
            AgentError::NotAuthorized(format!("NetworkManager is not running: {err}"))
        })?;
    if owner.as_str() != sender.as_str() {
        eprintln!(
            "wlrix-network: refused a secret agent call from {sender}, which is not NetworkManager"
        );
        return Err(AgentError::NotAuthorized(
            "only NetworkManager may ask".into(),
        ));
    }
    Ok(())
}

#[zbus::interface(name = "org.freedesktop.NetworkManager.SecretAgent")]
impl Agent {
    // The first five are the D-Bus method's own, fixed by NetworkManager; the last two are zbus's.
    #[allow(clippy::too_many_arguments)]
    async fn get_secrets(
        &self,
        connection: Settings,
        connection_path: OwnedObjectPath,
        setting_name: String,
        _hints: Vec<String>,
        flags: u32,
        #[zbus(connection)] bus: &zbus::Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<Settings, AgentError> {
        from_networkmanager(bus, &header).await?;
        let request =
            SecretRequest::parse(&connection, connection_path.as_str(), &setting_name, flags);
        let (reply, answer) = async_channel::bounded(1);
        self.events
            .send(AgentEvent::GetSecrets(request, reply))
            .map_err(|_| AgentError::InternalError("the daemon is stopping".into()))?;
        answer
            .recv()
            .await
            .unwrap_or_else(|_| Err(AgentError::InternalError("no answer".into())))
    }

    async fn cancel_get_secrets(
        &self,
        connection_path: OwnedObjectPath,
        setting_name: String,
        #[zbus(connection)] bus: &zbus::Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), AgentError> {
        from_networkmanager(bus, &header).await?;
        let _ = self.events.send(AgentEvent::Cancel {
            connection_path: connection_path.to_string(),
            setting: setting_name,
        });
        Ok(())
    }

    async fn save_secrets(
        &self,
        connection: Settings,
        _connection_path: OwnedObjectPath,
        #[zbus(connection)] bus: &zbus::Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), AgentError> {
        from_networkmanager(bus, &header).await?;
        let _ = self.events.send(AgentEvent::Save(connection));
        Ok(())
    }

    async fn delete_secrets(
        &self,
        connection: Settings,
        _connection_path: OwnedObjectPath,
        #[zbus(connection)] bus: &zbus::Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), AgentError> {
        from_networkmanager(bus, &header).await?;
        let _ = self.events.send(AgentEvent::Delete(connection));
        Ok(())
    }
}

/// The reply to a `GetSecrets`: one key in one section.
pub fn secrets(setting: &str, key: &str, value: &str) -> Settings {
    let value = OwnedValue::try_from(zbus::zvariant::Value::from(value.to_owned()))
        .expect("a string is ownable");
    HashMap::from([(setting.to_owned(), HashMap::from([(key.to_owned(), value)]))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    fn owned<'a>(value: impl Into<Value<'a>>) -> OwnedValue {
        value.into().try_into().unwrap()
    }

    fn settings(key_mgmt: &str, flags: u32, psk: Option<&str>) -> Settings {
        let mut security = HashMap::from([
            ("key-mgmt".to_owned(), owned(key_mgmt)),
            ("psk-flags".to_owned(), owned(flags)),
            ("wep-key-flags".to_owned(), owned(flags)),
        ]);
        if let Some(psk) = psk {
            security.insert("psk".to_owned(), owned(psk));
        }
        HashMap::from([
            (
                "connection".to_owned(),
                HashMap::from([
                    ("uuid".to_owned(), owned("u-1")),
                    ("id".to_owned(), owned("HomeNet")),
                ]),
            ),
            (
                "802-11-wireless".to_owned(),
                HashMap::from([("ssid".to_owned(), owned(b"HomeNet".to_vec()))]),
            ),
            ("802-11-wireless-security".to_owned(), security),
        ])
    }

    #[test]
    fn a_request_reads_what_the_loop_needs() {
        let request = SecretRequest::parse(
            &settings("wpa-psk", AGENT_OWNED, None),
            "/settings/3",
            "802-11-wireless-security",
            ALLOW_INTERACTION | REQUEST_NEW,
        );
        assert_eq!(request.uuid, "u-1");
        assert_eq!(request.id, "HomeNet");
        assert_eq!(request.ssid, "HomeNet");
        assert_eq!(request.security, Some(Security::WpaPsk));
        assert!(request.agent_owned && !request.not_saved);
        assert!(request.allow_interaction && request.request_new);

        let quiet = SecretRequest::parse(&settings("sae", 0, None), "/s", "x", 0);
        assert_eq!(quiet.security, Some(Security::Sae));
        assert!(!quiet.agent_owned && !quiet.allow_interaction);
    }

    #[test]
    fn enterprise_and_owe_are_nothing_the_prompt_asks_for() {
        for key_mgmt in ["wpa-eap", "owe", "ieee8021x"] {
            let request = SecretRequest::parse(&settings(key_mgmt, 0, None), "/s", "x", 1);
            assert_eq!(request.security, None, "{key_mgmt}");
        }
        assert_eq!(
            SecretRequest::parse(&settings("none", 0, None), "/s", "x", 1).security,
            Some(Security::Wep)
        );
    }

    #[test]
    fn only_agent_owned_saved_secrets_go_in_the_keyring() {
        let found = agent_owned_secrets(&settings("wpa-psk", AGENT_OWNED, Some("pw")));
        assert_eq!(
            found,
            vec![(
                "802-11-wireless-security".to_owned(),
                "psk".to_owned(),
                "pw".to_owned()
            )]
        );
        assert!(agent_owned_secrets(&settings("wpa-psk", 0, Some("pw"))).is_empty());
        assert!(
            agent_owned_secrets(&settings("wpa-psk", AGENT_OWNED | NOT_SAVED, Some("pw")))
                .is_empty()
        );
        assert!(agent_owned_secrets(&settings("wpa-psk", AGENT_OWNED, None)).is_empty());
    }

    #[test]
    fn the_reply_is_one_key_in_one_section() {
        let reply = secrets("802-11-wireless-security", "psk", "pw");
        let value: String = reply["802-11-wireless-security"]["psk"]
            .try_clone()
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(value, "pw");
    }
}
