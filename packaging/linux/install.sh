#!/bin/sh
# Installs AtroxaBot for the current user: the binary, plus the launcher entry
# and icon desktops use to show it in menus, docks and task switchers.
set -eu
cd "$(dirname "$0")"
bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
mkdir -p "$bin" "$data/applications" "$data/icons/hicolor/256x256/apps"
cp atroxabot "$bin/atroxabot"
chmod 755 "$bin/atroxabot"
cp atroxabot.png "$data/icons/hicolor/256x256/apps/atroxabot.png"
# An absolute Exec works even when the bin folder is not on PATH.
sed "s|^Exec=.*|Exec=$bin/atroxabot|" atroxabot.desktop > "$data/applications/atroxabot.desktop"
echo "Installed AtroxaBot to $bin/atroxabot; it is now in your applications menu."
