# audec campaign plan: expose, be honest, subtract

Written 2026-09-03 from three audits (`UX_EXPOSURE_AUDIT.md`,
`ARCHITECTURE_RESIDUE.md`, `INTEGRATION_LEDGER.md`) and the working
agreements learned on 2026-09-01/02. Three cycles plus a stabilization pass.
Fable designs, sequences, and audits; Opus deputies implement lanes. Every
lane ends with a live check on the real binary, not only headless tests.

## Goals, in priority order

1. **Trust**: no control claims an effect it did not have; no refusal is
   silent; readouts that say "truth" are live.
2. **Reach**: what the code can do, a musician can reach (export options and
   stems, track controls, automation beyond mixer knobs, the residual guide,
   drop targets, the whole Sample menu).
3. **Subtraction**: rival authorities, hand-drained mailboxes, per-consumer
   staleness counters, compatibility halves, and unfalsifiable migrations
   go; what remains is one authority per concept.

## Working agreements (non-negotiable for every lane)

- **One writer per file per cycle.** Lanes list the files they own; a lane
  that needs a file outside its list stops and reports instead of editing.
- **Gate = headless + live.** The lane's tests, plus a socket-driven run of
  the real binary that exercises the change (`scripts/live/`, new scenarios
  welcome). Audible claims are verified by export and comparison.
- **Ground truth in the briefing.** Real signatures, absolute paths, the
  exact live seam, and the audit row the lane resolves. No prose-only specs.
- **Adversarial audit before merge.** A different agent reads the diff
  against the audit row, runs the gate, and reports; green plus self-report
  is not verification.
- **No lint-driven deletion.** Deletions come from the audits' verdicts,
  one deliberate commit each, with the reason in the message.
- **Commits name files** (`git add <files>`, never `-A`), messages via
  `git commit -F`, unsigned is fine when unattended.
- **Harvest, don't trust.** Workflow runs are read with `cv workflow`; the
  lane's own summary is a claim to check.

## Dependencies that fix the order

- The **Workbench event channel** (R2) changes how every view reports to
  the shell; the arrangement/sampler "request sent → silent failure" family
  and pane teardown ride on it. Do it before the receipt plumbing, after the
  cheap wording fixes.
- The **reveal identity collapse** (R1) unblocks "REVEAL ↗", "Keep finding",
  `reverse_navigation`, and the residual guide. Do it right after R2, tree
  quiet in its files.
- The **control_views compatibility deletion** (R5) makes every mixer and
  automation change cheaper; it goes first, and the mixer/automation
  honesty items travel with it.
- Small subtractions (migrations, codec generic, test-only traits, preview
  resolver, policy enums, duplicate enums, barrels) touch files nobody else
  needs in cycle 1; they run in parallel now.
- Export, track controls, and the catalog cleanup are independent of the
  collapses; they also run in cycle 1.

## Cycle 1: subtract and be honest (six lanes, low conflict)

| lane | scope (audit rows) | owns | gate | owner |
|---|---|---|---|---|
| C1-Catalog | register or delete the 12 orphan ids; delete `PRODUCT_MENU_LAYOUT` and the `platform_semantics` menu half; put the Sample menu in the live menu; fix `cmd-e` vs Split Clip; Next/Previous Pane mean panes; disabled palette/context rows are inert | `src/ui_actions.rs`, menu fns in `src/ui.rs` (`projected_app_menus`, `audec_keymap`, `audec_action_registry`), `src/ui/shell_actions.rs` palette/context rendering | socket `actions` lists every menu id; each dispatches or is gone; `ui::` tests | Opus |
| C1-Controls | delete `ControlIntegrationMode` compatibility half, both backends, `with_backend`/`demo`/`from_shared_graph*`; mixer Undo says and does what it does (project undo, labelled) ; `+ insert`/`+ send`/`OUTPUT` report honestly ("plugin hosting not connected in this build"; no "sent" without a receipt); insert active/bypassed labelled "not rendered" until plugins are in the render path; write-mode button installed or removed; `+ Channel` adopts the bus only on receipt | `src/control_views.rs`, `src/control_actions.rs`, mixer/automation arms of `src/ui/workbench_editors.rs` and `src/ui/workbench_publication.rs` | `control_views::` and `control_actions::` tests; live: open Mixer and Automation by action, no panic, status strings match the new rules | Fable target shape → Opus |
| C1-Arrangement | receipts in requested tense until the controller confirms, refusal shown; PROJECT TRUTH reads live revision/dirty; track header M/S/rename/delete/reorder/lock as real controls on `PutTrack`; ↶/↷ honest | `src/arrangement_view.rs`, `src/arrangement_actions.rs`, arrangement build in `src/ui/workbench_publication.rs` | `arrangement_view::`/`arrangement_actions::` tests; live: track mute via the new action, export diff shows silence | Opus |
| C1-Export | export dialog gains bit depth, dither, gain, range (loop/selection/project), scope (master/bus/track); socket `export` accepts the same | `src/ui/workbench_project_io.rs` export path, `src/export.rs`, new `src/ui/export_options.rs`, `src/control_socket.rs` export shape, `src/ui/shell_control.rs` export arm | live: export a loop at 16-bit from bus 3; sox reports bits and duration; master vs bus differ | Opus |
| C1-Subtract | R6 migrations, R7 codec generic, R8 test-only traits, R9 preview resolver, R10 policy enums, R12 `PatternEditorMode`, R13 allow barrels, R16 dead journal fns | `src/project_io.rs`, `src/daw_project.rs`, `src/project_repository.rs`, `src/project_session_lifecycle.rs`, `src/file_actions.rs`, `src/midi_input.rs`, `src/workspace_native_authority.rs`, `src/workspace_accessibility.rs`, `src/rhythm_explanation.rs`, `src/cpal_device_backend.rs`, `src/constructive.rs` (policy only), `src/sample_material.rs`, `src/render_service.rs`, `src/app_controller.rs`, `src/workspace_items.rs`, `src/pattern_actions.rs`, `src/workspace_document.rs`, `src/command_journal.rs`, `src/project_session.rs` re-exports | full suite; live: open material, make beat, save, reopen, beat still audible | Opus (two lanes by file cluster if needed) |
| C1-Sampler | zone model gains loop mode and envelope; `SetLoop`/envelope edits become commands the renderer honours; TRIM error not overwritten; `emit` failure shown; REVEAL ↗ and KIT ‹ › either work or are removed | `src/sample_kit.rs`, zone arms of `src/constructive_controller.rs`, `src/sampler_view.rs`, `src/sampler_pane.rs`, sample voice rendering (`src/instruments.rs` or where zones are voiced) | flow test; live: make beat, set a zone to loop, export diff shows the looped tail | Fable zone model → Opus |

