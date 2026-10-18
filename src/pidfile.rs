// SPDX-License-Identifier: GPL-3.0-or-later
//! A pidfile, so `network.toml` can be reloaded without hunting for the process.
//!
//! `SIGHUP` re-reads the config (see [`crate::signals`]), and the point of that is being able to
//! hide or show the indicator without restarting the daemon -- which needs a pid, and a sibling
//! process cannot read one from the environment. So it goes in a well-known file under the
//! per-user runtime directory, the same arrangement `wlrix-compositor` and `wlrix-idle` use, and
//! the one `wlrix-settings-daemon` looks for.
//!
//! The file is removed on a clean exit via the returned [`Guard`]. A crash leaves it stale; a
//! reader should treat "no such process" as "not running" rather than trusting the file blindly,
//! the way `wlrix-settings-daemon` does.

use std::path::PathBuf;

/// Named for the process, beside the compositor's, the tray's and the sound daemon's.
const PID_NAME: &str = "wlrix-network.pid";

/// `$XDG_RUNTIME_DIR` (owned by one user, cleaned up on logout), else the temp dir -- the same
/// rule every other wlRIX pidfile follows, so they sit together.
fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(std::env::temp_dir)
}

/// Where the pidfile lives.
pub fn path() -> PathBuf {
    runtime_dir().join(PID_NAME)
}

/// Write this process's pid. Returns a guard that removes the file when dropped; failure to
/// write is reported and swallowed, since a missing pidfile is not worth refusing to start over
/// -- it only costs the reload its convenience.
pub fn write() -> Option<Guard> {
    let path = path();
    match create(&path).and_then(|mut file| {
        use std::io::Write;
        writeln!(file, "{}", std::process::id())
    }) {
        Ok(()) => Some(Guard { path }),
        Err(err) => {
            eprintln!("wlrix-network: could not write {}: {err}", path.display());
            None
        }
    }
}

/// Create (or truncate) the pidfile. `O_NOFOLLOW` because the path is predictable and the
/// temp-dir fallback is world-writable: without it, anyone could leave a symlink there and have
/// this process truncate a file of their choosing.
fn create(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Removes the pidfile on drop, so a live pidfile means a live daemon.
pub struct Guard {
    path: PathBuf,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.path) {
            eprintln!(
                "wlrix-network: could not remove {}: {err}",
                self.path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pidfile_sits_beside_the_others() {
        // Hand-kept in step with `wlrix-settings-daemon`'s `Owner::pidfile`, which is what
        // looks for it. A change here that is not mirrored there would make every network.toml
        // setting silently stop applying live.
        assert!(path().ends_with("wlrix-network.pid"));
    }
}
