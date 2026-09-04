# Integration ledger

Written 2026-09-02. rustc's dead-code lints flag about 2,000 items in this
crate and 22 modules the desktop app never reaches. A deletion pass showed
that once tests are honoured only ~5,000 lines are truly unreferenced: the
rest is code somebody wrote and tested and never plugged into the running
app. This ledger is the audit of that code: what each piece intends (its
tests are the specification), where it plugs in, what a musician gets, and
a verdict. Verdicts: WIRE (do it), SUPERSEDED (the live app already does
this another way; harvest what is named, delete the rest), PARK (real
capability, not now), DELETE (nothing to integrate).

## Unreachable modules

| module | what it is | tests prove it intends | live seam | musician value if wired | cost | verdict |
|---|---|---|---|---|---|---|
| `nmfd.rs` (1075) **landed 2026-09-02** | Convolutional NMF: temporally extended recurring templates | recovers two extended recurrences, deterministic per seed | `analysis.rs` `factor_analysis_components_cancellable`; live `decomposition.rs` is single-frame NMF with the same function name | the Components lens finds a whole kick gesture (attack to decay) instead of one frozen spectrum | S: kernel swap | **WIRE** |
| `beat_this_deprojection.rs` (832) + `_controller.rs` (813) | verified Beat This worker output joined into one anonymous competing rhythm hypothesis; lifecycle observe → plan → preview → accept via `ProjectSession` | evidence is promotable without naming an instrument; adapter refusal stays distinct from worker failure | `ui/lens_rhythm.rs` `adopt_rhythm_tempo`; the only caller of live-mounted `RhythmPromotionChooser` | a learned beat grid appears as one more ranked tempo hypothesis, auditionable, adoptable, undoable | M: worker binary, `ModelTaskService::poll`, chooser all exist, uncalled | **WIRE** |
| `cqt.rs` (680) **landed 2026-09-02 (lens toggle)** | multiresolution constant-Q transform | log bins beat fixed FFT bins for pitch; exact bounded mappings | none: every live transform is linear FFT; `FrequencyScale::Logarithmic` is only a display axis | bass stops smearing into one bin: a pitch-legible spectrogram (P0 in ML_MODELS) | M: needs a lens and a tile recipe | **WIRE** |
| `render_dependencies.rs` (1221) + `render_dependency_runtime.rs` (1183) | DSP-free product graph (master/bus/stem/audition/comparison), invalidation, dirty-node topological schedule, atomic cohorts | preroll propagates to consumers; audition is prioritised but never crosses a dependency; prerequisites gate jobs | `project_audio_controller.rs` `try_render_tiles` (replaces `TileRenderBatch::with_cancellation`); live is playhead-distance FIFO over a flat tile list | a bus edit re-renders only its downstream; no half-updated bus is ever audible | M | **WIRE** |
| `reverse_navigation.rs` (515) **wired 2026-09-04** | lowers findings/explanations/comparisons/readings into reveal plans with typed refusals | qualified reading entities are not collapsed; unscoped alternatives are refused | `ui/workbench_reading.rs` `reveal_from_explanation_workbench` duplicates it inline and silently refuses Artifact/Evidence | Reveal says why it cannot, instead of doing nothing | S | **WIRE** |
| `graph_device_runtime.rs` (1203) | realtime compiled-graph executor with block-atomic hot swap and a CPAL host | swap is block-atomic; loop stays exact across prefetch | `ui/workbench_publication.rs` `request_project_audio` → `audio_host::open_renderer`; live plays pre-rendered cohorts | edits audible immediately instead of after a bounce | L: realtime safety, feature-gated | **WIRE** after the render pair |
| `product_action_router.rs` (1748) | toolkit-neutral action routing with owner + generation freshness | staleness is a correlated diagnostic, not a dropped effect | `ui/shell_actions.rs` `dispatch_action_request`; live already has `ActionContextSignature` + `ContextEpoch` | background results landing in a closed pane refused with a named reason | L: rival encoding of a working seam | **SUPERSEDED** by `ui_actions`; harvest the background-completion envelope |
| `explanation_pane_model.rs` (1381) | GPUI-neutral rows, MDL fit, refusals, promote identity for explain-as-pattern | residual is auditionable; spectral excess is coverage-only | `explanation_workbench_view.rs` has refusals/channels/export pins but no rows and no MDL fit | ranked competing explanations with a fit number | M: 60% duplicate | **PARK**; harvest `MdlFitPresentation` + rows |
| `generative_lowering.rs` (1433) + `generative_ontology.rs` (1219) | typed generative terms compiled to one envelope + construction root | unsupported effects stay honest; inferred never launders into authored; units checked | none: no live path creates a synth voice, only selects one | making a pattern would create its instrument, undoably | L: needs an authoring UI | **PARK** |
| `lens.rs` (1038) | validated lens settings for five kinds | presentation changes never invalidate analysis | `settings.rs` already holds the live settings + `SettingEffect` | new parts only: trigger, channel projection, vectorscope | M | **SUPERSEDED** by `settings`; harvest `validate()` and `spectral_recipe_changed` |
| `selection_aspect_service.rs` (464) | linked selection + aspect + signal layer with echo guard | delivery idempotent; echo guard stops loops | `pane_session_binding.rs` already guards echo | switching aspect/layer as a linked op | M | **SUPERSEDED** by `pane_session_binding` |
| `app_controller.rs` (472) | app → project window → session ownership, primary/auxiliary, quit policy | primary window unique per session; quit after last window | `ui_platform.rs` opens exactly one window | arrangement on one monitor, mixer on another, same project | L: the GPUI half is unwritten | **PARK** |
| `persistence.rs` (766) | versioned record manifest | unknown records survive; newer versions fail loudly | none: replaced by `project_io` + `project_format` + `project_store` + `project_codecs` | — | — | **DELETE** |
| ~~`view.rs` + `view/`, `fifo.rs`, `window.rs`~~ removed 2026-09-02 | 2022 SDL-era view trait, ring buffer, window registry | none | not even compiled (no `mod`) | — | — | **DELETE** |

