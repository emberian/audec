# Analysis instrument audit (2026-09-12)

From the musician's chair, at main after cycle 4 wave 1. Evidence is
file:line at that HEAD. Companion to `UX_EXPOSURE_AUDIT.md` (2026-09-02)
and `DAW_UX_AUDIT.md`.

**Headline.** The measurement layer is honest and the audition layer is
good (HPSS four-way, Loom four-way, rhythm family medoids). Three things
are missing, in this order: the whole analysis half is invisible to
scripting (44 action ids, none naming a lens, a finding, a reading or a
comparison); every analysis parameter that changes evidence is a literal,
so a musician cannot ask a different question of the same audio; and the
finding → sound edge does not exist in the lens where the finding is born.

## Ranked

| # | a musician wants to… | today | smallest correct change | files | size | value |
|---|---|---|---|---|---|---|
| 1 | script the reverse flow (deproject → finding → compare → explanation) | no path; Compare is a pane button and needs an analysis callback: "Compare and Make sample stay with the analysis-result host; this pane has no analysis callback." (`reverse_surface_view.rs:1120`) | one socket verb `finding {index\|address, do: open\|keep\|compare\|apply\|sample\|audition:<kind>}` routed through the `ReverseAnalysisResultEvent` the pane already emits (`ui/workbench_reverse.rs:95-260`) | control_socket, shell_control, workbench_reverse | M | highest |
| 2 | open a lens by name | no `audec.lens.*` id; lenses are fixed tabs (`ui.rs:2118-2156`); the socket `lens` verb takes a numeric view | five ids `audec.lens.{waterfall,rhythm,components,separation,loom}` → `activate_or_show` | ui_actions, shell_actions | S | very high |
| 3 | ask for more/fewer components or longer gestures | frozen `ConvolutionalParams { rank: 6, template_length: 8, iterations: 60, activation_sparsity: 0.004 }` (`analysis.rs:346`); header is read-only; socket refuses recompute | `rank`/`template_length` on the Visualizer with `K−/K+`, `LAG−/LAG+` header controls and a Components `refresh` through `AnalysisProductRuntime`; persist in preferences | lens_components, lens_common, analysis, preferences, shell_control | M | very high |
| 4 | hear a component or see which seconds it owns | tiles have no click; footer refuses audition by design (`lens_components.rs:219`; `PaneAudioRoute::EvidenceOnly`) | click a row → select the component's strongest activation span and seek; ⌥-click → a soft-mask audition through the STFT, labelled "masked mixture, not a source" | lens_components, plots | S / L | very high |
| 5 | separate more than 30 s, tune the kernels | the lens silently clamps the view to 30 s (`lens_hpss.rs:80-89`) then says "selected span is current"; kernels hardcoded (`hpss.rs:32-42`) | a status line when the clamp fires; `H−/H+`, `P−/P+` for the two median widths | lens_hpss, lens_common | S / M | very high |
| 6 | go from a finding to a sound | `KeptFindingRecord { address, title, revision }` carries no span; "Make sample…" works only from the reverse pane's RESULT ACTIONS | add `span` to the record; "Hear" and "Make sample" on kept-finding rows and in the four lens headers, reusing `materialize_analysis_sample` | workspace_document, shell_explorer, lens_*, workbench_reverse | M | very high |
| 7 | reach the 2nd–Nth finding a lens published | "Open Finding · 7" always opens index 0 (`lens_rhythm.rs:549-555` and siblings) | `◂ ▸` over a `selected_finding`, or at least "Open Finding 1 of 7" | lens_*, ui.rs | S | high |
| 8 | be told a transform was refused | constant-Q refusal is stderr only and the fallback overwrites the preference (`lens_waterfall.rs:124-129`; siblings :32, :137, `lens_common.rs:57`) | route through `constructive_status`; never persist the fallback | lens_waterfall, lens_common | S | high |
| 9 | drag a range in a lens | no lens has a drag handler (only seek on mouse-down) | reuse the overview pointer kernel: drag → selection, ⌥-drag → loop | lens_common, timeline_interaction | M | high |
| 10 | click a rhythm hit / loom event | plots seek by x only; hits and events are paint-only; loom edits target "the event nearest the playhead" | hit-test the painted marks: rhythm → seek, loom → select for edit | plots, lens_rhythm, lens_loom | M | high |
| 11 | keep a loaded reading across reopen | `loaded_readings` is a bare Workbench field; no `audec.readings.v1` record | mirror `KEPT_FINDINGS_EXTENSION`: path + manifest digest, re-verified on open | workspace_document, workbench_reading, shell_project | M | high |
| 12 | ask "what is in this span and unexplained" | `Within` compiles but no button builds it; `NotExplainedByComparison` unreachable from the builder | `+ WITHIN SELECTION` and `+ NOT EXPLAINED BY THIS COMPARISON` | reading_query_view | S | high |
| 13 | change the 1,200 × 216 atlas | constants govern the whole evidence chain; a column is ~310 ms on a 6-minute song, so no component gesture shorter than ~2.5 s exists | make it true of the view: stream the field over the visible range at the same 1,200 columns, so zoom buys resolution | lens_waterfall, spectral_tiles | L | high |
| 14 | tune the onset detector | `RhythmConfig::default()` is the only config in production; 5 family rows max, no "show more" | `SENS−/+` (MAD multiplier) and a BPM-range cycle, with `refresh` | lens_rhythm, lens_common, rhythm | M | medium-high |
| 15 | change Loom's lookbehind / template length | 60 s constant; templates ≤ 8 ms + 240 ms, so a chord stab or a phrase cannot be a template | `WINDOW −/+` (30/60/120 s), `LEN −/+` (120/240/500/1000 ms) | lens_loom, loom, ui.rs | M | medium-high |
| 16 | drag an Explorer object into the arrangement | no `on_drag` in the Explorer; rename/delete offer nothing | Findings/Samples rows as drag sources with the existing payloads; say rename/delete are refused for evidence | shell_explorer, ui_drag | M | medium |
| 17 | cancel a long explanation plan | Plan/Execute are synchronous; the cancellation exists only between two statements | background-spawn them (they take a `RenderCancellation`) or remove the button and say so | workbench_reading | M | medium |
| 18 | get every refusal in words | six silent `return;`s (`lens_components.rs:30,40`; `lens_rhythm.rs:286`; `lens_loom.rs:44-51` — "Make Pattern" does nothing; `lens_waterfall.rs:20,72`; `lens_common.rs:151`) | each sets `constructive_status` | lens_* | S | medium |
| 19 | persist analysis choices | `Preferences` carries only `spectrum` | a field per knob as #3/#5/#14/#15 land | preferences | S each | medium |
| 20 | know and change constant-Q's bins per octave | `24` twice and the header hardcodes the string | one `CQT_BINS_PER_OCTAVE`; FFT± steps it in CQT mode (12/24/36) | analysis, spectral_tiles, lens_common, lens_waterfall | S | medium |

