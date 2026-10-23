#!/usr/bin/env python3
"""A GTK3 window with controls the AT-SPI backend is asked to find and act on.

Written for the Linux runtime check: every accessible name here is an assertion
target in scripts/linux-atspi-runtime.py, so renaming one must fail that check
rather than pass silently.
"""

import sys

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import GLib, Gdk, Gtk  # noqa: E402

APP_ID = "cua-atspi-probe"

# The window list and the accessibility tree have to agree on who owns this
# frame: `set_prgname` names the AT-SPI application, `set_program_class` names
# the X11 WM_CLASS, and Python leaves the latter as the script filename unless
# something sets it. Matching is only the backend's problem if both sides were
# allowed to differ, so they do not here.
GLib.set_prgname(APP_ID)
Gdk.set_program_class(APP_ID)

RESULT = sys.argv[1] if len(sys.argv) > 1 else "/tmp/probe_result.txt"


class Probe(Gtk.Window):
    def __init__(self):
        super().__init__(title="cua-atspi-probe")
        self.set_default_size(420, 180)
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        box.set_border_width(12)

        box.pack_start(Gtk.Label(label="type into the entry below"), False, False, 0)

        self.entry = Gtk.Entry()
        self.entry.get_accessible().set_name("probe-entry")
        box.pack_start(self.entry, False, False, 0)

        self.submit = Gtk.Button(label="probe-submit")
        self.submit.connect("clicked", self.on_submit)
        box.pack_start(self.submit, False, False, 0)

        self.toggle = Gtk.CheckButton(label="probe-toggle")
        box.pack_start(self.toggle, False, False, 0)

        self.add(box)
        self.connect("destroy", Gtk.main_quit)

    def on_submit(self, _button):
        # Reaching this file is the proof that a DoAction really fired the
        # control, not merely that the tree described it.
        with open(RESULT, "w", encoding="utf-8") as handle:
            handle.write(self.entry.get_text())
        Gtk.main_quit()


window = Probe()
window.show_all()
Gtk.main()
