#!/usr/bin/env bash
# One graphical session for the Linux accessibility checks: display, window
# manager, a foreground look at the probe application, then the harness.
#
# Kept as a script rather than inlined in the workflow because the interesting
# failure is "the application never mapped a window", and that is only visible
# if the application is run where its own output can be seen.
set -uo pipefail

echo "== DISPLAY=${DISPLAY:-unset} SESSION_BUS=${DBUS_SESSION_BUS_ADDRESS:-unset}"

echo "== pygobject / gtk3"
python3 - <<'PY'
import gi

gi.require_version("Gtk", "3.0")
from gi.repository import Gtk

print("gtk", Gtk.get_major_version(), ".", Gtk.get_minor_version())
PY

echo "== probe in the background, then what X sees"
python3 tests/linux_atspi/probe_gtk.py /tmp/preflight.txt &
probe=$!
sleep 5
if ! kill -0 "$probe" 2>/dev/null; then
  echo "the probe exited early with status $?"
fi
xwininfo -root -tree | sed -n '1,14p'
kill "$probe" 2>/dev/null || true
wait "$probe" 2>/dev/null || true

echo "== harness"
python3 scripts/linux-atspi-runtime.py
