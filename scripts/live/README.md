# Live desktop scenarios

These drive the real audec binary through its control socket
(`AUDEC_CONTROL_SOCKET`) and read back what the app believes, so a change to
audio, transport, creation, or workspace flows is verified on the desktop
rather than only headless. Each scenario launches the app on the material you
pass, prints what it expects at each step, and prints the app's status.

    scripts/live/loop_state_machine.sh  /path/to/material.flac
    scripts/live/make_beat_audible.sh   /path/to/material.flac   # exports before/after, compares with sox + numpy
    scripts/live/editors_and_windows.sh /path/to/material.flac

`ctl.py '<json>' ...` sends raw requests; `tree.py` pretty-prints an
`objects` reply. `AUDEC_BIN` selects the binary (default `target/debug/audec`),
`AUDEC_LIVE_DIR` the scratch directory. Window screenshots
(`winlist.swift` + `screencapture -l`) need Screen Recording permission for
the terminal.