## The reverse flow, traced

It starts in a lens header ("Open Findings" / "Keep finding") and needs a
published finding, not a selection. Reveal opens a Reverse Surface pane
whose RESULT ACTIONS are Keep / Apply / Compare / Make sample
(`reverse_surface_view.rs:776-845`). Compare is `Available` only for a
current deprojection candidate with non-zero comparison and explanation
ids (`analysis_result_lifecycle.rs:226-234, 744-753`); HPSS and Loom
evidence bind `comparison: None` (`NoComparisonPlan`), Components refuses
`NoPhaseBearingPcm`. An Explanation workbench pane exists only as the
result of revealing an Explanation/Comparison object. The one verb in row
1 makes Compare reachable without a pane.

## Inventory for gates

Action ids touching this half: `audec.editor.reading_query`,
`audec.transport.audition_diff`, the three `audec.sample.*` (consume a
selection a lens set), the workspace pane verbs. Absent: any lens,
finding, comparison, explanation, reading or deprojection id. Socket
verbs: `ping status actions action open seek select click drag loop play
pause stop export objects reading_import reading_export lens quit`;
`lens` controls: `spectral-transform`, `fft-size-up/down`, `fft-window`,
`db-range-up/down` (waterfall only), `refresh` (components refuses).
`status.lenses` carries no span, no state, no finding count: three
S-sized fields that make every scenario above assertable.
