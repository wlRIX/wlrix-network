#!/usr/bin/env just --justfile
name := 'wlrix-network'

rootdir := ''
prefix := '/usr'

base-dir := absolute_path(clean(rootdir / prefix))
bin-dir := base-dir / 'bin'

bin-src := 'target' / 'release' / name
bin-dst := bin-dir / name

default:
  @just --list

release:
  cargo build --release

lint:
  cargo clippy --all-targets -- -D warnings

test:
  cargo test

# Install the daemon.
#
# One file. No `.desktop` entry, since it is not something a person starts, and no D-Bus activation
# file: nothing calls it by name -- it registers with the tray, not the other way round.
#
# Deliberately does not build: this is normally run as root, and building as root leaves a target
# directory nobody can write to afterwards.
#
#     just release && sudo just install
[doc("Install the daemon (build first; run as root)")]
install:
  #!/usr/bin/env bash
  set -euo pipefail
  if [ ! -x '{{bin-src}}' ]; then
    echo "no release build -- run 'just release' first" >&2
    exit 1
  fi
  install -Dm0755 '{{bin-src}}' '{{bin-dst}}'
  echo "installed {{bin-dst}}"
  echo
  echo "wlrix-session starts this by name off PATH. It needs NetworkManager; with no"
  echo "wireless device it shows nothing. 'wlrix-network --status' says what it sees."

[doc("Remove what install put down")]
uninstall:
  #!/usr/bin/env bash
  set -euo pipefail
  rm -f '{{bin-dst}}'
  echo "removed {{bin-dst}}"

clean:
  cargo clean
