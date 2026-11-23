#!/bin/sh
# Installs MapStick for the current user, into ~/.local (or $PREFIX).
# Run it from the unpacked release archive:  ./install.sh
# Remove with:                               ./install.sh --uninstall
set -eu

here=$(cd "$(dirname "$0")" && pwd)
prefix=${PREFIX:-$HOME/.local}
apps="$prefix/share/applications"
icons="$prefix/share/icons/hicolor"

if [ "${1:-}" = "--uninstall" ]; then
    # The background check would keep failing without the program.
    systemctl --user disable --now mapstick-check.timer 2>/dev/null || true
    rm -f "$HOME/.config/systemd/user/mapstick-check.timer" \
          "$HOME/.config/systemd/user/mapstick-check.service" \
          "$HOME/.config/systemd/user/mapstick-check-failed.service"
    systemctl --user daemon-reload 2>/dev/null || true
    rm -f "$prefix/bin/mapstick" "$apps/mapstick.desktop" \
          "$icons/scalable/apps/mapstick.svg" "$icons"/*/apps/mapstick.png
    echo "MapStick removed. Your favourites stay in ~/.config/mapstick."
    exit 0
fi

install -Dm755 "$here/mapstick" "$prefix/bin/mapstick"
mkdir -p "$apps"
sed "s|^Exec=mapstick |Exec=$prefix/bin/mapstick |" "$here/mapstick.desktop" > "$apps/mapstick.desktop"
install -Dm644 "$here/icons/mapstick.svg" "$icons/scalable/apps/mapstick.svg"
for size in 16 32 48 64 128 256 512; do
    install -Dm644 "$here/icons/mapstick-$size.png" "$icons/${size}x$size/apps/mapstick.png"
done

update-desktop-database "$apps" 2>/dev/null || true
gtk-update-icon-cache -q "$icons" 2>/dev/null || true

echo "MapStick installed to $prefix/bin/mapstick — find it in your application menu."
