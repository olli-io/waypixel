#!/bin/sh
# Downloads the released waypixel binary, copies it to a bin directory, and adds
# the systemd user unit that starts the daemon at login.
#
# Usage: ./install.sh
#   BINDIR=<dir>      target directory for the binary (default: ~/.local/bin)
#   VERSION=<vX.Y.Z>  release to install (default: the latest release)
set -eu

REPO="olli-io/waypixel"
BINDIR="${BINDIR:-$HOME/.local/bin}"
UNITDIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) ASSET="waypixel-x86_64-linux" ;;
    Linux-aarch64) ASSET="waypixel-aarch64-linux" ;;
    *) echo "no released binary for $(uname -s) $(uname -m): build from source" >&2; exit 1 ;;
esac

if [ -n "${VERSION:-}" ]; then
    URL="https://github.com/$REPO/releases/download/$VERSION"
else
    URL="https://github.com/$REPO/releases/latest/download"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

for f in "$ASSET" waypixel.service SHA256SUMS; do
    curl -fsSL -o "$TMP/$f" "$URL/$f"
done
(cd "$TMP" && grep -E "  ($ASSET|waypixel\.service)\$" SHA256SUMS | sha256sum -c --quiet -)

# `install` replaces the file, so this also works while the daemon runs
install -Dm755 "$TMP/$ASSET" "$BINDIR/waypixel"
echo "installed $BINDIR/waypixel"

mkdir -p "$UNITDIR"
sed "s|^ExecStart=.*|ExecStart=$BINDIR/waypixel daemon|" "$TMP/waypixel.service" > "$UNITDIR/waypixel.service"
echo "installed $UNITDIR/waypixel.service"

systemctl --user daemon-reload
systemctl --user enable waypixel.service
# restart: a daemon that runs gets the new binary
systemctl --user restart waypixel.service
echo "waypixel.service is enabled and started"