Fable during cycle 1: writes the C1-Controls target shape and the C1-Sampler
zone model before those lanes start; designs R2 and R1 for cycle 2; audits
every lane's diff against its audit rows.

## Cycle 2: collapse the shell (sequential, high conflict)

| lane | scope | owns | gate | owner |
|---|---|---|---|---|
| C2-Channel | R2 one `WorkbenchEvent` channel (the `Pending*` structs become variants), R3 `Fresh<T>` per authority, R4 sample pipeline collapse; teardown drops the receiver | `src/ui.rs`, `src/ui/workbench_lifecycle.rs`, `src/ui/workbench_events.rs`, `src/pane_audio.rs`, `src/sample_actions.rs`, every view factory's callback site | full suite; all three live scenarios; a new scenario that closes a pane mid-audition and shows nothing keeps playing | Fable design → Opus, tree quiet in these files |
| C2-Reveal | R1 `ObjectRef` + `RevealRequest` + `RevealReceipt`; surfaces implement `locate`; both `EditorTarget`s merge; `reverse_navigation` wired; REVEAL ↗ works; "Keep finding" durable | `src/object_navigation.rs`, `src/explorer_model.rs`, `src/explanation_workbench_view.rs`, `src/reading_query_workbench.rs`, `src/reading_effect_bridge.rs`, `src/product_input.rs`, `src/project_selection.rs`, `src/deprojection_workspace_bridge.rs`, `src/workspace_items.rs`, `src/workspace_document.rs`, `src/reverse_navigation.rs`, `src/reverse_surface_adapter.rs`, `src/ui/helpers.rs`, `src/ui/workbench_reading.rs` | full suite; live: keep a finding, quit, reopen, it is there; reveal from an explanation lands or names its refusal | Fable design → Opus, after C2-Channel |
| C2-Automation | non-mixer descriptor sources (clip, residual, component, lens), lane algebra actions, write-mode recording | `src/automation.rs`, `src/control_actions.rs` automation arms, `src/control_views.rs` automation half | tests; live: create a clip-gain lane by action, export diff shows the fade | Opus, after C1-Controls |
| C2-Loom | Loom sketch edits become project commands ("edit existing construction"); Template audition honours gain/mute; Make Pattern opens the new pattern | `src/ui/lens_loom.rs`, loom construction arms of `src/constructive_controller.rs`, `src/loom_*.rs` | flow test; live: nudge an event, undo removes it | Fable design of the edit command → Opus |

## Cycle 3: reach

| lane | scope | owner |
|---|---|---|
| C3-Compare | reverse Compare as a real A/B null test (audible diff between cohorts); coverage artifacts published; residual guide exposed in the reading query pane | Fable design → Opus |
| C3-Readings | portable reading export and semantic diff exposed; a host import path | Opus |
| C3-Time | tempo map and time signature editing; track→bus rerouting; mixer-bus and pattern-library drop targets | Opus |
| C3-Ranges | R11 one half-open sample range; R14 narration rewritten as invariants in touched files; R15 constant provenance fields | Opus |
| C3-Plugins | feasibility of plugin inserts in the render path (the only way "+ insert" and "active" can stop lying) | Fable feasibility, then decide |

## Stabilization

Full suite, all live scenarios, the make-beat export comparison, and a human
pass with screenshots (needs Screen Recording permission for the terminal).
`docs/STATE.md` is rewritten from what the scripts and the human saw.

## Lane briefing template

1. The audit rows this lane resolves, verbatim, with file:line.
2. Owned files (absolute paths). Files it may read but not edit.
3. The live seam: the exact function that will call the new code.
4. Real signatures pasted from the tree for every type the lane touches.
5. The gate: test filters (mounted module paths, check the `running N`
   line) and the live scenario, including the expected socket output.
6. Box safety: never a bare `-p audec` suite, never `git add -A`, never
   stash; on hbox, `swarm-build`.
7. Report format: what changed, what the gate showed (paste the lines),
   what was left out and why.
