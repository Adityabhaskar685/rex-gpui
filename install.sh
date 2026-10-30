#!/bin/sh
# Install rex-gpui as a desktop app for the current user:
#   ./install.sh            build the release binary and install it
#   ./install.sh uninstall  remove what install put in place
#
# The binary goes to ~/.local/bin, the launcher to ~/.local/share/applications, and the icon
# to ~/.local/share/icons. Settings in ~/.config/rex-gpui stay either way.
set -eu

cd "$(dirname "$0")"
bin_dir="${XDG_BIN_HOME:-$HOME/.local/bin}"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"
apps_dir="$data_dir/applications"
icon_dir="$data_dir/icons/hicolor/scalable/apps"

refresh() {
    command -v update-desktop-database >/dev/null && update-desktop-database -q "$apps_dir" || true
    command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t "$data_dir/icons/hicolor" || true
}

if [ "${1:-}" = uninstall ]; then
    rm -f "$bin_dir/rex-gpui" "$apps_dir/rex-gpui.desktop" "$icon_dir/rex-gpui.svg"
    refresh
    echo "Removed rex-gpui."
    exit 0
fi

cargo build --release
install -Dm755 target/release/rex-gpui "$bin_dir/rex-gpui"
install -Dm644 packaging/rex-gpui.svg "$icon_dir/rex-gpui.svg"
# Point the launcher at the installed binary, since ~/.local/bin isn't always on the
# desktop session's PATH.
mkdir -p "$apps_dir"
sed "s|^Exec=rex-gpui|Exec=$bin_dir/rex-gpui|" packaging/rex-gpui.desktop > "$apps_dir/rex-gpui.desktop"
chmod 644 "$apps_dir/rex-gpui.desktop"
refresh
echo "Installed rex-gpui to $bin_dir. Find it as \"Rex\" in your apps."
