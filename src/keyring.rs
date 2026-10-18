// SPDX-License-Identifier: GPL-3.0-or-later
//! Wi-Fi passwords in the session keyring, through the Secret Service API.
//!
//! # When the keyring holds them at all
//!
//! A new connection is **per-user, with its password in the keyring, when a keyring is available,
//! and system-wide otherwise** -- see `_docs/wlrix-network-roadmap.md`. "Available" is the test
//! `Wlrix.Files` uses: something answers `org.freedesktop.secrets` and the default collection is
//! unlocked. In a wlRIX session it is: gnome-keyring runs as a user unit and the greeter's PAM
//! stack unlocks the login collection with the password just typed. A locked one is not unlocked
//! by prompt -- gcr-prompter needs a parent window this daemon does not have -- and counts as no
//! keyring.
//!
//! # NetworkManager's schema, exactly
//!
//! Items are written as nm-applet and GNOME Shell write them: `xdg:schema` set to
//! `org.freedesktop.NetworkManager.Connection`, with `connection-uuid`, `setting-name` and
//! `setting-key`. So a password saved under GNOME is found here, and one saved here is found from
//! GNOME -- the same reasoning as `Wlrix.Files` using GNOME's network-password schema. Lookups ask
//! for fewer attributes than are written, because the Secret Service matches an item whose
//! attributes are a superset of the query.
//!
//! # Plain transfer
//!
//! Secrets cross the session bus unencrypted (the `plain` algorithm). The bus is local and
//! per-user, and anything that can read it can already ask the keyring for the same items; the
//! Diffie-Hellman alternative protects against nothing that is not already inside the session.

use std::collections::HashMap;

use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const NAME: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE: &str = "org.freedesktop.Secret.Service";
const COLLECTION: &str = "org.freedesktop.Secret.Collection";
const ITEM: &str = "org.freedesktop.Secret.Item";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// NetworkManager's own schema name, which is what makes the items shared with nm-applet.
const SCHEMA: &str = "org.freedesktop.NetworkManager.Connection";

/// `(oayays)`: session, parameters, value, content type.
type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

/// The attributes one secret is filed under.
fn attributes<'a>(
    uuid: &'a str,
    setting: &'a str,
    key: Option<&'a str>,
) -> HashMap<&'a str, &'a str> {
    let mut map = HashMap::from([
        ("xdg:schema", SCHEMA),
        ("connection-uuid", uuid),
        ("setting-name", setting),
    ]);
    if let Some(key) = key {
        map.insert("setting-key", key);
    }
    map
}

/// The label nm-applet gives the same item, so the two look alike in Seahorse.
fn label(connection: &str, setting: &str, key: &str) -> String {
    format!("Network secret for {connection}/{setting}/{key}")
}

/// The session keyring, when there is a usable one.
pub struct Keyring<'a> {
    connection: &'a zbus::blocking::Connection,
    collection: OwnedObjectPath,
}

impl<'a> Keyring<'a> {
    /// The default collection, if something serves it and it is unlocked. `None` otherwise --
    /// which is an answer, not an error: it means new connections are system-wide.
    pub fn open(connection: &'a zbus::blocking::Connection) -> Option<Self> {
        let reply = connection
            .call_method(
                Some(NAME),
                SERVICE_PATH,
                Some(SERVICE),
                "ReadAlias",
                &("default",),
            )
            .ok()?;
        let collection: OwnedObjectPath = reply.body().deserialize().ok()?;
        if collection.as_str() == "/" {
            return None;
        }
        let reply = connection
            .call_method(
                Some(NAME),
                collection.as_str(),
                Some(PROPERTIES),
                "Get",
                &(COLLECTION, "Locked"),
            )
            .ok()?;
        let locked: OwnedValue = reply.body().deserialize().ok()?;
        if bool::try_from(locked).unwrap_or(true) {
            return None;
        }
        Some(Self {
            connection,
            collection,
        })
    }

    fn session(&self) -> zbus::Result<OwnedObjectPath> {
        let reply = self.connection.call_method(
            Some(NAME),
            SERVICE_PATH,
            Some(SERVICE),
            "OpenSession",
            &("plain", Value::from("")),
        )?;
        let (_output, session): (OwnedValue, OwnedObjectPath) = reply.body().deserialize()?;
        Ok(session)
    }

