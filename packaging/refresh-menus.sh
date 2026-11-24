#!/bin/sh
# Tells the desktop that a launcher or icon was added or removed, so the app
# menu shows it (with its icon) without logging out.
#
# KDE's menu reads a cache (ksycoca) that isn't always rebuilt when a file
# lands in ~/.local/share; until it is, the menu entry has no icon even though
# the taskbar finds it once the app is running. Inside a distrobox the cache
# belongs to the host, so it's rebuilt there.
#
# Running KDE programs (the Plasma shell: menu, search, pinned launchers)
# also remember icons they looked for and didn't find, and only look again
# now and then, so they're told icons changed (KIconLoader's D-Bus signal,
# as KDE's icon settings send) and drop what they remember.
#
# Usage: refresh-menus.sh <applications dir> <icons dir>. Never fails.

apps=$1
icons=$2

touch "$icons/hicolor" 2>/dev/null
if command -v update-desktop-database >/dev/null 2>&1; then
	update-desktop-database "$apps" >/dev/null 2>&1
fi
# A user icon cache, if some other program made one, lists only the icons
# there when it was made; GTK trusts it until the folder is touched again.
# -t: a user hicolor folder usually has no index.theme of its own.
if command -v gtk-update-icon-cache >/dev/null 2>&1 && [ -f "$icons/hicolor/icon-theme.cache" ]; then
	gtk-update-icon-cache -q -f -t "$icons/hicolor" >/dev/null 2>&1
fi

refresh='for k in kbuildsycoca6 kbuildsycoca5; do
	if command -v "$k" >/dev/null 2>&1; then "$k" >/dev/null 2>&1; break; fi
done
if command -v dbus-send >/dev/null 2>&1; then
	dbus-send --session --type=signal /KIconLoader org.kde.KIconLoader.iconChanged int32:0 >/dev/null 2>&1
elif command -v gdbus >/dev/null 2>&1; then
	gdbus emit --session --object-path /KIconLoader --signal org.kde.KIconLoader.iconChanged 0 >/dev/null 2>&1
fi'
if command -v kbuildsycoca6 >/dev/null 2>&1 || command -v kbuildsycoca5 >/dev/null 2>&1; then
	sh -c "$refresh"
elif command -v distrobox-host-exec >/dev/null 2>&1; then
	distrobox-host-exec sh -c "$refresh" >/dev/null 2>&1
else
	sh -c "$refresh"
fi
exit 0
