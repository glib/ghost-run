#!/bin/sh
# Tells the desktop that a launcher or icon was added or removed, so the app
# menu shows it (with its icon) without logging out.
#
# KDE's menu reads a cache (ksycoca) that isn't always rebuilt when a file
# lands in ~/.local/share; until it is, the menu entry has no icon even though
# the taskbar finds it once the app is running. Inside a distrobox the cache
# belongs to the host, so it's rebuilt there.
#
# Usage: refresh-menus.sh <applications dir> <icons dir>. Never fails.

apps=$1
icons=$2

touch "$icons/hicolor" 2>/dev/null
if command -v update-desktop-database >/dev/null 2>&1; then
	update-desktop-database "$apps" >/dev/null 2>&1
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1 && [ -f "$icons/hicolor/index.theme" ]; then
	gtk-update-icon-cache -q "$icons/hicolor" >/dev/null 2>&1
fi

rebuild='for k in kbuildsycoca6 kbuildsycoca5; do
	if command -v "$k" >/dev/null 2>&1; then "$k" >/dev/null 2>&1; exit 0; fi
done'
if command -v kbuildsycoca6 >/dev/null 2>&1 || command -v kbuildsycoca5 >/dev/null 2>&1; then
	sh -c "$rebuild"
elif command -v distrobox-host-exec >/dev/null 2>&1; then
	distrobox-host-exec sh -c "$rebuild" >/dev/null 2>&1
fi
exit 0
