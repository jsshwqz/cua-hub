#!/usr/bin/env bash
# Linux AT-SPI smoke test: launch a small GTK window and snapshot the tree.
# Usage: dbus-run-session -- bash scripts/a11y_smoke.sh [tree-binary]
set -u

BIN="${1:-/tmp/cua-target/debug/examples/atspi_tree}"
export DISPLAY="${DISPLAY:-:0}"
export GTK_MODULES="${GTK_MODULES:-gail:atk-bridge}"
export GTK_A11Y="${GTK_A11Y:-atspi}"
export NO_AT_BRIDGE=0

# Enable the accessibility bridge in the session.
gsettings set org.gnome.desktop.interface toolkit-accessibility true 2>/dev/null || true
gsettings set org.a11y.Status IsEnabled true 2>/dev/null || true

python3 - <<'PY' >/tmp/a11y_app.log 2>&1 &
import gi
gi.require_version('Gtk', '3.0')
from gi.repository import Gtk

win = Gtk.Window(title='CUA AT-SPI Demo')
win.set_default_size(420, 180)
box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
entry = Gtk.Entry()
entry.set_text('hello atspi')
entry.set_name('cua-entry')
btn = Gtk.Button(label='Click Me')
btn.set_name('cua-button')
box.pack_start(entry, False, False, 0)
box.pack_start(btn, False, False, 0)
win.add(box)
win.connect('destroy', Gtk.main_quit)
win.show_all()
Gtk.main()
PY
APP=$!
trap 'kill $APP 2>/dev/null' EXIT
sleep 3

if ! ps -p "$APP" >/dev/null 2>&1; then
  echo "GTK app exited early; log:" >&2
  head -20 /tmp/a11y_app.log >&2
  exit 2
fi

"$BIN"
