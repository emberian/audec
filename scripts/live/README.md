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
    scripts/live/readings_and_compare.sh /path/to/material.flac  # export a project reading, import it verified, refusals by name; save, reopen, and see it re-verified
    scripts/live/finding_to_sound.sh    /path/to/material.flac   # the span each published finding is about, which is what Hear and Make sample on a Findings row stand on
    scripts/live/inserts.sh             /path/to/material.flac   # + insert adds a native filter the graph renders; exports before/after, sox stat + centroid
    scripts/live/drops.sh               /path/to/material.flac   # the mixer strip's routing drop (by action) moves a channel's audio onto another bus; the pattern library rail renders
    scripts/live/reverse_flow.sh        /path/to/material.flac   # name a lens, wait for its findings, keep and compare one with no pane open, read the Compare branch; every refusal verbatim
    scripts/live/clip_edits.sh          /path/to/material.flac   # clip gain, a fade, a marker and a mouse-free placement, each read back from status.arrangement and measured in the exports
    scripts/live/edit_during_render.sh  /path/to/material.flac   # an edit cancels an in-flight render: audio_error stays null, the export says which revision it is rendering for, and it matches the settled master byte for byte
    scripts/live/mixer_sampler.sh       /path/to/material.flac   # a zone plays backwards (the difference lives only inside the beat), inserts reorder by name, refusals verbatim
    scripts/live/pattern_edit.sh        /path/to/material.flac   # the toolbar's Piano / drums opens an editor that can be heard: audec.pattern.audition answers Playing exact pattern audition
    scripts/live/autosave_and_tempo.sh  /path/to/material.flac   # a never-saved project is autosaved; BPM by number; the click is heard, not bounced; a tail that says what it is
    scripts/live/descriptor_rewrite.sh  /path/to/material.flac   # rewrites a pane's descriptor from another pane; the socket keeps answering
    scripts/live/lens_knobs_components.sh /path/to/material.flac # ask for more components and get them; the seconds a component owns; constant-Q says its pitch grid; a refused transform keeps the preference

`ctl.py '<json>' ...` sends raw requests (`status`, `actions`, `action {id, parameters}`, `open`, `seek`, `select`, `click`, `drag`, `loop`, `play`/`pause`/`stop`, `export {path, …, tail_seconds, metronome}`, `tempo {bpm}`, `objects`, `finding {index|address, do}`, `lens {view, control}`, `reading_import {path, manifest_digest}`, `reading_export {path}`, `save {path}`, `quit`).

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
- `status.lenses[*].settings` is every knob that lens owns, at the value it is
  set to: the waterfall's `transform`, `fft_size`, `hop_size`, `window`,
  `db_ceiling`, `db_range`, `cqt_bins_per_octave` and `refused` (the reason the
  last field run could not do the transform that was chosen, or null); the
  components lens's `rank`, `template_length`, `template_seconds` (what that
  length is worth in this material), `iterations`, `shown` (how many components
  the published product has) and `selected_finding`.
- Components `lens` controls: `components-rank-up`/`-down` and
  `components-lag-up`/`-down` change the question (they do not recompute);
  `refresh` is the Refactor that recomputes the whole song at it;
  `component-span:<n>` selects the seconds component *n* owns and seeks there;
  `components-finding-next`/`-previous` move the header's finding cursor. Under
  constant-Q, `fft-size-up`/`-down` step the pitch grid (12 / 24 / 36 bins per
  octave) rather than an FFT length.
- `status.preview` is the finite preview bus by owner, with the pad gates the
  workbench still holds; `status.diff` is the null between the active render
  cohort and the one it retired, with its RMS inside and outside the auditioned
  span.
- `status.arrangement` is the focused arrangement pane's own status line, its
  clip selection and its markers: the pane's refusals reach a script there,
  because `notice` is the Workbench's channel and carries none of them.
- `tempo {bpm}` sets the tempo of the segment the playhead is standing in, the
  same `TempoPointIntent` the ± buttons plan. `export` takes `tail_seconds`
  (rendered as far as the arrangement reaches, silence past it, and the status
  says which) and `metronome` (the click is monitored, never bounced, unless
  asked); `status.metronome` says whether the compiled master carries it.
- `save {path}` is Save As without the file dialog, so a scripted session can
  reopen a package and see what survived.

`tree.py` pretty-prints an
`objects` reply. `AUDEC_BIN` selects the binary (default `target/debug/audec`),
`AUDEC_LIVE_DIR` the scratch directory. Window screenshots
(`winlist.swift` + `screencapture -l`) need Screen Recording permission for
the terminal.
