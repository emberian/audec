//! Make sample / slice / beat from the active span, and pane audition.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl Workbench {
    pub(super) fn active_sample_span(&self) -> Option<ResolvedSampleSpan> {
        resolve_active_sample_span(self.timeline_selection, self.loop_enabled, self.loop_range)
    }

    pub(super) fn active_sample_span_label(&self, scope: ResolvedSampleSpan) -> String {
        let range = scope.range();
        let origin = match scope.origin() {
            SampleSpanOrigin::Selection => "Selection",
            SampleSpanOrigin::Loop => "Loop ON",
        };
        let alternate = scope
            .alternate
            .is_some()
            .then_some(" · older loop preserved")
            .unwrap_or_default();
        format!(
            "{origin} · {} – {}{alternate}",
            format_time(self.seconds_for_sample(range.start.get().max(0) as u64)),
            format_time(self.seconds_for_sample(range.end.get().max(0) as u64))
        )
    }

    pub(super) fn active_sample_workflow_spec(
        &self,
        command: SampleWorkflowCommand,
        origin: SampleSpanOrigin,
    ) -> SampleWorkflowSpec {
        let source_name = self
            .analysis()
            .map(|analysis| sample_workflow_name_stem(&analysis.title))
            .unwrap_or_else(|| "Source".into());
        SampleWorkflowSpec::expected(
            command,
            origin,
            &source_name,
            SampleInstrumentDestination::New {
                name: sample_workflow_instrument_name(command, &source_name),
            },
            None,
        )
    }

    pub(super) fn publish_timeline_sample(
        &mut self,
        command: SampleWorkflowCommand,
        cx: &mut Context<Self>,
    ) {
        let Some(scope) = self.active_sample_span() else {
            self.constructive_status =
                Some("Enable a non-empty loop or select a source range first".into());
            cx.notify();
            return;
        };
        let range = scope.range();
        let origin = scope.origin();
        if let Some(analysis) = self.analysis() {
            let frames = range.len();
            if !within_interactive_sampling_limit(frames, analysis.sample_rate) {
                self.constructive_status = Some(
                    "Interactive sampling is currently limited to 30-second selections".into(),
                );
                cx.notify();
                return;
            }
        }
        let spec = self.active_sample_workflow_spec(command, origin);
        let label = match command {
            SampleWorkflowCommand::MakeSample => "Make sample",
            SampleWorkflowCommand::SliceToPads => "Slice to kit",
            SampleWorkflowCommand::MakeBeat => "Make beat",
        };
        match self.session.update(cx, |session, _| {
            session.publish_primary_sample_workflow(range, spec)
        }) {
            Ok(outcome) => {
                let revision = outcome.constructive.update.revisions().aggregate;
                let presentation = outcome.receipt.presentation();
                self.constructive_status = Some(format!(
                    "{} · {} · revision {revision}",
                    presentation.headline, presentation.detail
                ));
                let mut recommendation = recommend_sample_result(&outcome.receipt.publication);
                recommendation.request.current_view = Some(WorkspaceViewId::TRACK_OVERVIEW);
                match self.session.read(cx).issue_reveal(recommendation.request) {
                    Ok(receipt) => {
                        self.object_reveals.push(PendingObjectReveal {
                            receipt,
                            diagnostics: recommendation.diagnostics,
                            headline: presentation.headline,
                        });
                    }
                    Err(error) => {
                        self.constructive_status =
                            Some(format!("{label} · reveal unavailable · {error}"));
                    }
                }
                self.handle_session_events(cx);
            }
            Err(error) => self.constructive_status = Some(format!("{label} failed · {error}")),
        }
        cx.notify();
    }

    /// Hear the frames a finding is about.
    ///
    /// This is the ordinary transport, not a second player: the span becomes
    /// the selection and the loop through the same timeline kernel a pointer
    /// drag uses, and play is requested from it. A musician who then hits
    /// Loop or drags the overview is editing the same thing they just heard.
    pub(super) fn hear_finding_span(
        &mut self,
        finding: crate::project_controller::FindingRef,
        span: crate::aspect::FrameSpan,
        cx: &mut Context<Self>,
    ) {
        let Some(range) = TimelineRange::new(
            TimelinePoint(span.start.max(0) as u64),
            TimelinePoint(span.end.max(0) as u64),
        ) else {
            self.constructive_status = Some(format!(
                "{} names an empty span, so there is nothing to hear",
                ObjectRef::Finding(finding).address()
            ));
            cx.notify();
            return;
        };
        self.dispatch_timeline_event(TimelineInteractionEvent::ReplaceSelection(Some(range)), cx);
        self.dispatch_timeline_event(
            TimelineInteractionEvent::ReplaceLoop(crate::timeline::LoopState::active(range)),
            cx,
        );
        self.dispatch_timeline_event(TimelineInteractionEvent::PlayRequested, cx);
        self.constructive_status = Some(format!(
            "Hearing {} – {} · the finding's span is the loop",
            format_time(self.seconds_for_sample(range.start.get())),
            format_time(self.seconds_for_sample(range.end.get()))
        ));
        cx.notify();
    }

    /// "Make sample" for a finding reached from a row rather than a pane.
    ///
    /// It asks the same result controller the pane asks, so availability,
    /// the pending ticket, and the durable receipt are decided in exactly one
    /// place. A finding whose analysis is not loaded is refused by name.
    pub(super) fn make_sample_from_finding(
        &mut self,
        finding: crate::project_controller::FindingRef,
        cx: &mut Context<Self>,
    ) {
        match self
            .reverse_surface_factory
            .clone()
            .request_finding_sample(finding, cx)
        {
            Ok(()) => {
                self.constructive_status = Some("Make sample requested".into());
            }
            Err(crate::reverse_surface_view::ReverseAnalysisResultError::UnknownFinding(_)) => {
                self.constructive_status = Some(
                    "This finding's analysis is not loaded, so there is no phase-bearing signal to sample; open its lens to run it again".into(),
                );
            }
            Err(error) => {
                self.constructive_status = Some(format!("Make sample · {error}"));
            }
        }
        cx.notify();
    }

    pub(super) fn make_sample_from_active_span(&mut self, cx: &mut Context<Self>) {
        self.publish_timeline_sample(SampleWorkflowCommand::MakeSample, cx);
    }

    pub(super) fn slice_active_span_to_kit(&mut self, cx: &mut Context<Self>) {
        self.publish_timeline_sample(SampleWorkflowCommand::SliceToPads, cx);
    }

    pub(super) fn make_beat_from_active_span(&mut self, cx: &mut Context<Self>) {
        self.publish_timeline_sample(SampleWorkflowCommand::MakeBeat, cx);
    }

    /// Reverse the selected sample zone, or the first zone of the project's
    /// first kit when nothing names one.
    ///
    /// The rule, and the receipt naming which zone moved, is the one
    /// `route_selected_channel` already established for edits asked for by
    /// name rather than by pointer. The edit itself is the same
    /// `ZoneEditIntent::SetReverse` the sampler inspector's REVERSE sends.
    pub(super) fn reverse_selected_zone(&mut self, cx: &mut Context<Self>) {
        use crate::sample_actions::{ZoneEditIntent, ZoneEditTarget};

        let session = self.session.read(cx);
        // A pad selection names a zone when a zone is what the musician
        // picked; with none, the first zone of the first kit is the one the
        // receipt will name.
        let selected = match session.selection().selection.objects.primary {
            Some(ObjectRef::Pad(pad)) => Some(pad),
            _ => None,
        };
        let resolved = session.project_snapshot().ok().and_then(|snapshot| {
            let kits = &snapshot.project.state().domains.sample_kits.kits;
            let (kit, zone) = kits.values().find_map(|kit| {
                let zone = selected
                    .filter(|pad| pad.kit == kit.id)
                    .and_then(|pad| {
                        pad.zone
                            .and_then(|zone| kit.zones.get(&zone))
                            .or_else(|| kit.ordered_zones(pad.pad).next())
                    })
                    .or_else(|| {
                        kit.pad_order
                            .iter()
                            .find_map(|pad| kit.ordered_zones(*pad).next())
                    })?;
                Some((kit, zone))
            })?;
            Some((
                ZoneEditTarget {
                    kit: kit.id,
                    pad: zone.pad,
                    zone: zone.id,
                    expected_revision: kit.revision,
                },
                !zone.reverse,
            ))
        });
        let Some((target, reverse)) = resolved else {
            self.constructive_status =
                Some("No sample zone to reverse yet · Make beat from a selection first".into());
            cx.notify();
            return;
        };
        let id = NEXT_CONTEXTUAL_SAMPLE_REQUEST.fetch_add(1, Ordering::Relaxed);
        self.sender().send(WorkbenchEvent::SampleRequest {
            source: None,
            request: SampleActionRequest {
                id: SampleRequestId(id.max(1)),
                action: SampleAction::EditZone(ZoneEditIntent::SetReverse { target, reverse }),
            },
            completion: None,
        });
        self.constructive_status = Some(format!(
            "Zone {} · {}",
            target.zone.get(),
            if reverse { "reversed" } else { "forwards" }
        ));
        cx.notify();
    }

    pub(super) fn make_beat_from_sampler(&mut self, view: WorkspaceViewId, cx: &mut Context<Self>) {
        let sampler = match self.workspace_panes.get(&view).cloned() {
            Some(WorkspacePaneRuntime::Hosted(host)) => {
                host.upgrade()
                    .and_then(|host| match &host.read(cx).content {
                        WorkspacePaneContent::Sampler(sampler) => Some(sampler.clone()),
                        _ => None,
                    })
            }
            _ => None,
        };
        let Some(sampler) = sampler else {
            self.constructive_status = Some("The instrument editor is no longer available".into());
            cx.notify();
            return;
        };
        let (source, state) = {
            let sampler = sampler.read(cx);
            (sampler.source().clone(), sampler.state())
        };
        let resolved = source
            .kits
            .lock()
            .map_err(|_| "The instrument library is busy".to_owned())
            .and_then(|library| {
                let kit = library
                    .kits
                    .get(&source.kit)
                    .ok_or_else(|| "The visible instrument is no longer current".to_owned())?;
                let zone = state
                    .selected_zone
                    .and_then(|zone| kit.zones.get(&zone))
                    .or_else(|| {
                        state
                            .selected_pad
                            .and_then(|pad| kit.ordered_zones(pad).next())
                    })
                    .ok_or_else(|| "Select a playable zone before making a beat".to_owned())?;
                let selection = match zone.material {
                    SourceMaterialRef::Asset(asset) => SampleSelection::whole_asset(asset),
                    SourceMaterialRef::VirtualSlice(slice) => SampleSelection {
                        asset: slice.source_asset,
                        source_range: Some(slice.source_range),
                    },
                };
                Ok((selection, kit.revision))
            });
        let (selection, expected_revision) = match resolved {
            Ok(resolved) => resolved,
            Err(message) => {
                let _ = self.set_workspace_completion(
                    view,
                    RevealCompletion {
                        headline: "Beat not created".into(),
                        breadcrumb: "Instrument › selected zone".into(),
                        diagnostic: Some(message),
                    },
                    cx,
                );
                return;
            }
        };
        let id = NEXT_CONTEXTUAL_SAMPLE_REQUEST.fetch_add(1, Ordering::Relaxed);
        let request = SampleActionRequest {
            id: SampleRequestId(id.max(1)),
            action: SampleAction::MakeBeat(MakeBeatIntent {
                source: selection,
                chop: SampleChopIntent::OneShot,
                kit: SampleKitDestination::ExistingKit {
                    kit: source.kit,
                    expected_revision,
                },
                target_bus: None,
                bars: 1,
                quantize_ticks: (crate::sequencer::PPQ / 4) as u64,
                result_focus: MakeBeatResultFocus::PatternEditor,
            }),
        };
        self.sender().send(WorkbenchEvent::SampleRequest {
            source: Some(view),
            request,
            completion: None,
        });
        let _ = self.set_workspace_completion(
            view,
            RevealCompletion {
                headline: "Making beat from selected zone…".into(),
                breadcrumb: "Instrument → Beat → Pattern".into(),
                diagnostic: None,
            },
            cx,
        );
    }

    pub(super) fn capture_pane_source(
        &self,
        span: RenderSpan,
        sample_rate: u32,
        source_mono: &[f32],
        cx: &App,
    ) -> Result<PaneSourcePin, String> {
        let session = self.session.read(cx);
        let revisions = session
            .project_snapshot()
            .map_err(|error| error.to_string())?
            .revisions();
        let format = RenderFormat::new(sample_rate, 1).map_err(|error| error.to_string())?;
        PaneSourcePin::new(
            session.document_generation(),
            session.snapshot().generation,
            revisions,
            None,
            span,
            format,
            source_mono,
        )
        .map_err(|error| error.to_string())
    }

    pub(super) fn pane_audition_context(&self, cx: &App) -> Result<PaneAuditionContext, String> {
        let session = self.session.read(cx);
        let revisions = session
            .project_snapshot()
            .map_err(|error| error.to_string())?
            .revisions();
        Ok(PaneAuditionContext {
            document_generation: session.document_generation(),
            publication_generation: session.snapshot().generation,
            revisions,
            audible_cohort: self
                .audio_controller
                .transport_session()
                .snapshot()
                .audible_cohort,
        })
    }

    pub(super) fn audition_pane_timeline(
        &mut self,
        owner: AuditionOwner,
        kind: PaneAudioKind,
        source: PaneSourcePin,
        mono: Arc<[f32]>,
        cx: &mut Context<Self>,
    ) {
        let Some(audio) = self.audio.as_ref() else {
            self.audio_error = Some("Project audio is not ready for aligned audition".into());
            cx.notify();
            return;
        };
        if !self.primary_source_timeline_aligned {
            self.audio_error = Some(
                "This analysis is not mapped to an exact project placement; aligned timeline audition is unavailable"
                    .into(),
            );
            cx.notify();
            return;
        }
        let Some(control) = self.audio_controller.renderer_control() else {
            self.audio_error = Some("Project renderer is not ready for aligned audition".into());
            cx.notify();
            return;
        };
        let current = match self.pane_audition_context(cx) {
            Ok(current) => current,
            Err(error) => {
                self.audio_error = Some(error);
                cx.notify();
                return;
            }
        };
        let effect = AnalysisPaneBridge::from_owner(owner).timeline_mono(
            kind,
            source,
            control.format(),
            mono,
            AuditionAlignment::SeekToStart { play: true },
        );
        let result =
            effect.and_then(|effect| effect.apply(&mut self.audio_controller, audio, &current));
        match result {
            Ok(()) => {
                self.publish_audio_status(cx);
                cx.notify();
            }
            Err(error) => {
                self.audio_error = Some(error.to_string());
                cx.notify();
            }
        }
    }

    pub(super) fn preview_pane_mono(
        &mut self,
        owner: AuditionOwner,
        kind: PaneAudioKind,
        source: &PaneSourcePin,
        sample_rate: u32,
        mono: Arc<[f32]>,
        cx: &mut Context<Self>,
    ) {
        let Some(audio) = self.audio.as_ref() else {
            self.audio_error = Some("Project preview bus is not ready".into());
            cx.notify();
            return;
        };
        let current = match self.pane_audition_context(cx) {
            Ok(current) => current,
            Err(error) => {
                self.audio_error = Some(error);
                cx.notify();
                return;
            }
        };
        let effect = AnalysisPaneBridge::from_owner(owner).short_preview_mono(
            &mut self.preview_controller,
            kind,
            source,
            &current,
            sample_rate,
            mono,
        );
        match effect {
            Ok(effect) => {
                effect.apply(&mut self.preview_controller, audio);
                self.audio_error = None;
            }
            Err(error) => self.audio_error = Some(error.to_string()),
        }
        cx.notify();
    }
}