## Tested-but-unwired clusters inside live files

| cluster | unwired part | tests prove it intends | live seam | musician value | cost | verdict |
|---|---|---|---|---|---|---|
| ML worker stack (`model_supervisor`, `model_task_service`, `model_worker`, `worker_runtime`) | cache leases, cancellation, OOM/protocol failure classes; real `audec-beat-this-worker` (rten/ONNX) and fake worker bins | hung worker cancelled then killed in bounded time; ML workers kept out of realtime reservations | `ModelTaskService::new` is called only from a test; one service field + poll on `ProjectSession` | learned beat/downbeat detection on import | M | **WIRE** (with Beat This above) |
| `pane_cohesion` | one authority joining selection, transport, audition, material publication | one selection authority fans out without locating; loop adoption explicit | `DawWorkspace` holds three flat fields; teardown hand-rolled in `ui/workbench_panes.rs`; `cancel_all` at six sites | closing a pane never leaves a preview looping; extracted material lands in the arrangement | S | **WIRE** (narrow: `unregister_pane`, `publish_material_result`) |
| `settings::SettingEffect` **persistence landed 2026-09-02 (`preferences.rs`)** | presentational / cheap projection / analysis-invalidating / engine-rebuild classifier | spectrum normalisation respects Nyquist; legacy defaults preserved | no persistence path at all today | preferences survive restart; a lens colour nudge stops re-running analysis | S (+ a codec) | **WIRE**; delete the three orphan `normalized` |
| `platform_semantics` | native menus, `registered_commands`, AccessKit dispatch with parity receipts | native menu uses platform checked/disabled state; receipts prove focus parity | menus superseded by `projected_app_menus`; no AccessKit binding exists anywhere | VoiceOver could read and drive the DAW | L | menus **SUPERSEDED**; AccessKit half **WIRE** if accessibility is a goal |
| `workspace_accessibility` (dead half) | generic semantic surface projection + canvas virtualisation | projection contains only the visible window; stale projections rejected | live half is wired; automation already served by the control socket | screen-reader navigation of a large timeline | L | **PARK** unless with AccessKit |
| `coverage` presenter + tile cache | `CoverageWorkbenchPresenter`, `CoverageTileCache`, `clear_comparison` | over-gained construction lights excess instead of hiding it; span analysis partitions the field exactly | explained energy reaches the UI; the Components lens shows an unrelated NMF "% explained" next to it | cached coverage overlay while scrubbing | S | **PARK**, but reconcile the two "% explained" numbers now |
| `deprojection_expression` | explain-as-expression with residual/excess/cost pinned to the compiled program | score alignment rejects identity or cost drift | `artifact_promotion_bridge` does the promotion today | — | S | **SUPERSEDED** |
| `reconstruction` + `reconstruction_apply` | ranked deterministic proposals with anonymous families and a mandatory residual layer; atomic apply | families never become instrument labels; every proposal keeps a residual layer | producer never runs; `artifact_promotion_bridge` promotes one candidate | "three readings of this loop, each with its leftover audio kept" | L (146 items) | apply half **SUPERSEDED**; ranking half **PARK**; keep the id vocabulary |
| `reading_query_view` presentation | `ReadingQueryPresentation`, `install_residual_guide`, `accept_refusal` | notice never conflates a planned command with success | the reaction is wired, nothing installs; refusals arrive as strings | refusal reasons a musician can act on | S | **PARK**; `accept_refusal` is a small WIRE |
| `change_set` ergonomics, `control_views` compat constructors, `daw_project::journal`/`touched`, legacy migration | builder sugar, "Cycle 2" aliases, a parallel history nobody reads, a migrator with no v0 file | — | live uses the literal builder, controller constructors, `command_journal` | — | S | **DELETE** (migration: PARK until a v0 file exists) |

## Order

1. ~~`nmfd` kernel swap in the Components lens~~ landed: six eight-frame gestures, gesture tiles in the lens.
2. `reverse_navigation` into explanation reveal (S, deletes an inline duplicate).
3. `pane_cohesion` teardown and material publication (S, retires a bug class).
4. ~~a preferences codec~~ landed as `preferences.rs` (lens spectrum choices survive relaunch); `SettingEffect` classification still unused.
5. Beat This: `ModelTaskService` on the session, controller into the Rhythm lens (M).
6. `render_dependencies` pair into `try_render_tiles` (M).
7. `cqt`: lens toggle landed; the tile recipe (zoomed detail tiles) is still FFT.
8. `graph_device_runtime` (L, after 6).
9. Deletions with DELETE verdicts, in one commit, once 1 to 4 have landed.
