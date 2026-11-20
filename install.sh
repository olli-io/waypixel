#!/bin/sh
# Builds pixelwp, copies the binary to a bin directory, and adds the systemd
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
install -Dm755 target/release/pixelwp "$BINDIR/pixelwp"
echo "installed $BINDIR/pixelwp"

mkdir -p "$UNITDIR"
sed "s|^ExecStart=.*|ExecStart=$BINDIR/pixelwp daemon|" pixelwp.service > "$UNITDIR/pixelwp.service"
echo "installed $UNITDIR/pixelwp.service"

systemctl --user daemon-reload
systemctl --user enable pixelwp.service
# restart: a daemon that runs gets the new binary
systemctl --user restart pixelwp.service
echo "pixelwp.service is enabled and started"
