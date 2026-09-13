//! Host half of the external control protocol.
//!
//! `control_socket` owns the listener thread and the mailbox; this module
//! answers each request on the GPUI main thread through the same authorities
//! the palette and menus use. Actions go through `invoke_action_id` with the
//! `ExternalProtocol` origin so registry gating applies unchanged; structured
//! verbs lower to the exact Workbench entry points the pointer gestures use.

use super::*;
use crate::control_socket::{
    error_reply, ok_reply, ControlMailbox, ControlRequest, FindingAction, FindingTarget,
    LoopRequest, SampleSpan, SeekTarget,
};
use crate::timeline::{
    LoopEditPolicy, LoopState, TimelineInteractionEvent, TimelinePoint, TimelineRange,
};
use serde_json::{json, Value};

use super::workbench_project_io::parse_manifest_digest;

/// Poll the mailbox on the main thread and answer every pending request.
/// The task ends when the window is gone.
pub fn install_control_poller(
    handle: WindowHandle<DawWorkspace>,
    mailbox: ControlMailbox,
    cx: &mut App,
) {
    cx.spawn(async move |cx| loop {
        cx.background_executor()
            .timer(Duration::from_millis(33))
            .await;
        let pending = mailbox.drain();
        if pending.is_empty() {
            continue;
        }
        let delivered = cx.update(|cx| {
            handle.update(cx, |workspace, window, cx| {
                for request in pending {
                    let reply =
                        workspace.handle_control_request(request.request.clone(), window, cx);
                    request.reply(reply);
                }
            })
        });
        if delivered.is_err() {
            break;
        }
    })
    .detach();
}