    /// Store one secret, replacing any earlier one filed under the same attributes.
    pub fn store(
        &self,
        connection_id: &str,
        uuid: &str,
        setting: &str,
        key: &str,
        value: &str,
    ) -> zbus::Result<()> {
        let session = self.session()?;
        let mut properties: HashMap<&str, Value> = HashMap::new();
        properties.insert(
            "org.freedesktop.Secret.Item.Label",
            Value::from(label(connection_id, setting, key)),
        );
        properties.insert(
            "org.freedesktop.Secret.Item.Attributes",
            Value::from(attributes(uuid, setting, Some(key))),
        );
        let secret: Secret = (
            session,
            Vec::new(),
            value.as_bytes().to_vec(),
            "text/plain".to_owned(),
        );
        self.connection.call_method(
            Some(NAME),
            self.collection.as_str(),
            Some(COLLECTION),
            "CreateItem",
            &(properties, secret, true),
        )?;
        Ok(())
    }

    /// Every secret filed for one setting of one connection, by key.
    pub fn find(&self, uuid: &str, setting: &str) -> zbus::Result<HashMap<String, String>> {
        let unlocked = self.search(&attributes(uuid, setting, None))?;
        if unlocked.is_empty() {
            return Ok(HashMap::new());
        }
        let session = self.session()?;
        let paths: Vec<ObjectPath> = unlocked.iter().map(|path| path.as_ref()).collect();
        let reply = self.connection.call_method(
            Some(NAME),
            SERVICE_PATH,
            Some(SERVICE),
            "GetSecrets",
            &(paths, session),
        )?;
        let secrets: HashMap<OwnedObjectPath, Secret> = reply.body().deserialize()?;
        let mut found = HashMap::new();
        for (item, (_, _, value, _)) in secrets {
            let Some(key) = self.attribute(&item, "setting-key") else {
                continue;
            };
            if let Ok(text) = String::from_utf8(value) {
                found.insert(key, text);
            }
        }
        Ok(found)
    }

    /// Remove every secret filed for one connection.
    pub fn delete(&self, uuid: &str) -> zbus::Result<()> {
        let mut query = HashMap::from([("xdg:schema", SCHEMA)]);
        query.insert("connection-uuid", uuid);
        for item in self.search(&query)? {
            // A locked item would need a prompt to delete; it is skipped, not fought over.
            let _ =
                self.connection
                    .call_method(Some(NAME), item.as_str(), Some(ITEM), "Delete", &());
        }
        Ok(())
    }

    /// The unlocked items whose attributes include `query`.
    fn search(&self, query: &HashMap<&str, &str>) -> zbus::Result<Vec<OwnedObjectPath>> {
        let reply = self.connection.call_method(
            Some(NAME),
            SERVICE_PATH,
            Some(SERVICE),
            "SearchItems",
            &(query,),
        )?;
        let (unlocked, _locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) =
            reply.body().deserialize()?;
        Ok(unlocked)
    }

    fn attribute(&self, item: &OwnedObjectPath, name: &str) -> Option<String> {
        let reply = self
            .connection
            .call_method(
                Some(NAME),
                item.as_str(),
                Some(PROPERTIES),
                "Get",
                &(ITEM, "Attributes"),
            )
            .ok()?;
        let value: OwnedValue = reply.body().deserialize().ok()?;
        let map: HashMap<String, String> = value.try_into().ok()?;
        map.get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_are_filed_as_networkmanager_files_them() {
        let filed = attributes("1234", "802-11-wireless-security", Some("psk"));
        assert_eq!(
            filed["xdg:schema"],
            "org.freedesktop.NetworkManager.Connection"
        );
        assert_eq!(filed["connection-uuid"], "1234");
        assert_eq!(filed["setting-name"], "802-11-wireless-security");
        assert_eq!(filed["setting-key"], "psk");
        // A lookup leaves the key out, so it finds every key the setting has.
        assert!(!attributes("1234", "x", None).contains_key("setting-key"));
    }

    #[test]
    fn the_label_matches_nm_applets() {
        assert_eq!(
            label("HomeNet", "802-11-wireless-security", "psk"),
            "Network secret for HomeNet/802-11-wireless-security/psk"
        );
    }
}
