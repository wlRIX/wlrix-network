// SPDX-License-Identifier: GPL-3.0-or-later
//! The password dialog, `wlrix-network-prompt`, run as a child process.
//!
//! The dialog is an Avalonia application in `wlrix-apps`, like the pinentry and the polkit agent,
//! and this daemon draws nothing. The exchange is one line of JSON each way:
//!
//! ```text
//! -> {"mode":"password","ssid":"HomeNet","security":"wpa-psk","retry":false}
//! <- {"ssid":"HomeNet","security":"wpa-psk","password":"..."}
//! ```
//!
//! and Cancel is an exit with nothing written. The password exists in this process only on its
//! way to NetworkManager or the keyring; nothing writes it anywhere else, and nothing logs it.
//!
//! The child's stdout is read on a thread of its own, since a person may take minutes to answer,
//! and the answer comes back to the main loop by channel with the token it was started under.
//! The loop keeps the [`Child`] so a `CancelGetSecrets` can close the dialog.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};

use serde::{Deserialize, Serialize};

/// The program, found on `PATH`. `WLRIX_NETWORK_PROMPT` names another, for running a build of the
/// dialog that is not installed.
const PROGRAM: &str = "wlrix-network-prompt";

/// A network's security, as the dialog and NetworkManager's `key-mgmt` both name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Security {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "wpa-psk")]
    WpaPsk,
    #[serde(rename = "sae")]
    Sae,
    #[serde(rename = "wep")]
    Wep,
}

impl Security {
    /// What goes in `802-11-wireless-security.key-mgmt`. WEP is `none` there -- static keys, no
    /// key management -- with the key in `wep-key0`.
    pub fn key_mgmt(self) -> &'static str {
        match self {
            Security::None | Security::Wep => "none",
            Security::WpaPsk => "wpa-psk",
            Security::Sae => "sae",
        }
    }

    /// Which key of the security setting holds the password.
    pub fn secret_key(self) -> Option<&'static str> {
        match self {
            Security::None => None,
            Security::WpaPsk | Security::Sae => Some("psk"),
            Security::Wep => Some("wep-key0"),
        }
    }

    /// The flags key that says who stores [`Security::secret_key`].
    pub fn flags_key(self) -> Option<&'static str> {
        match self {
            Security::None => None,
            Security::WpaPsk | Security::Sae => Some("psk-flags"),
            Security::Wep => Some("wep-key-flags"),
        }
    }
}

/// What the dialog is asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum Request {
    /// The password for a network that is known by name.
    Password {
        ssid: String,
        security: Security,
        /// NetworkManager asked again because the last key did not work.
        retry: bool,
    },
    /// Join Other Network: a hidden network's name, security and password.
    Other,
}

/// What the person entered.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Answer {
    pub ssid: String,
    pub security: Security,
    pub password: String,
}

/// One line of request.
pub fn request_line(request: &Request) -> String {
    serde_json::to_string(request).expect("a request always serializes")
}

/// The dialog's answer, or `None` for a Cancel, a crash, or anything else that is not one line of
/// the expected shape.
pub fn parse_answer(text: &str) -> Option<Answer> {
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    serde_json::from_str(line).ok()
}

/// Start the dialog. The answer arrives on `answers` with `token`, once it is given -- `None` for
/// a Cancel or a dialog that would not start. Returns the child, for closing it early.
pub fn spawn(
    request: &Request,
    token: u64,
    answers: calloop::channel::Sender<(u64, Option<Answer>)>,
) -> Result<Child, String> {
    let program = std::env::var("WLRIX_NETWORK_PROMPT").unwrap_or_else(|_| PROGRAM.to_owned());
    let mut child = Command::new(&program)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|err| format!("could not start {program}: {err}"))?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let line = request_line(request);
    std::thread::Builder::new()
        .name("prompt".into())
        .spawn(move || {
            // Written here rather than before the thread starts: a dialog that never reads its
            // stdin would otherwise block the main loop on a full pipe.
            let _ = writeln!(stdin, "{line}");
            drop(stdin);
            let mut text = String::new();
            let _ = stdout.read_to_string(&mut text);
            let _ = answers.send((token, parse_answer(&text)));
        })
        .map_err(|err| format!("could not start the prompt thread: {err}"))?;
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_the_shape_the_dialog_parses() {
        let line = request_line(&Request::Password {
            ssid: "HomeNet".into(),
            security: Security::Sae,
            retry: true,
        });
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["mode"], "password");
        assert_eq!(value["ssid"], "HomeNet");
        assert_eq!(value["security"], "sae");
        assert_eq!(value["retry"], true);
        assert_eq!(request_line(&Request::Other), r#"{"mode":"other"}"#);
    }

    #[test]
    fn an_answer_is_read_and_anything_else_is_a_cancel() {
        let answer = parse_answer(r#"{"ssid":"Café","security":"wpa-psk","password":"a\"b"}"#)
            .expect("an answer");
        assert_eq!(answer.ssid, "Café");
        assert_eq!(answer.security, Security::WpaPsk);
        assert_eq!(answer.password, "a\"b");
        assert_eq!(parse_answer(""), None, "Cancel writes nothing");
        assert_eq!(parse_answer("garbage"), None);
        assert_eq!(
            parse_answer(r#"{"ssid":"x","security":"rot13","password":""}"#),
            None
        );
    }

    #[test]
    fn each_security_says_where_its_key_goes() {
        assert_eq!(Security::WpaPsk.key_mgmt(), "wpa-psk");
        assert_eq!(Security::Sae.secret_key(), Some("psk"));
        assert_eq!(Security::Wep.key_mgmt(), "none");
        assert_eq!(Security::Wep.secret_key(), Some("wep-key0"));
        assert_eq!(Security::Wep.flags_key(), Some("wep-key-flags"));
        assert_eq!(Security::None.secret_key(), None);
    }

    #[test]
    fn a_missing_dialog_is_an_error_not_a_hang() {
        // SAFETY of the test: the variable is read once, at spawn, in this process only.
        unsafe { std::env::set_var("WLRIX_NETWORK_PROMPT", "/nonexistent/prompt") };
        let (sender, _channel) = calloop::channel::channel();
        assert!(spawn(&Request::Other, 1, sender).is_err());
        unsafe { std::env::remove_var("WLRIX_NETWORK_PROMPT") };
    }
}
