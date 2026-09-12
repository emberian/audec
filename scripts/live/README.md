# Live desktop scenarios

These drive the real audec binary through its control socket
(`AUDEC_CONTROL_SOCKET`) and read back what the app believes, so a change to
audio, transport, creation, or workspace flows is verified on the desktop
rather than only headless. Each scenario launches the app on the material you
pass, prints what it expects at each step, and prints the app's status.

    scripts/live/loop_state_machine.sh  /path/to/material.flac
    scripts/live/make_beat_audible.sh   /path/to/material.flac   # exports before/after, compares with sox + numpy
    scripts/live/editors_and_windows.sh /path/to/material.flac
    scripts/live/sampler_pane_teardown.sh /path/to/material.flac  # closing a pane leaves nothing playing
    scripts/live/audition_diff.sh /path/to/material.flac  # plays new minus old between render cohorts; checks status.diff against the two exports
    scripts/live/tempo_and_routing.sh   /path/to/material.flac   # tempo/meter at the playhead; what each bus carries
    scripts/live/readings_and_compare.sh /path/to/material.flac  # export a project reading, import it verified, refusals by name
    scripts/live/inserts.sh             /path/to/material.flac   # + insert adds a native filter the graph renders; exports before/after, sox stat + centroid
    scripts/live/drops.sh               /path/to/material.flac   # the mixer strip's routing drop (by action) moves a channel's audio onto another bus; the pattern library rail renders
    scripts/live/reverse_flow.sh        /path/to/material.flac   # name a lens, wait for its findings, keep and compare one with no pane open, read the Compare branch; every refusal verbatim

`ctl.py '<json>' ...` sends raw requests (`status`, `actions`, `action {id, parameters}`, `open`, `seek`, `select`, `click`, `drag`, `loop`, `play`/`pause`/`stop`, `export`, `objects`, `finding {index|address, do}`, `lens {view, control}`, `reading_import {path, manifest_digest}`, `reading_export {path}`, `quit`).

- `action` carries the `parameters` an id declares — a JSON object of name to
  bool, whole number, or string. `audec.workspace.activate {view}` is the first
  one: it focuses a pane by number instead of walking tabs. A name the id does
  not declare is refused, naming the ones it does take; floating point is not in
  the parameter vocabulary and is refused where it is written.
- `audec.lens.{waterfall,rhythm,components,separation,loom}` name the five
  lenses. They show the pane the workspace already holds for that lens rather
  than stacking another one on the same analysis. (They replace
  `audec.analysis.*`, which opened a second pane every time.)
- `finding {index | address, do: open | keep | compare | apply | sample |
  audition:<kind>}` acts on one published Finding with no reverse pane open,
  through the same lifecycle and the same event the pane's RESULT ACTIONS use.
  `status.findings` lists what there is to act on: index, address, title, kind,
  the lens that published it, the span behind it, and for every verb either
  `available`, `pending`, `completed` with its durable revision, or `refused`
  with the reason in the words the pane would have shown. The reply repeats the
  app's own notice verbatim.
- `status.lenses[*]` carries `state` (`Idle`/`Analyzing`/`Ready`/`Failed` with
  `failure`), `span` (the window the lens is showing, in frames and seconds,
  with `basis` = `result` when it is the window an analysis actually read,
  `analyzing` while one is in flight, `viewport` when the lens is only drawing),
  and `findings` (how many it has published). `lens {view, control}` still
  drives one lens's header controls by name.
- `status.preview` is the finite preview bus by owner, with the pad gates the
  workbench still holds; `status.diff` is the null between the active render
  cohort and the one it retired, with its RMS inside and outside the auditioned
  span.

`tree.py` pretty-prints an
`objects` reply. `AUDEC_BIN` selects the binary (default `target/debug/audec`),
`AUDEC_LIVE_DIR` the scratch directory. Window screenshots
(`winlist.swift` + `screencapture -l`) need Screen Recording permission for
the terminal.