impl DawWorkspace {
    pub(super) fn handle_control_request(
        &mut self,
        request: ControlRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> String {
        // A scripted window is rarely painted, and the product shell used to
        // install a reopened workspace and write down its records only while
        // painting. Settle first, so the socket sees the same app a musician
        // looking at the window would.
        self.settle_shell(cx);
        match request {
            ControlRequest::Ping => ok_reply(json!("pong")),
            ControlRequest::Status => ok_reply(self.control_status(cx)),
            ControlRequest::Actions => {
                self.refresh_action_projection(cx);
                let entries = self
                    .action_projection
                    .entries()
                    .map(|entry| {
                        json!({
                            "id": entry.descriptor.id.as_str(),
                            "label": entry.descriptor.label,
                            "enabled": entry.state.enabled,
                            "checked": entry.state.checked,
                            "disabled_reason": entry.state.disabled_reason,
                        })
                    })
                    .collect::<Vec<_>>();
                ok_reply(Value::Array(entries))
            }
            ControlRequest::Action { id, parameters } => {
                let Some(descriptor) = self.action_registry.get_str(&id) else {
                    return error_reply(format!("unknown action `{id}`"));
                };
                let action = descriptor.id;
                let named = parameters
                    .iter()
                    .map(|(name, value)| (name.to_owned(), json!(format!("{value:?}"))))
                    .collect::<serde_json::Map<_, _>>();
                let before = self.control_notice(cx);
                self.invoke_action_with_parameters(
                    action,
                    InvocationOrigin::ExternalProtocol,
                    parameters,
                    window,
                    cx,
                );
                let after = self.control_notice(cx);
                ok_reply(json!({
                    "dispatched": action.as_str(),
                    "parameters": Value::Object(named),
                    "notice": if after != before { after } else { None },
                }))
            }
            ControlRequest::Open { path } => {
                if !path.exists() {
                    return error_reply(format!("no such file: {}", path.display()));
                }
                self.workbench
                    .update(cx, |workbench, cx| workbench.load_path(path.clone(), cx));
                ok_reply(json!({ "loading": path.display().to_string() }))
            }
            ControlRequest::Seek(target) => {
                self.workbench.update(cx, |workbench, cx| match target {
                    SeekTarget::Sample(sample) => workbench.seek_to_sample(sample, cx),
                    SeekTarget::Seconds(seconds) => workbench.seek_to(seconds, cx),
                });
                ok_reply(self.control_status(cx))
            }
            ControlRequest::Click { sample } => {
                let at = TimelinePoint(sample);
                self.workbench.update(cx, |workbench, cx| {
                    workbench.dispatch_timeline_event(
                        TimelineInteractionEvent::PointerDown {
                            at,
                            loop_policy: LoopEditPolicy::for_range_gesture(false),
                        },
                        cx,
                    );
                    workbench
                        .dispatch_timeline_event(TimelineInteractionEvent::PointerUp { at }, cx);
                });
                ok_reply(self.control_status(cx))
            }
            ControlRequest::Drag { start, end, alt } => {
                self.workbench.update(cx, |workbench, cx| {
                    workbench.dispatch_timeline_event(
                        TimelineInteractionEvent::PointerDown {
                            at: TimelinePoint(start),
                            loop_policy: LoopEditPolicy::for_range_gesture(alt),
                        },
                        cx,
                    );
                    workbench.dispatch_timeline_event(
                        TimelineInteractionEvent::PointerMove {
                            at: TimelinePoint(end),
                        },
                        cx,
                    );
                    workbench.dispatch_timeline_event(
                        TimelineInteractionEvent::PointerUp {
                            at: TimelinePoint(end),
                        },
                        cx,
                    );
                });
                ok_reply(self.control_status(cx))
            }
            ControlRequest::Select(span) => {
                let range = match span {
                    None => None,
                    Some(span) => match timeline_range(span) {
                        Some(range) => Some(range),
                        None => return error_reply("empty selection"),
                    },
                };
                self.dispatch_control_timeline_event(
                    TimelineInteractionEvent::ReplaceSelection(range),
                    cx,
                )
            }
            ControlRequest::Loop(request) => {
                let event = match request {
                    LoopRequest::Clear => TimelineInteractionEvent::ClearLoop,
                    LoopRequest::Replace { span, enabled } => match timeline_range(span) {
                        Some(range) => TimelineInteractionEvent::ReplaceLoop(if enabled {
                            LoopState::active(range)
                        } else {
                            LoopState::disabled(Some(range))
                        }),
                        None => return error_reply("empty loop"),
                    },
                };
                self.dispatch_control_timeline_event(event, cx)
            }
            ControlRequest::Play => {
                self.dispatch_control_timeline_event(TimelineInteractionEvent::PlayRequested, cx)
            }
            ControlRequest::Pause => {
                self.dispatch_control_timeline_event(TimelineInteractionEvent::PauseRequested, cx)
            }
            ControlRequest::Stop => {
                self.dispatch_control_timeline_event(TimelineInteractionEvent::StopRequested, cx)
            }
            ControlRequest::Export { path, options } => {
                let resolved = self
                    .workbench
                    .read(cx)
                    .resolve_export_options(&options, cx)
                    .map(|options| {
                        let summary = self.workbench.read(cx).export_summary(&options, cx);
                        (options, summary)
                    });
                let (options, summary) = match resolved {
                    Ok(resolved) => resolved,
                    Err(message) => return error_reply(message),
                };
                self.workbench.update(cx, |workbench, cx| {
                    workbench.start_export_with(path.clone(), options, cx)
                });
                ok_reply(json!({
                    "exporting": path.display().to_string(),
                    "settings": summary,
                }))
            }
            ControlRequest::Finding { target, action } => self.control_finding(target, action, cx),
            ControlRequest::Tempo { bpm } => {
                self.workbench
                    .update(cx, |workbench, cx| workbench.set_project_tempo(bpm, cx));
                let status = self.workbench.read(cx).constructive_status.clone();
                let musical = self.workbench.read(cx).playhead_musical_time(cx);
                ok_reply(json!({
                    "bpm": musical.map(|time| time.bpm),
                    "notice": status,
                }))
            }
            ControlRequest::Objects => {
                self.refresh_product_shell(cx);
                let Some(model) = self.explorer_model.as_ref() else {
                    return error_reply("no project explorer yet");
                };
                let modes = ExplorerMode::ALL
                    .iter()
                    .map(|mode| explorer_node_json(model.root(*mode)))
                    .collect::<Vec<_>>();
                ok_reply(Value::Array(modes))
            }
            ControlRequest::ReadingImport {
                path,
                manifest_digest,
            } => {
                let expected = match manifest_digest.as_deref().map(parse_manifest_digest) {
                    Some(Ok(digest)) => Some(digest),
                    Some(Err(error)) => return error_reply(error),
                    None => None,
                };
                let loaded = self.workbench.update(cx, |workbench, cx| {
                    workbench.load_reading_file(path.clone(), expected, cx)
                });
                match loaded {
                    Ok(receipt) => ok_reply(json!({
                        "reading_id": receipt.reading_id,
                        "revision": receipt.revision,
                        "manifest_digest": receipt.manifest_digest,
                        "verification": format!("{:?}", receipt.verification),
                        "qualified_entities": receipt.entities,
                        "loaded_readings": receipt.loaded,
                        "replaced": receipt.replaced,
                        "notice": self.control_notice(cx),
                    })),
                    Err(error) => error_reply(error),
                }
            }
            ControlRequest::ReadingExport { path } => {
                let exported = self.workbench.update(cx, |workbench, cx| {
                    workbench.write_project_reading(path.clone(), cx)
                });
                match exported {
                    Ok(receipt) => ok_reply(json!({
                        "path": receipt.path.display().to_string(),
                        "reading_id": receipt.reading_id,
                        "revision": receipt.revision,
                        "manifest_digest": receipt.manifest_digest,
                        "hypotheses": receipt.entities,
                        "retained_foreign": receipt.retained_foreign,
                        "notice": self.control_notice(cx),
                    })),
                    Err(error) => error_reply(error),
                }
            }
            ControlRequest::Save { path } => {
                // The workspace document is the shell's, so the shell hands it
                // to the save exactly as the File menu does; nothing about the
                // save path is special-cased for scripting.
                self.save_project_package(path.clone(), cx);
                ok_reply(json!({ "saving": path.display().to_string() }))
            }
            ControlRequest::Lens { view, control } => {
                let Some(lens) = self.analysis_lens(WorkspaceViewId(view), cx) else {
                    return error_reply(format!("view {view} is not an analysis lens"));
                };
                let outcome = lens.update(cx, |lens, cx| match control.as_str() {
                    "spectral-transform" | "fft-size-up" | "fft-size-down" | "fft-window"
                    | "db-range-up" | "db-range-down"
                        if lens.kind != VizKind::Waterfall =>
                    {
                        Err(format!(
                            "`{control}` is a waterfall control; this lens is {:?}",
                            lens.kind
                        ))
                    }
                    "components-rank-up"
                    | "components-rank-down"
                    | "components-lag-up"
                    | "components-lag-down"
                    | "components-finding-next"
                    | "components-finding-previous"
                        if lens.kind != VizKind::Components =>
                    {
                        Err(format!(
                            "`{control}` is a components control; this lens is {:?}",
                            lens.kind
                        ))
                    }
                    "spectral-transform" => {
                        lens.cycle_transform(cx);
                        Ok(())
                    }
                    "fft-size-up" => {
                        lens.change_fft_size(1, cx);
                        Ok(())
                    }
                    "fft-size-down" => {
                        lens.change_fft_size(-1, cx);
                        Ok(())
                    }
                    "fft-window" => {
                        lens.cycle_window_function(cx);
                        Ok(())
                    }
                    "db-range-up" => {
                        lens.adjust_db_range(6.0, cx);
                        Ok(())
                    }
                    "db-range-down" => {
                        lens.adjust_db_range(-6.0, cx);
                        Ok(())
                    }
                    "components-rank-up" => lens.rank_control(1, cx),
                    "components-rank-down" => lens.rank_control(-1, cx),
                    "components-lag-up" => lens.template_length_control(1, cx),
                    "components-lag-down" => lens.template_length_control(-1, cx),
                    "components-finding-next" => lens.step_component_finding(1, cx),
                    "components-finding-previous" => lens.step_component_finding(-1, cx),
                    // `component-span:3` is what clicking the third component
                    // row does: select the seconds that component owns.
                    other if other.starts_with("component-span:") => {
                        if lens.kind != VizKind::Components {
                            return Err(format!(
                                "`{other}` is a components control; this lens is {:?}",
                                lens.kind
                            ));
                        }
                        let ordinal = other
                            .trim_start_matches("component-span:")
                            .parse::<usize>()
                            .map_err(|_| {
                                format!(
                                    "`{other}` needs a component number, as in `component-span:1`"
                                )
                            })?;
                        if ordinal == 0 {
                            return Err(
                                "components are numbered from 1, as they are drawn".to_string()
                            );
                        }
                        lens.select_component_span(ordinal - 1, cx)
                    }
                    "rhythm-sens-up" | "rhythm-sens-down" | "rhythm-bpm-range"
                        if lens.kind != VizKind::Rhythm =>
                    {
                        Err(format!(
                            "`{control}` is a rhythm control; this lens is {:?}",
                            lens.kind
                        ))
                    }
                    "rhythm-sens-up" => {
                        lens.step_rhythm_sensitivity(1, cx);
                        Ok(())
                    }
                    "rhythm-sens-down" => {
                        lens.step_rhythm_sensitivity(-1, cx);
                        Ok(())
                    }
                    "rhythm-bpm-range" => {
                        lens.cycle_rhythm_tempo_window(cx);
                        Ok(())
                    }
                    "loom-window-up" | "loom-window-down" | "loom-len-up" | "loom-len-down"
                        if lens.kind != VizKind::Loom =>
                    {
                        Err(format!(
                            "`{control}` is a Loom control; this lens is {:?}",
                            lens.kind
                        ))
                    }
                    "loom-window-up" => {
                        lens.step_loom_window(1, cx);
                        Ok(())
                    }
                    "loom-window-down" => {
                        lens.step_loom_window(-1, cx);
                        Ok(())
                    }
                    "loom-len-up" => {
                        lens.step_loom_template_length(1, cx);
                        Ok(())
                    }
                    "loom-len-down" => {
                        lens.step_loom_template_length(-1, cx);
                        Ok(())
                    }
                    "loom-cluster-next" | "loom-cluster-prev" | "loom-event-toggle"
                        if lens.kind != VizKind::Loom =>
                    {
                        Err(format!(
                            "`{control}` is a Loom control; this lens is {:?}",
                            lens.kind
                        ))
                    }
                    "loom-cluster-next" => {
                        lens.cycle_loom_cluster(1, cx);
                        Ok(())
                    }
                    "loom-cluster-prev" => {
                        lens.cycle_loom_cluster(-1, cx);
                        Ok(())
                    }
                    // The pane's edit row was mouse-only, so which event an
                    // edit lands on could not be read back by a script at all.
                    "loom-event-toggle" => {
                        lens.edit_nearest_loom_event(0.0, 0.0, true, cx);
                        Ok(())
                    }
                    "finding-next" | "finding-prev" => {
                        let count = match lens.kind {
                            VizKind::Rhythm => lens.rhythm_finding_count(),
                            VizKind::Loom => lens.loom_finding_count(),
                            other => {
                                return Err(format!(
                                    "`{control}` moves a lens's own Finding selection; the {other:?} lens publishes none"
                                ))
                            }
                        };
                        lens.step_selected_finding(
                            if control == "finding-prev" { -1 } else { 1 },
                            count,
                            cx,
                        );
                        Ok(())
                    }
                    // A press inside a lens's plot, in fractions of the plot
                    // rather than in pixels: the socket's `click` says which
                    // sample on the overview timeline, and this says which
                    // point of a lens's own plot. It goes through the same
                    // hit test the mouse does.
                    other if other.starts_with("rhythm-press:")
                        || other.starts_with("loom-press:") =>
                    {
                        let (name, argument) = other.split_once(':').expect("checked above");
                        let kind = if name == "rhythm-press" {
                            VizKind::Rhythm
                        } else {
                            VizKind::Loom
                        };
                        if lens.kind != kind {
                            return Err(format!(
                                "`{name}` presses the {kind:?} plot; this lens is {:?}",
                                lens.kind
                            ));
                        }
                        let position = match plot_press_position(lens, argument) {
                            Ok(position) => position,
                            Err(message) => return Err(message),
                        };
                        match kind {
                            VizKind::Rhythm => lens.press_rhythm_plot(position, cx),
                            _ => lens.press_loom_plot(position, cx),
                        }
                        Ok(())
                    }
                    "refresh" => {
                        match lens.kind {
                            VizKind::Waterfall => lens.rerun_spectrum(cx),
                            VizKind::Rhythm => lens.refresh_rhythm(cx),
                            VizKind::Separation => lens.refresh_hpss(cx),
                            VizKind::Loom => lens.refresh_loom(cx),
                            // Components analysis is whole-song and lives on
                            // the workbench, but that is where it runs, not a
                            // reason a script cannot ask for it.
                            VizKind::Components => lens.refresh_components(cx)?,
                        }
                        Ok(())
                    }
                    other => Err(format!("unknown lens control `{other}`")),
                });
                match outcome {
                    Ok(()) => {
                        let findings = self.workbench.read(cx).published_analysis_results();
                        ok_reply(self.lens_json(&lens, &findings, cx))
                    }
                    Err(message) => error_reply(message),
                }
            }
            ControlRequest::Quit => {
                cx.quit();
                ok_reply(json!("quitting"))
            }
        }
    }

    /// Act on one published Finding without a pane. Resolution, the host view
    /// the request is attributed to, the lifecycle's own availability rules,
    /// and the reverse pane's event are all one road; this verb only chooses
    /// which Finding and which verb, and reports what the app said.
    fn control_finding(
        &mut self,
        target: FindingTarget,
        action: FindingAction,
        cx: &mut Context<Self>,
    ) -> String {
        let findings = self.workbench.read(cx).published_analysis_results();
        let (index, finding) = match &target {
            FindingTarget::Index(index) => match findings.get(*index) {
                Some(finding) => (*index, finding.clone()),
                None => {
                    return error_reply(format!(
                        "no finding at index {index}; status.findings lists {}",
                        findings.len()
                    ))
                }
            },
            FindingTarget::Address(address) => match findings
                .iter()
                .enumerate()
                .find(|(_, finding)| &finding.address == address)
            {
                Some((index, finding)) => (index, finding.clone()),
                None => {
                    return error_reply(format!(
                        "no published finding at address `{address}`; status.findings lists {}",
                        findings.len()
                    ))
                }
            },
        };
        let Some(host_view) = self.finding_host_view(cx) else {
            return error_reply("no workspace pane is open to host a finding action".to_string());
        };
        let reference = finding.result.finding;
        let durable = match &action {
            FindingAction::Open => None,
            FindingAction::Keep => Some(AnalysisDurableAction::KeepFinding),
            FindingAction::Compare => Some(AnalysisDurableAction::Compare),
            FindingAction::Apply => Some(AnalysisDurableAction::ApplyConstruction),
            FindingAction::Sample => Some(AnalysisDurableAction::MakeSample),
            FindingAction::Audition(_) => None,
        };
        let outcome = self
            .workbench
            .update(cx, |workbench, cx| match (&action, durable) {
                (FindingAction::Open, _) => {
                    workbench.reveal_analysis_finding(host_view, reference, cx);
                    Ok(())
                }
                (FindingAction::Audition(name), _) => {
                    let offered = finding
                        .presentation
                        .auditions
                        .iter()
                        .map(|choice| format!("{:?}", choice.kind))
                        .collect::<Vec<_>>();
                    let Some(choice) = finding
                        .presentation
                        .auditions
                        .iter()
                        .find(|choice| format!("{:?}", choice.kind).eq_ignore_ascii_case(name))
                    else {
                        return Err(if offered.is_empty() {
                            format!("this finding offers no audition; `{name}` was asked for")
                        } else {
                            format!(
                                "`{name}` is not an audition this finding offers; it offers {}",
                                offered.join(", ")
                            )
                        });
                    };
                    if let AnalysisAuditionAvailability::Refused(reason) = choice.availability {
                        return Err(reason.message().to_string());
                    }
                    workbench.begin_analysis_result_audition(host_view, reference, choice.kind, cx)
                }
                (_, Some(durable)) => {
                    workbench.begin_analysis_result_action(host_view, reference, durable, cx)
                }
                (_, None) => unreachable!("open and audition are handled above"),
            });
        if let Err(message) = outcome {
            return error_reply(message);
        }
        // Read the card back: the lifecycle is the authority on whether the
        // verb landed, and the notice is the app's own words about it.
        let refreshed = self
            .workbench
            .read(cx)
            .published_analysis_results()
            .into_iter()
            .find(|published| published.result.finding == reference);
        let sample_rate = self.control_sample_rate(cx);
        ok_reply(json!({
            "index": index,
            "address": finding.address,
            "did": match &action {
                FindingAction::Open => "open".to_string(),
                FindingAction::Keep => "keep".to_string(),
                FindingAction::Compare => "compare".to_string(),
                FindingAction::Apply => "apply".to_string(),
                FindingAction::Sample => "sample".to_string(),
                FindingAction::Audition(kind) => format!("audition:{kind}"),
            },
            "notice": self.control_notice(cx),
            "finding": refreshed
                .as_ref()
                .map(|published| finding_json(index, published, sample_rate)),
        }))
    }

    /// The workspace pane a pane-less finding action is attributed to: the
    /// reverse pane already showing this half of the app if there is one, else
    /// whichever pane is active. An audition is owned by a view, so it must be
    /// a view that exists; nothing here invents an owner.
    fn finding_host_view(&self, cx: &App) -> Option<WorkspaceViewId> {
        let workbench = self.workbench.read(cx);
        workbench
            .workspace_panes
            .iter()
            .find(|(_, runtime)| matches!(runtime, WorkspacePaneRuntime::Reverse))
            .map(|(view, _)| *view)
            .or(self.action_projection.active_view)
            .or_else(|| workbench.active_workspace_view())
            .or_else(|| workbench.workspace_panes.keys().copied().min())
    }

    fn control_sample_rate(&self, cx: &App) -> f64 {
        match &self.workbench.read(cx).state {
            ProjectState::Ready(analysis) => analysis.sample_rate.max(1) as f64,
            _ => 0.0,
        }
    }

    fn dispatch_control_timeline_event(
        &mut self,
        event: TimelineInteractionEvent,
        cx: &mut Context<Self>,
    ) -> String {
        self.workbench.update(cx, |workbench, cx| {
            workbench.dispatch_timeline_event(event, cx)
        });
        ok_reply(self.control_status(cx))
    }

    /// The analysis lens hosted under a workspace view id, legacy or dynamic.
    pub(super) fn analysis_lens(
        &self,
        view: WorkspaceViewId,
        cx: &App,
    ) -> Option<Entity<Visualizer>> {
        let workbench = self.workbench.read(cx);
        match workbench.workspace_panes.get(&view)? {
            WorkspacePaneRuntime::Analysis(lens) => lens.upgrade(),
            WorkspacePaneRuntime::Hosted(host) => match &host.upgrade()?.read(cx).content {
                WorkspacePaneContent::Analysis(lens) => Some(lens.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    /// The arrangement pane's own status line, markers, and clip selection.
    /// `notice` is the Workbench's channel and carries none of these: an
    /// arrangement refusal is written where the arrangement draws it.
    fn arrangement_json(&self, cx: &App) -> Option<Value> {
        let workbench = self.workbench.read(cx);
        let view = workbench.workspace_panes.values().find_map(|pane| {
            let WorkspacePaneRuntime::Hosted(host) = pane else {
                return None;
            };
            match &host.upgrade()?.read(cx).content {
                WorkspacePaneContent::Arrangement(view) => Some(view.clone()),
                _ => None,
            }
        })?;
        let view = view.read(cx);
        Some(json!({
            "status": view.status(),
            "selected_clips": view.selected_clips().iter().map(|clip| clip.get()).collect::<Vec<_>>(),
            "markers": view
                .markers()
                .into_iter()
                .map(|(at, name)| json!({ "sample": at.0, "name": name }))
                .collect::<Vec<_>>(),
        }))
    }

    fn lenses_json(&self, findings: &[PublishedAnalysisResult], cx: &App) -> Value {
        let views: Vec<WorkspaceViewId> = self
            .workbench
            .read(cx)
            .workspace_panes
            .keys()
            .copied()
            .collect();
        let lenses = views
            .into_iter()
            .filter_map(|view| {
                let lens = self.analysis_lens(view, cx)?;
                let mut value = self.lens_json(&lens, findings, cx);
                value["view"] = json!(view.0);
                Some(value)
            })
            .collect();
        Value::Array(lenses)
    }

    /// What one lens is: its spectrum settings, what it is doing, the window
    /// of material it is showing, and how many findings it has published.
    /// A scenario that cannot see `state` has to guess when an analysis is
    /// done, and a lens that will not say which window it read is a lens whose
    /// evidence cannot be checked against the song.
    fn lens_json(
        &self,
        lens: &Entity<Visualizer>,
        findings: &[PublishedAnalysisResult],
        cx: &App,
    ) -> Value {
        let workbench = self.workbench.read(cx);
        let lens = lens.read(cx);
        let sample_rate = workbench
            .analysis()
            .map_or(0.0, |analysis| analysis.sample_rate.max(1) as f64);
        let total = workbench.total_samples();
        let seconds_to_frame = |seconds: f64| -> u64 {
            if sample_rate <= 0.0 || !seconds.is_finite() || seconds <= 0.0 {
                0
            } else {
                (seconds * sample_rate).round() as u64
            }
        };
        let frames = |start: u64, end: u64, basis: &str| -> Value {
            json!({
                "start": start,
                "end": end,
                "start_seconds": if sample_rate > 0.0 { start as f64 / sample_rate } else { 0.0 },
                "end_seconds": if sample_rate > 0.0 { end as f64 / sample_rate } else { 0.0 },
                "basis": basis,
            })
        };
        // The viewport is the fallback window: what the lens is drawing when
        // it holds no result of its own.
        let viewport = frames(
            (lens.time_start.clamp(0.0, 1.0) * total as f64).round() as u64,
            (lens.time_end.clamp(0.0, 1.0) * total as f64).round() as u64,
            "viewport",
        );
        let (state, failure, span) = match lens.kind {
            VizKind::Waterfall => {
                let state = if lens.spectrum_transforming {
                    "Analyzing"
                } else if lens.local_spectrogram.is_some() || lens.local_spectral_db.is_some() {
                    "Ready"
                } else {
                    "Idle"
                };
                (state, None, viewport.clone())
            }
            VizKind::Components => {
                let state = if workbench.component_analysis_pending {
                    "Analyzing"
                } else if workbench
                    .analysis()
                    .is_some_and(|analysis| analysis.components.is_some())
                {
                    "Ready"
                } else {
                    "Idle"
                };
                (state, None, viewport.clone())
            }
            VizKind::Rhythm => match &lens.rhythm_state {
                RhythmViewState::Idle => ("Idle", None, viewport.clone()),
                RhythmViewState::Analyzing => ("Analyzing", None, viewport.clone()),
                RhythmViewState::Failed(error) => ("Failed", Some(error.clone()), viewport.clone()),
                RhythmViewState::Ready(result) => (
                    "Ready",
                    None,
                    frames(
                        result.source.span.start.max(0) as u64,
                        result.source.span.end.max(0) as u64,
                        "result",
                    ),
                ),
            },
            VizKind::Separation => match &lens.hpss_state {
                HpssViewState::Idle => ("Idle", None, viewport.clone()),
                HpssViewState::Analyzing {
                    start_seconds,
                    end_seconds,
                } => (
                    "Analyzing",
                    None,
                    frames(
                        seconds_to_frame(*start_seconds),
                        seconds_to_frame(*end_seconds),
                        "analyzing",
                    ),
                ),
                HpssViewState::Failed(error) => ("Failed", Some(error.clone()), viewport.clone()),
                HpssViewState::Ready(result) => (
                    "Ready",
                    None,
                    frames(result.start_frame, result.end_frame, "result"),
                ),
            },
            VizKind::Loom => match &lens.loom_state {
                LoomViewState::Idle => ("Idle", None, viewport.clone()),
                LoomViewState::Inferring {
                    start_seconds,
                    end_seconds,
                    ..
                } => (
                    "Analyzing",
                    None,
                    frames(
                        seconds_to_frame(*start_seconds),
                        seconds_to_frame(*end_seconds),
                        "analyzing",
                    ),
                ),
                LoomViewState::Failed(error) => ("Failed", Some(error.clone()), viewport.clone()),
                LoomViewState::Ready(result) => (
                    "Ready",
                    None,
                    frames(
                        result.start_sample as u64,
                        result.end_sample as u64,
                        "result",
                    ),
                ),
            },
        };
        let published = findings
            .iter()
            .filter(|finding| lens_of_result_kind(finding.result.kind) == lens.kind)
            .count();
        // Every knob this lens owns, at the value it is set to. A scenario
        // that can read a knob back is a scenario that can prove one moved.
        // What the lens's own knobs are set to, and whether the result on
        // screen was produced with them. A knob that changes evidence and a
        // result that predates it are two different facts and a script needs
        // both.
        let rhythm = lens.rhythm_settings.normalized();
        let loom = lens.loom_settings.normalized();
        // What the knobs found. A knob that changes evidence is only a claim
        // until the evidence is counted, so the counts travel with it.
        let rhythm_result = match &lens.rhythm_state {
            RhythmViewState::Ready(result) => json!({
                "hits": result.hits.len(),
                "families": result.event_families.len(),
                "patterns": result.patterns.len(),
                "tempo_hypotheses": result.tempo_hypotheses.len(),
                "rows": visible_rhythm_family_ids(
                    result,
                    (lens.time_start * result.sample_frames as f64).floor() as usize,
                    (lens.time_end * result.sample_frames as f64).ceil() as usize,
                    RHYTHM_MAX_VISIBLE_FAMILIES,
                ).len(),
            }),
            _ => Value::Null,
        };
        let loom_result = match &lens.loom_state {
            LoomViewState::Ready(result) => json!({
                "clusters": result.sketch.clusters.len(),
                "events": result.sketch.events.len(),
                "template_samples": result
                    .sketch
                    .clusters
                    .first()
                    .map(|cluster| cluster.template.samples.len()),
                "explained_energy": result.fit.explained_energy,
                "selected_cluster": result.selected_cluster,
            }),
            _ => Value::Null,
        };
        let settings = match lens.kind {
            VizKind::Waterfall => json!({
                "transform": lens.spectrum_settings.transform.label(),
                "fft_size": lens.spectrum_settings.fft_size,
                "hop_size": lens.spectrum_settings.hop_size,
                "window": lens.spectrum_settings.window.label(),
                "db_ceiling": lens.spectrum_settings.db_ceiling,
                "db_range": lens.spectrum_settings.db_range,
                "cqt_bins_per_octave": lens.spectrum_settings.cqt_bins_per_octave,
                "refused": lens.spectrum_refusal,
            }),
            VizKind::Components => json!({
                "rank": workbench.component_params.rank,
                "template_length": workbench.component_params.template_length,
                "template_seconds": workbench.component_template_seconds(),
                "iterations": workbench.component_params.iterations,
                "shown": workbench
                    .analysis()
                    .and_then(|analysis| analysis.components.as_ref())
                    .map(|components| components.components.len()),
                "selected_finding": lens.selected_finding,
            }),
            VizKind::Rhythm => json!({
                "result": rhythm_result,
                "sensitivity": rhythm.threshold_mad_multiplier,
                "tempo_window": rhythm.tempo_label(),
                "tempo_min_bpm": rhythm.tempo_range().0,
                "tempo_max_bpm": rhythm.tempo_range().1,
                "stale": lens.rhythm_result_is_stale(),
                "selected_finding": lens.selected_finding.min(
                    lens.rhythm_finding_count().saturating_sub(1)),
                "finding_count": lens.rhythm_finding_count(),
            }),
            VizKind::Loom => json!({
                "result": loom_result,
                "lookbehind_seconds": loom.lookbehind_seconds(),
                "template_milliseconds": loom.template_milliseconds(),
                "stale": lens.loom_result_is_stale(),
                "selected_finding": lens.selected_finding.min(
                    lens.loom_finding_count().saturating_sub(1)),
                "finding_count": lens.loom_finding_count(),
                "selected_event": match &lens.loom_state {
                    LoomViewState::Ready(result) => result.selected_event,
                    _ => None,
                },
            }),
            _ => Value::Null,
        };
        json!({
            "kind": format!("{:?}", lens.kind),
            "state": state,
            "failure": failure,
            "span": span,
            "findings": published,
            "settings": settings,
            "transform": lens.spectrum_settings.transform.label(),
            "fft_size": lens.spectrum_settings.fft_size,
            "window": lens.spectrum_settings.window.label(),
            "db_range": lens.spectrum_settings.db_range,
            "transforming": lens.spectrum_transforming,
            "settings": settings,
        })
    }

    fn control_notice(&self, cx: &App) -> Option<String> {
        self.workbench.read(cx).constructive_status.clone()
    }

    /// Everything a scenario reads back. The action projection is refreshed
    /// first: `active_view` is a projected fact, and reporting the one that
    /// was current before the action that just ran made `status` lag a verb
    /// behind the app it describes.
    fn control_status(&mut self, cx: &mut Context<Self>) -> Value {
        self.refresh_action_projection(cx);
        let findings = self.workbench.read(cx).published_analysis_results();
        let workbench = self.workbench.read(cx);
        let session = workbench.session.read(cx);
        let revisions = session.snapshot().revisions();
        let (state, material) = match &workbench.state {
            ProjectState::Empty => ("empty", Value::Null),
            ProjectState::Loading(path) => ("loading", json!(path.display().to_string())),
            ProjectState::Failed(message) => ("failed", json!(message)),
            ProjectState::Ready(analysis) => (
                "ready",
                json!({
                    "path": analysis.path.display().to_string(),
                    "title": analysis.title,
                    "sample_rate": analysis.sample_rate,
                    "channels": analysis.channels,
                    "duration_seconds": analysis.duration_seconds,
                }),
            ),
        };
        let sample_rate = match &workbench.state {
            ProjectState::Ready(analysis) => analysis.sample_rate.max(1) as f64,
            _ => 0.0,
        };
        let span_json = |range: &SampleRange| {
            json!({
                "start": range.start.0,
                "end": range.end.0,
                "start_seconds": if sample_rate > 0.0 { range.start.0 as f64 / sample_rate } else { 0.0 },
                "end_seconds": if sample_rate > 0.0 { range.end.0 as f64 / sample_rate } else { 0.0 },
            })
        };
        json!({
            "state": state,
            "material": material,
            "playing": workbench.transport_is_playing(),
            "playhead_sample": workbench.playhead_sample(),
            "playhead_seconds": workbench.playhead_seconds,
            "total_samples": workbench.total_samples(),
            "selection": workbench.timeline_selection.as_ref().map(span_json),
            "loop": workbench.loop_range.as_ref().map(|range| {
                let mut value = span_json(range);
                value["enabled"] = json!(workbench.loop_enabled);
                value
            }),
            "follow": workbench.timeline_follow,
            "musical_time": workbench.playhead_musical_time(cx).map(|time| json!({
                "bpm": time.bpm,
                "numerator": time.signature.numerator,
                "denominator": time.signature.denominator,
                "bar": time.bar,
                "bar_start_tick": time.bar_start.0,
                "segment_start_tick": time.segment_start.0,
            })),
            "revision": revisions.map(|revisions| revisions.aggregate),
            "dirty": session.is_dirty().ok(),
            "io": workbench.project_io_status.label(),
            "notice": workbench.constructive_status,
            "metronome": workbench.metronome_enabled(),
            "audio_error": workbench.audio_error,
            "audio_device": workbench.audio_device_status,
            "windows": cx.windows().len(),
            "active_view": self.action_projection.active_view.map(|view| view.0),
            "lenses": self.lenses_json(&findings, cx),
            "findings": Value::Array(
                findings
                    .iter()
                    .enumerate()
                    .map(|(index, finding)| finding_json(index, finding, sample_rate))
                    .collect(),
            ),
            "arrangement": self.arrangement_json(cx),
            "preview": preview_json(workbench),
            "diff": diff_json(workbench),
            "readiness": readiness_json(workbench),
            "memory": memory_json(workbench),
        })
    }
}

/// How much of the audible revision is the newest one.
///
/// `missing > 0` while `playing` is true is the honest picture of playback
/// before completion: the tiles that exist are the edit, the rest are still
/// the previous cohort, and `from_previous_frames` counts the frames that
/// actually came from it rather than what was hoped.
fn readiness_json(workbench: &Workbench) -> Value {
    let status = workbench.audio_controller.readiness_status();
    json!({
        "required": status.required,
        "covered": status.covered,
        "missing": status.missing,
        "priming": status.priming,
        "from_previous_frames": status.from_previous_frames,
        "currently_from_previous": status.currently_from_previous,
        "starved_frames": status.starved_frames,
    })
}

/// Resident bytes the render side is responsible for, against the budget that
/// bounds them. `over_budget` is true when every resident product is still
/// referenced by a cohort: the ceiling is reported, never enforced by
/// discarding audio someone is playing.
fn memory_json(workbench: &Workbench) -> Value {
    let status = workbench.audio_controller.memory_status();
    json!({
        "product_resident_bytes": status.product_resident_bytes,
        "product_budget_bytes": status.product_budget_bytes,
        "product_entries": status.product_entries,
        "product_evictions": status.product_evictions,
        "over_budget": status.over_budget,
        "previous_slots": status.previous_slots,
        "previous_rehydrate_bytes": status.previous_rehydrate_bytes,
        "tile_cache_receipts": status.tile_cache_receipts,
    })
}

/// New-minus-old between the active render cohort and the one it retired.
/// `rms_in_loop`/`rms_outside_loop` are the null's energy inside the auditioned
/// span and everywhere else, so a scenario can check the same numbers a
/// before/after export comparison reports. They are null until a diff has been
/// measured for the pair of cohorts that is current right now.
fn diff_json(workbench: &Workbench) -> Value {
    let status = workbench.audio_controller.diff_status();
    json!({
        "available": status.available,
        "playing": status.playing,
        "span": status.span.map(|span| json!({ "start": span.start, "end": span.end })),
        "rms_in_loop": status.rms_in_loop,
        "rms_outside_loop": status.rms_outside_loop,
    })
}

/// What the finite preview bus is doing, by owner. A closed pane that left an
/// audition running would show here as an active preview owned by a view that
/// no longer exists, which is the only way a scenario can see the difference.
fn preview_json(workbench: &Workbench) -> Value {
    let status = workbench.preview_controller.status();
    let request_json = |request: &crate::pane_audio::PreviewRequest| {
        json!({
            "owner": format!("{:?}", request.token.owner),
            "generation": request.token.generation,
            "kind": format!("{:?}", request.kind),
        })
    };
    json!({
        "active": status.active.as_ref().map(request_json),
        "desired": status.desired.as_ref().map(request_json),
        "held_pads": workbench
            .pad_preview_tickets
            .keys()
            .map(|(view, kit, pad)| json!({ "view": view.0, "kit": kit.get(), "pad": pad.get() }))
            .collect::<Vec<_>>(),
        "bus_active": workbench
            .audio
            .as_ref()
            .map(|audio| audio.preview_active()),
    })
}

fn timeline_range(span: SampleSpan) -> Option<TimelineRange> {
    TimelineRange::new(TimelinePoint(span.start), TimelinePoint(span.end))
}

fn explorer_node_json(node: &ExplorerNode) -> Value {
    let target = match &node.target {
        ExplorerTarget::Mode(mode) => json!({ "mode": format!("{mode:?}") }),
        ExplorerTarget::Category(category) => json!({ "category": format!("{category:?}") }),
        ExplorerTarget::Object(object) => json!({
            "object": format!("{object:?}"),
            "kind": format!("{:?}", object.kind()),
        }),
    };
    json!({
        "id": node.id.as_str(),
        "label": node.label,
        "detail": node.detail,
        "diagnostic": node.diagnostic.as_ref().map(|diagnostic| format!("{diagnostic:?}")),
        "target": target,
        "children": node.children.iter().map(explorer_node_json).collect::<Vec<_>>(),
    })
}

/// One published Finding as a scenario reads it: where it is in the list, its
/// stable address, what it is about, the span of material behind it, and what
/// each of its verbs would do right now — including the refusal, verbatim,
/// for the ones it will not do.
fn finding_json(index: usize, published: &PublishedAnalysisResult, sample_rate: f64) -> Value {
    let span = published.result.source.span;
    let start = span.start.max(0) as u64;
    let end = span.end.max(0) as u64;
    let actions = published
        .presentation
        .actions
        .iter()
        .map(|action| {
            let state = match &action.state {
                AnalysisPresentedActionState::Available => json!({ "state": "available" }),
                AnalysisPresentedActionState::Pending(ticket) => json!({
                    "state": "pending",
                    "generation": ticket.generation,
                }),
                AnalysisPresentedActionState::Completed {
                    primary,
                    durable_revision,
                } => json!({
                    "state": "completed",
                    "primary": primary.address(),
                    "revision": durable_revision,
                }),
                AnalysisPresentedActionState::Refused(reason) => json!({
                    "state": "refused",
                    "reason": reason.message(),
                }),
            };
            (finding_action_word(action.action).to_owned(), state)
        })
        .collect::<serde_json::Map<_, _>>();
    json!({
        "index": index,
        "address": published.address,
        "title": published.result.label,
        "kind": format!("{:?}", published.result.kind),
        "lens": format!("{:?}", lens_of_result_kind(published.result.kind)),
        "temporary": published.presentation.temporary,
        "artifact": published
            .result
            .descriptor
            .id
            .0
            .bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        "span": {
            "start": start,
            "end": end,
            "start_seconds": if sample_rate > 0.0 { start as f64 / sample_rate } else { 0.0 },
            "end_seconds": if sample_rate > 0.0 { end as f64 / sample_rate } else { 0.0 },
        },
        "actions": Value::Object(actions),
        "auditions": published
            .presentation
            .auditions
            .iter()
            .map(|choice| json!({
                "kind": format!("{:?}", choice.kind),
                "label": choice.label,
                "available": matches!(
                    choice.availability,
                    AnalysisAuditionAvailability::Available(_)
                ),
                "refusal": match choice.availability {
                    AnalysisAuditionAvailability::Refused(reason) => Some(reason.message()),
                    AnalysisAuditionAvailability::Available(_) => None,
                },
            }))
            .collect::<Vec<_>>(),
    })
}

/// The word the `finding` verb uses for one durable action. One spelling for
/// the request and the report, so a scenario can read back what it asked for.
const fn finding_action_word(action: AnalysisDurableAction) -> &'static str {
    match action {
        AnalysisDurableAction::KeepFinding => "keep",
        AnalysisDurableAction::ApplyConstruction => "apply",
        AnalysisDurableAction::Compare => "compare",
        AnalysisDurableAction::MakeSample => "sample",
    }
}

/// Which lens published a result of this kind. Findings belong to the lens
/// that made them, so `status.lenses[*].findings` and `status.findings` are
/// two readings of one list rather than two counts that can disagree.
/// Turn `"<x>,<y>"` in fractions of a lens's plot into the pixel point the
/// mouse would have pressed. The plot records where it was painted; a lens
/// that has not painted one yet cannot be pressed, and says so.
fn plot_press_position(lens: &Visualizer, argument: &str) -> Result<gpui::Point<Pixels>, String> {
    let (x, y) = argument
        .split_once(',')
        .ok_or_else(|| format!("`{argument}` is not `<x>,<y>` in fractions of the plot"))?;
    let parse = |name: &str, raw: &str| -> Result<f32, String> {
        let value: f32 = raw
            .trim()
            .parse()
            .map_err(|_| format!("{name} `{raw}` is not a number"))?;
        if !(0.0..=1.0).contains(&value) {
            return Err(format!(
                "{name} {value} is outside the plot; it is a fraction in 0..=1"
            ));
        }
        Ok(value)
    };
    let (x, y) = (parse("x", x)?, parse("y", y)?);
    let painted = lens.timeline_bounds.lock().ok().and_then(|bounds| *bounds);
    let (bounds, _) = press_geometry(painted, lens.kind);
    Ok(gpui::point(
        bounds.origin.x + bounds.size.width * x,
        bounds.origin.y + bounds.size.height * y,
    ))
}

const fn lens_of_result_kind(kind: AnalysisResultKind) -> VizKind {
    match kind {
        AnalysisResultKind::RhythmPattern | AnalysisResultKind::RhythmFamilyMedoid => {
            VizKind::Rhythm
        }
        AnalysisResultKind::HpssComponent(_) => VizKind::Separation,
        AnalysisResultKind::LoomSequence | AnalysisResultKind::LoomTemplate => VizKind::Loom,
        AnalysisResultKind::ComponentMagnitude => VizKind::Components,
    }
}
