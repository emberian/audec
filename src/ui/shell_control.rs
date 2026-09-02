//! Host half of the external control protocol.
//!
//! `control_socket` owns the listener thread and the mailbox; this module
//! answers each request on the GPUI main thread through the same authorities
//! the palette and menus use. Actions go through `invoke_action_id` with the
//! `ExternalProtocol` origin so registry gating applies unchanged; structured
//! verbs lower to the exact Workbench entry points the pointer gestures use.

use super::*;
use crate::control_socket::{
    error_reply, ok_reply, ControlMailbox, ControlRequest, LoopRequest, SampleSpan, SeekTarget,
};
use crate::timeline::{
    LoopEditPolicy, LoopState, TimelineInteractionEvent, TimelinePoint, TimelineRange,
};
use serde_json::{json, Value};

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
            ControlRequest::Action { id } => {
                let Some(descriptor) = self.action_registry.get_str(&id) else {
                    return error_reply(format!("unknown action `{id}`"));
                };
                let action = descriptor.id;
                let before = self.control_notice(cx);
                self.invoke_action_id(action, InvocationOrigin::ExternalProtocol, window, cx);
                let after = self.control_notice(cx);
                ok_reply(json!({
                    "dispatched": action.as_str(),
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
            ControlRequest::Export { path } => {
                self.workbench.update(cx, |workbench, cx| {
                    workbench.start_export_to(path.clone(), cx)
                });
                ok_reply(json!({ "exporting": path.display().to_string() }))
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
                    "refresh" => {
                        match lens.kind {
                            VizKind::Waterfall => lens.rerun_spectrum(cx),
                            VizKind::Rhythm => lens.refresh_rhythm(cx),
                            VizKind::Separation => lens.refresh_hpss(cx),
                            VizKind::Loom => lens.refresh_loom(cx),
                            VizKind::Components => {
                                return Err("components analysis is owned by the workbench; reopen the material to recompute it".to_string());
                            }
                        }
                        Ok(())
                    }
                    other => Err(format!("unknown lens control `{other}`")),
                });
                match outcome {
                    Ok(()) => ok_reply(lens_json(&lens, cx)),
                    Err(message) => error_reply(message),
                }
            }
            ControlRequest::Quit => {
                cx.quit();
                ok_reply(json!("quitting"))
            }
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
    fn analysis_lens(&self, view: WorkspaceViewId, cx: &App) -> Option<Entity<Visualizer>> {
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

    fn lenses_json(&self, cx: &App) -> Value {
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
                let mut value = lens_json(&lens, cx);
                value["view"] = json!(view.0);
                Some(value)
            })
            .collect();
        Value::Array(lenses)
    }

    fn control_notice(&self, cx: &App) -> Option<String> {
        self.workbench.read(cx).constructive_status.clone()
    }

    fn control_status(&self, cx: &App) -> Value {
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
            "revision": revisions.map(|revisions| revisions.aggregate),
            "dirty": session.is_dirty().ok(),
            "io": workbench.project_io_status.label(),
            "notice": workbench.constructive_status,
            "audio_error": workbench.audio_error,
            "audio_device": workbench.audio_device_status,
            "windows": cx.windows().len(),
            "active_view": self.action_projection.active_view.map(|view| view.0),
            "lenses": self.lenses_json(cx),
        })
    }
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

fn lens_json(lens: &Entity<Visualizer>, cx: &App) -> Value {
    let lens = lens.read(cx);
    json!({
        "kind": format!("{:?}", lens.kind),
        "transform": lens.spectrum_settings.transform.label(),
        "fft_size": lens.spectrum_settings.fft_size,
        "window": lens.spectrum_settings.window.label(),
        "db_range": lens.spectrum_settings.db_range,
        "transforming": lens.spectrum_transforming,
    })
}
