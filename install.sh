#!/bin/sh
# Builds waypixel, copies the binary to a bin directory, and adds the systemd
# user unit that starts the daemon at login.
#
# Usage: ./install.sh
#   BINDIR=<dir>   target directory for the binary (default: ~/.cargo/bin)
set -eu

cd "$(dirname "$0")"
BINDIR="${BINDIR:-$HOME/.cargo/bin}"
UNITDIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

cargo build --release --locked

# `install` replaces the file, so this also works while the daemon runs
install -Dm755 target/release/waypixel "$BINDIR/waypixel"
echo "installed $BINDIR/waypixel"

mkdir -p "$UNITDIR"
sed "s|^ExecStart=.*|ExecStart=$BINDIR/waypixel daemon|" waypixel.service > "$UNITDIR/waypixel.service"
echo "installed $UNITDIR/waypixel.service"

systemctl --user daemon-reload
systemctl --user enable waypixel.service
# restart: a daemon that runs gets the new binary
systemctl --user restart waypixel.service
echo "waypixel.service is enabled and started"
