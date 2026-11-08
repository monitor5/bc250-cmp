#!/bin/sh
# Usage: sudo ./install.sh   (run from the directory containing bc250mon + deploy files)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
bin="$here/bc250mon"; [ -f "$bin" ] || bin="$here/../bc250mon"
install -m 0755 "$bin" /usr/local/bin/bc250mon
[ -f /etc/default/bc250mon ] || install -m 0644 "$here/bc250mon.default" /etc/default/bc250mon
install -m 0644 "$here/bc250mon.service" /etc/systemd/system/bc250mon.service
systemctl daemon-reload
echo "Edit /etc/default/bc250mon (LISTEN / NAME / PEERS), then:"
echo "  sudo systemctl enable --now bc250mon"
