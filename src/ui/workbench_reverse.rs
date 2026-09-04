//! Reverse-surface events, analysis findings, constructions, and comparisons.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl Workbench {
    pub(super) fn on_reverse_surface_event(
        &mut self,
        event: ReverseSurfaceViewEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            ReverseSurfaceViewEvent::Action {
                view,
                intent: SurfaceActionIntent::Reveal(mut request),
            } => {
                request.current_view = Some(view);
                match self.session.read(cx).issue_reveal(request) {
                    Ok(receipt) => {
                        self.object_reveals.push(PendingObjectReveal {
                            receipt,
                            diagnostics: Vec::new(),
                            headline: "Evidence selected".into(),
                        });
                    }
                    Err(error) => {
                        self.constructive_status =
                            Some(format!("Evidence reveal unavailable · {error}"));
                    }
                }
            }
            ReverseSurfaceViewEvent::Action {
                view,
                intent:
                    SurfaceActionIntent::ApplyExplicitConsequence {
                        document,
                        consequence,
                        requested_at,
                        ..
                    },
            } => {
                let current = self
                    .session
                    .read(cx)
                    .project_snapshot()
                    .ok()
                    .map(|snapshot| snapshot.revisions());
                if requested_at.is_some() && requested_at != current {
                    self.constructive_status = Some(
                        "Reverse edit was not applied because its project receipt is stale".into(),
                    );
                } else if consequence.authority == EditAuthority::ProjectCommand
                    && consequence.key == CONSEQUENCE_APPLY_CONSTRUCTION
                {
                    self.apply_reverse_construction(
                        view,
                        DeprojectionWorkspaceTarget::Object(document),
                        cx,
                    );
                } else if consequence.authority == EditAuthority::ProjectCommand
                    && consequence.key == CONSEQUENCE_KEEP_FINDING
                {
                    match keep_reverse_finding(self.session.read(cx), &document, requested_at) {
                        Ok(outcome) => {
                            self.enqueue_reveal_recommendation(
                                outcome.reveal,
                                Some(view),
                                |_| "Finding kept",
                                cx,
                            );
                            self.constructive_status = Some("Finding kept".into());
                        }
                        Err(error) => {
                            self.constructive_status =
                                Some(format!("{} · {error}", consequence.label));
                        }
                    }
                } else {
                    self.constructive_status = Some(format!(
                        "{} · {:?} has no executable host adapter",
                        consequence.label, consequence.authority
                    ));
                }
            }
            ReverseSurfaceViewEvent::Audition { view, intent } => {
                let request = match intent {
                    SurfaceAuditionIntent::Signal(request) => request,
                    SurfaceAuditionIntent::InspectExcess { controller, .. } => controller,
                };
                self.request_comparison_product(view, request, cx);
            }
        }
        cx.notify();
    }

    pub(super) fn on_reverse_analysis_result_event(
        &mut self,
        event: ReverseAnalysisResultEvent,
        cx: &mut Context<Self>,
    ) {
        // An unqualified audition abandons this event, not the repaint the old
        // batch drain always ran afterwards.
        'event: {
            match event {
                ReverseAnalysisResultEvent::Durable { view, intent } => {
                    let ticket = intent.ticket();
                    let completion = match intent {
                        AnalysisDurableIntent::KeepFinding {
                            descriptor,
                            finding,
                            ..
                        } => self.analysis_finding_retention(finding, cx).and_then(
                            |(artifact, retention_revision)| {
                                let retained = self
                                    .session
                                    .read(cx)
                                    .deprojection_workspace_artifacts()
                                    .descriptor(descriptor.id)
                                    .cloned()
                                    .ok_or_else(|| {
                                        "the analysis artifact is no longer retained".to_owned()
                                    })?;
                                if retained != descriptor || artifact != descriptor.id {
                                    return Err(
                                        "the retained artifact no longer matches this Finding"
                                            .to_owned(),
                                    );
                                }
                                Ok(AnalysisDurableCompletion::Kept {
                                    ticket,
                                    artifact: descriptor.id,
                                    finding,
                                    retention_revision,
                                })
                            },
                        ),
                        AnalysisDurableIntent::Compare {
                            target, evidence, ..
                        } => self
                            .analysis_candidate_summary(evidence, cx)
                            .and_then(|summary| {
                                if summary.comparison != target.comparison
                                    || summary.explanation != target.explanation
                                {
                                    return Err(
                                        "the comparison binding was superseded by a newer analysis"
                                            .to_owned(),
                                    );
                                }
                                Ok(AnalysisDurableCompletion::Compared {
                                    ticket,
                                    target,
                                    interpretation_revision: summary.pin.catalog_generation.max(1),
                                })
                            }),
                        AnalysisDurableIntent::ApplyConstruction {
                            target, evidence, ..
                        } => match target {
                            AnalysisPromotionTarget::LoomSequence {
                                artifact,
                                scoped_evidence,
                            } => {
                                if scoped_evidence != evidence
                                    || evidence.scope != FindingScope::Artifact(artifact)
                                {
                                    Err("the Loom promotion target no longer matches this Finding"
                                        .to_owned())
                                } else {
                                    self.execute_loom_result_construction(artifact, evidence, cx)
                                        .map(|publication| AnalysisDurableCompletion::Applied {
                                            ticket,
                                            publication,
                                        })
                                }
                            }
                            AnalysisPromotionTarget::Deprojection(target) => {
                                let current = self.analysis_candidate_summary(evidence, cx);
                                current.and_then(|summary| {
                                    let expected = DeprojectionWorkspaceTarget::Object(
                                        ObjectRef::Finding(summary.finding),
                                    );
                                    if target != expected {
                                        return Err(
                                            "the promotion target no longer matches this Finding"
                                                .to_owned(),
                                        );
                                    }
                                    let applied =
                                        self.execute_reverse_construction(view, target, cx)?;
                                    if applied.artifact != summary.artifact {
                                        return Err(
                                            "the applied construction came from a different artifact"
                                                .to_owned(),
                                        );
                                    }
                                    Ok(AnalysisDurableCompletion::AppliedObjects {
                                        ticket,
                                        revision: applied.revision,
                                        primary: applied.primary,
                                        related: applied.related,
                                    })
                                })
                            }
                            AnalysisPromotionTarget::RhythmChoice { .. } => Err(
                                "the selected rhythm construction belongs to the rhythm chooser"
                                    .to_owned(),
                            ),
                        },
                        AnalysisDurableIntent::MakeSample {
                            source, evidence, ..
                        } => self.materialize_analysis_sample(source, evidence, cx).map(
                            |publication| AnalysisDurableCompletion::Sampled {
                                ticket,
                                publication,
                            },
                        ),
                    };
                    match completion {
                        Ok(completion) => match self
                            .reverse_surface_factory
                            .complete_analysis_result(completion, cx)
                        {
                            Ok(receipt) => {
                                self.constructive_status = Some(format!(
                                    "{} is durable at revision {}",
                                    receipt.primary.address(),
                                    receipt.durable_revision
                                ));
                            }
                            Err(error) => {
                                self.constructive_status = Some(format!(
                                    "Analysis result completion was rejected · {error}"
                                ));
                            }
                        },
                        Err(error) => {
                            self.reverse_surface_factory
                                .cancel_analysis_result(ticket, cx);
                            self.constructive_status =
                                Some(format!("Analysis result action was not applied · {error}"));
                        }
                    }
                }
                ReverseAnalysisResultEvent::Audition { intent, .. } => {
                    let artifact = match intent.finding().scope {
                        FindingScope::Artifact(artifact) => artifact,
                        _ => {
                            self.constructive_status =
                                Some("Analysis audition is not qualified by an artifact".into());
                            break 'event;
                        }
                    };
                    let kind = intent.kind();
                    let mut product = self.analysis_pcm_products.get(&(artifact, kind)).cloned();
                    if product.is_none() && kind == PaneAudioKind::LoomTemplate {
                        let local_key = self
                            .session
                            .read(cx)
                            .list_analysis_evidence_findings()
                            .ok()
                            .and_then(|summaries| {
                                summaries.into_iter().find_map(|summary| {
                                    if summary.finding != intent.finding() {
                                        return None;
                                    }
                                    match summary.kind {
                                        AnalysisEvidenceKind::LoomTemplate { cluster_id } => {
                                            Some(cluster_id as u64)
                                        }
                                        _ => None,
                                    }
                                })
                            });
                        product = local_key.and_then(|local_key| {
                            self.analysis_derived_pcm_products
                                .get(&(artifact, local_key))
                                .cloned()
                        });
                    }
                    match product {
                        Some(product)
                            if kind.route()
                                == crate::pane_audio::PaneAudioRoute::TimelineAligned =>
                        {
                            self.audition_pane_timeline(
                                intent.owner(),
                                kind,
                                product.source,
                                product.mono,
                                cx,
                            );
                            self.constructive_status = Some(format!(
                                "{} is aligned to the project transport",
                                product.label
                            ));
                        }
                        Some(product)
                            if kind.route() == crate::pane_audio::PaneAudioRoute::ShortPreview =>
                        {
                            self.preview_pane_mono(
                                intent.owner(),
                                kind,
                                &product.source,
                                product.sample_rate,
                                product.mono,
                                cx,
                            );
                            self.constructive_status =
                                Some(format!("Previewing {}", product.label));
                        }
                        Some(_) => {
                            self.constructive_status = Some(
                                "This analysis result has evidence but no audible signal".into(),
                            );
                        }
                        None => {
                            self.constructive_status = Some(format!(
                                "The retained {:?} signal is unavailable or was superseded",
                                kind
                            ));
                        }
                    }
                }
            }
        }
        cx.notify();
    }

    pub(super) fn analysis_candidate_summary(
        &self,
        finding: crate::project_controller::FindingRef,
        cx: &App,
    ) -> Result<DeprojectionCandidateDocumentSummary, String> {
        self.session
            .read(cx)
            .list_deprojection_workspace_candidates()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|summary| {
                summary.finding == finding
                    && matches!(summary.freshness, DeprojectionCandidateFreshness::Current)
            })
            .ok_or_else(|| "the analysis Finding was superseded or removed".to_owned())
    }

    pub(super) fn analysis_finding_retention(
        &self,
        finding: crate::project_controller::FindingRef,
        cx: &App,
    ) -> Result<(ArtifactId, u64), String> {
        let session = self.session.read(cx);
        if let Some(summary) = session
            .list_deprojection_workspace_candidates()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|summary| {
                summary.finding == finding
                    && summary.freshness == DeprojectionCandidateFreshness::Current
            })
        {
            return Ok((summary.artifact, summary.pin.catalog_generation.max(1)));
        }
        session
            .list_analysis_evidence_findings()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|summary| {
                summary.finding == finding
                    && summary.freshness == DeprojectionCandidateFreshness::Current
            })
            .map(|summary| (summary.artifact, summary.pin.catalog_generation.max(1)))
            .ok_or_else(|| "the analysis Finding was superseded or removed".to_owned())
    }

    pub(super) fn reveal_analysis_finding(
        &mut self,
        source_view: WorkspaceViewId,
        finding: crate::project_controller::FindingRef,
        cx: &mut Context<Self>,
    ) {
        let request = crate::project_controller::RevealRequest::new(
            ObjectRef::Finding(finding),
            RevealIntent::ActivateExisting,
        )
        .with_current_view(source_view);
        match self.session.read(cx).issue_reveal(request) {
            Ok(receipt) => {
                self.object_reveals.push(PendingObjectReveal {
                    receipt,
                    diagnostics: Vec::new(),
                    headline: "Analysis Finding opened".into(),
                });
            }
            Err(error) => {
                self.constructive_status =
                    Some(format!("Analysis Finding could not be opened · {error}"));
            }
        }
        cx.notify();
    }

    pub(super) fn keep_analysis_finding(
        &mut self,
        source_view: WorkspaceViewId,
        finding: crate::project_controller::FindingRef,
        cx: &mut Context<Self>,
    ) {
        match keep_reverse_finding(self.session.read(cx), &ObjectRef::Finding(finding), None) {
            Ok(outcome) => {
                self.enqueue_reveal_recommendation(
                    outcome.reveal,
                    Some(source_view),
                    |_| "Finding kept",
                    cx,
                );
                self.constructive_status = Some("Finding kept".into());
            }
            Err(error) => {
                self.constructive_status = Some(format!("Finding was not kept · {error}"));
            }
        }
        cx.notify();
    }

    pub(super) fn refresh_reverse_surface_documents(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<usize, String> {
        let documents = {
            let session = self.session.read(cx);
            let summaries = session
                .list_deprojection_workspace_candidates()
                .map_err(|error| error.to_string())?;
            let evidence = session
                .list_analysis_evidence_findings()
                .map_err(|error| error.to_string())?;
            project_reverse_surface_documents(
                summaries.iter(),
                evidence.iter(),
                session.deprojection_workspace_artifacts(),
                session.deprojection_workspace_interpretations(),
            )
            .map_err(|error| error.to_string())?
        };
        let count = documents.len();
        self.reverse_surface_factory
            .replace_documents(documents, cx)
            .map_err(|error| error.to_string())?;
        Ok(count)
    }

    pub(super) fn register_rhythm_analysis_results(
        &mut self,
        descriptor: &ArtifactDescriptor,
        summaries: &[DeprojectionCandidateDocumentSummary],
        source: &PaneSourcePin,
        cx: &mut Context<Self>,
    ) -> Result<usize, String> {
        let mut registered = 0;
        for summary in summaries {
            if summary.artifact != descriptor.id {
                return Err("rhythm candidate references a different artifact".into());
            }
            let result = TemporaryAnalysisResult::new(
                descriptor.clone(),
                summary.finding,
                summary.label.clone(),
                AnalysisResultKind::RhythmPattern,
                source.clone(),
                AnalysisResultBindings::from_workspace_candidate(summary)
                    .map_err(|error| error.to_string())?,
                None,
            )
            .map_err(|error| error.to_string())?;
            // Identical reruns are explicit replacements. A result with an
            // in-flight durable action refuses invalidation so late host
            // completions cannot land on a different card generation.
            self.reverse_surface_factory
                .invalidate_analysis_result(summary.finding, cx)
                .map_err(|error| error.to_string())?;
            self.reverse_surface_factory
                .insert_analysis_result(result, cx)
                .map_err(|error| error.to_string())?;
            registered += 1;
        }
        Ok(registered)
    }

    pub(super) fn register_hpss_analysis_results(
        &mut self,
        descriptor: &ArtifactDescriptor,
        summaries: &[AnalysisEvidenceDocumentSummary],
        source: &PaneSourcePin,
        original: Arc<[f32]>,
        result: &crate::hpss::HpssResult,
        cx: &mut Context<Self>,
    ) -> Result<usize, String> {
        let products = [
            (PaneAudioKind::HpssSource, original, "Selected source"),
            (
                PaneAudioKind::HpssHarmonic,
                Arc::from(result.harmonic.clone()),
                "Tonally sustained estimate",
            ),
            (
                PaneAudioKind::HpssTransient,
                Arc::from(result.percussive.clone()),
                "Transient estimate",
            ),
            (
                PaneAudioKind::HpssResidual,
                Arc::from(result.residual.clone()),
                "HPSS residual",
            ),
        ];
        for (kind, mono, label) in products {
            self.analysis_pcm_products.insert(
                (descriptor.id, kind),
                AnalysisPcmProduct {
                    source: source.clone(),
                    sample_rate: descriptor.sample_rate,
                    mono,
                    label: label.into(),
                },
            );
        }

        let mut registered = 0;
        for summary in summaries {
            let temporary = TemporaryAnalysisResult::hpss_evidence(
                descriptor.clone(),
                summary,
                source.clone(),
                result,
            )
            .map_err(|error| error.to_string())?;
            self.reverse_surface_factory
                .invalidate_analysis_result(summary.finding, cx)
                .map_err(|error| error.to_string())?;
            self.reverse_surface_factory
                .insert_analysis_result(temporary, cx)
                .map_err(|error| error.to_string())?;
            registered += 1;
        }
        Ok(registered)
    }

    pub(super) fn register_loom_analysis_results(
        &mut self,
        descriptor: &ArtifactDescriptor,
        summaries: &[AnalysisEvidenceDocumentSummary],
        source: &PaneSourcePin,
        original: Arc<[f32]>,
        sketch: &SequenceSketch,
        cx: &mut Context<Self>,
    ) -> Result<usize, String> {
        let source_start = usize::try_from(source.span.start)
            .map_err(|_| "Loom evidence begins before the project timeline".to_owned())?;
        let construction: Arc<[f32]> = Arc::from(sketch.render_span(source_start, original.len()));
        let residual: Arc<[f32]> = Arc::from(
            original
                .iter()
                .zip(construction.iter())
                .map(|(source, rendered)| source - rendered)
                .collect::<Vec<_>>(),
        );
        for (kind, mono, label) in [
            (PaneAudioKind::LoomSource, original, "Loom source"),
            (
                PaneAudioKind::LoomConstruction,
                construction,
                "Loom construction",
            ),
            (PaneAudioKind::LoomResidual, residual, "Loom residual"),
        ] {
            self.analysis_pcm_products.insert(
                (descriptor.id, kind),
                AnalysisPcmProduct {
                    source: source.clone(),
                    sample_rate: descriptor.sample_rate,
                    mono,
                    label: label.into(),
                },
            );
        }

        if let Some(sequence) = summaries
            .iter()
            .find(|summary| summary.kind == AnalysisEvidenceKind::LoomSequence)
        {
            self.loom_construction_products.insert(
                descriptor.id,
                LoomConstructionProduct {
                    source: source.clone(),
                    sketch: sketch.clone(),
                    label: sequence.label.clone(),
                    finding: sequence.finding,
                    diverged_from_evidence: false,
                },
            );
        }

        let mut registered = 0;
        for summary in summaries {
            let temporary = match summary.kind {
                AnalysisEvidenceKind::LoomSequence => {
                    TemporaryAnalysisResult::loom_sequence_evidence(
                        descriptor.clone(),
                        summary,
                        source.clone(),
                        sketch,
                    )
                }
                AnalysisEvidenceKind::LoomTemplate { cluster_id } => {
                    let cluster = sketch.cluster(cluster_id).ok_or_else(|| {
                        format!("Loom template {cluster_id} is no longer retained")
                    })?;
                    self.analysis_derived_pcm_products.insert(
                        (descriptor.id, cluster_id as u64),
                        AnalysisPcmProduct {
                            source: source.clone(),
                            sample_rate: descriptor.sample_rate,
                            mono: Arc::from(cluster.template.samples.clone()),
                            label: summary.label.clone(),
                        },
                    );
                    TemporaryAnalysisResult::loom_template_evidence(
                        descriptor.clone(),
                        summary,
                        source.clone(),
                        sketch,
                    )
                }
                AnalysisEvidenceKind::HpssComponent(_) => {
                    return Err("HPSS evidence was routed to the Loom result adapter".into())
                }
                AnalysisEvidenceKind::ComponentMagnitude { .. } => {
                    return Err("NMF evidence was routed to the Loom result adapter".into())
                }
            }
            .map_err(|error| error.to_string())?;
            self.reverse_surface_factory
                .invalidate_analysis_result(summary.finding, cx)
                .map_err(|error| error.to_string())?;
            self.reverse_surface_factory
                .insert_analysis_result(temporary, cx)
                .map_err(|error| error.to_string())?;
            registered += 1;
        }
        Ok(registered)
    }

    pub(super) fn register_components_analysis_results(
        &mut self,
        descriptor: &ArtifactDescriptor,
        summaries: &[AnalysisEvidenceDocumentSummary],
        source: &PaneSourcePin,
        cx: &mut Context<Self>,
    ) -> Result<usize, String> {
        let mut registered = 0;
        for summary in summaries {
            if summary.artifact != descriptor.id {
                return Err("component evidence references a different artifact".into());
            }
            let temporary = TemporaryAnalysisResult::component_magnitude_evidence(
                descriptor.clone(),
                summary,
                source.clone(),
            )
            .map_err(|error| error.to_string())?;
            self.reverse_surface_factory
                .invalidate_analysis_result(summary.finding, cx)
                .map_err(|error| error.to_string())?;
            self.reverse_surface_factory
                .insert_analysis_result(temporary, cx)
                .map_err(|error| error.to_string())?;
            registered += 1;
        }
        Ok(registered)
    }

    pub(super) fn materialize_analysis_sample(
        &mut self,
        source: crate::pane_audio::result_lifecycle::AnalysisSampleSource,
        evidence: crate::project_controller::FindingRef,
        cx: &mut Context<Self>,
    ) -> Result<crate::project_controller::ConstructivePublication, String> {
        let (artifact, product) = match source {
            crate::pane_audio::result_lifecycle::AnalysisSampleSource::ArtifactSignal {
                artifact,
                signal,
                span,
            } => {
                let product = self
                    .analysis_pcm_products
                    .get(&(artifact, signal))
                    .cloned()
                    .ok_or_else(|| "the phase-bearing analysis signal was superseded".to_owned())?;
                if product.source.span != span {
                    return Err("the retained analysis signal no longer matches this span".into());
                }
                (artifact, product)
            }
            crate::pane_audio::result_lifecycle::AnalysisSampleSource::ExactSource(_) => {
                return Err(
                    "source-range result sampling must use the ordinary material workflow".into(),
                )
            }
            crate::pane_audio::result_lifecycle::AnalysisSampleSource::DerivedPcm {
                artifact,
                local_key,
                content,
                frames,
                sample_rate,
                channels,
            } => {
                let product = self
                    .analysis_derived_pcm_products
                    .get(&(artifact, local_key))
                    .cloned()
                    .ok_or_else(|| "the derived analysis template was superseded".to_owned())?;
                if content != crate::render_runtime::canonical_pcm_digest(&product.mono)
                    || frames != product.mono.len() as u64
                    || sample_rate != product.sample_rate
                    || channels != 1
                {
                    return Err("the derived analysis template identity no longer matches".into());
                }
                (artifact, product)
            }
        };
        if evidence.scope != FindingScope::Artifact(artifact) {
            return Err("sample evidence and artifact identities do not match".into());
        }
        let format = AudioFormat::new(product.sample_rate, 1).map_err(|error| error.to_string())?;
        let pcm =
            PcmAsset::new(format, Arc::clone(&product.mono)).map_err(|error| error.to_string())?;
        let identity = canonical_pcm_identity(DecodedPcmView::from_pcm_asset(&pcm))
            .map_err(|error| error.to_string())?;
        let digest = content_digest_hex(artifact.0);
        let relative = ProjectRelativePath::parse(format!(
            "media/analysis/{digest}-{}.f32pcm",
            identity.fingerprint.id.to_hex()
        ))
        .map_err(|error| error.to_string())?;
        let location =
            AssetLocation::new(None, Some(relative)).map_err(|error| error.to_string())?;
        let registration = AssetRegistration {
            name: product.label.clone(),
            location: location.clone(),
            metadata: DecodedAudioMetadata {
                sample_rate_hz: product.sample_rate,
                channels: 1,
                frame_count: SampleFrames(identity.frame_count),
                container: Some("audec-pcm".into()),
                codec: Some("f32le".into()),
                bit_depth: Some(32),
            },
            content: identity.fingerprint,
            provenance: AssetProvenance::new(
                unix_time_ms(),
                AssetOrigin::Generated {
                    generator: format!(
                        "audec analysis materializer · {}",
                        ObjectRef::Finding(evidence).address()
                    ),
                },
                location,
            ),
            tags: BTreeSet::from([
                "analysis-derived".into(),
                "phase-bearing".into(),
                "sample".into(),
            ]),
            favorite: false,
        };
        let end = i64::try_from(identity.frame_count)
            .map_err(|_| "analysis sample is too long for the source timeline".to_owned())?;
        let range = SampleRange::new(Sample::new(0), Sample::new(end));
        let instrument_name = format!("{} instrument", product.label);
        let spec = SampleWorkflowSpec::expected(
            SampleWorkflowCommand::MakeSample,
            SampleSpanOrigin::Selection,
            &product.label,
            SampleInstrumentDestination::New {
                name: instrument_name,
            },
            None,
        );
        let outcome = self.session.update(cx, |session, _| {
            let expected_revision = session
                .project_snapshot()
                .map_err(|error| error.to_string())?
                .revisions()
                .aggregate;
            let imported = session
                .import_asset(expected_revision, registration, pcm)
                .map_err(|error| error.to_string())?;
            session
                .publish_workbench_range(
                    imported.asset,
                    range,
                    WorkbenchSampleIntent::Workflow(spec),
                )
                .map_err(|error| error.to_string())
        })?;
        self.handle_session_events(cx);
        let _ = self.refresh_reverse_surface_documents(cx);
        Ok(outcome.constructive.publication)
    }

    pub(super) fn execute_loom_result_construction(
        &mut self,
        artifact: ArtifactId,
        evidence: crate::project_controller::FindingRef,
        cx: &mut Context<Self>,
    ) -> Result<crate::project_controller::ConstructivePublication, String> {
        let product = self
            .loom_construction_products
            .get(&artifact)
            .cloned()
            .ok_or_else(|| "the editable Loom sequence was superseded".to_owned())?;
        if product.finding != evidence
            || evidence.kind != crate::project_controller::FindingKind::Loom
            || evidence.scope != FindingScope::Artifact(artifact)
        {
            return Err("the Loom construction no longer matches this Finding".into());
        }
        let source_span = FrameSpan::new(product.source.span.start, product.source.span.end)
            .ok_or_else(|| "the Loom construction has an empty source extent".to_owned())?;
        let outcome = self
            .session
            .update(cx, |session, _| {
                session.execute_loom_construction(LoomConstructionIntent {
                    artifact,
                    finding: evidence,
                    source_span,
                    sketch: product.sketch,
                    label: product.label,
                    diverged_from_evidence: product.diverged_from_evidence,
                    created_unix_ms: unix_time_ms(),
                    target_bus: None,
                })
            })
            .map_err(|error| error.to_string())?;
        let publication = outcome.publication.clone();
        self.handle_session_events(cx);
        let refreshed = self.refresh_reverse_surface_documents(cx);
        self.constructive_status = Some(match refreshed {
            Ok(documents) => format!(
                "Loom construction committed at revision {} · {} pad(s) · {documents} reverse documents refreshed",
                publication.revision,
                publication.created_pads.len()
            ),
            Err(error) => format!(
                "Loom construction committed at revision {}; reverse surfaces need refresh · {error}",
                publication.revision
            ),
        });
        Ok(publication)
    }

    pub(super) fn apply_reverse_construction(
        &mut self,
        view: WorkspaceViewId,
        target: DeprojectionWorkspaceTarget,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = self.execute_reverse_construction(view, target, cx) {
            self.constructive_status =
                Some(format!("Editable construction was not applied · {error}"));
        }
    }

    pub(super) fn execute_reverse_construction(
        &mut self,
        view: WorkspaceViewId,
        target: DeprojectionWorkspaceTarget,
        cx: &mut Context<Self>,
    ) -> Result<AppliedReverseConstruction, String> {
        let cancellation = RenderCancellation::new();
        let plan = {
            let session = self.session.read(cx);
            session
                .resolve_deprojection_workspace_request(target)
                .map_err(|error| error.to_string())
                .and_then(|resolved| {
                    plan_artifact_promotion_comparison(
                        &session,
                        session.deprojection_workspace_artifacts(),
                        resolved.request,
                        &cancellation,
                    )
                    .map_err(|error| error.to_string())
                })
        };
        let result = plan.and_then(|plan| {
            let session = self.session.clone();
            session
                .update(cx, |session, _| plan.execute(session, &cancellation))
                .map_err(|error| error.to_string())
        });
        match result {
            Ok(result) => {
                let result = Arc::new(result);
                let artifact = result.descriptor.id;
                let publication = result.promotion.project.publication.clone();
                let revision = publication.revisions.aggregate;
                let created_count = result.promotion.created.len();
                let mut created = result
                    .promotion
                    .created
                    .iter()
                    .filter_map(object_from_promoted_created)
                    .collect::<Vec<_>>();
                created.sort_by_key(|object| (promotion_reveal_rank(object), object.address()));
                created.dedup();
                self.reverse_promotion_waits
                    .insert(view, Arc::clone(&result));
                self.request_project_audio(publication, cx);
                let hydrated = self.refresh_reverse_surface_documents(cx);

                let mut reveal_warning = None;
                let primary = created
                    .first()
                    .cloned()
                    .unwrap_or(ObjectRef::Comparison(result.target.comparison));
                let related = if created.is_empty() {
                    Vec::new()
                } else {
                    created.iter().skip(1).cloned().collect::<Vec<_>>()
                };
                let request = crate::project_controller::RevealRequest::new(
                    primary.clone(),
                    RevealIntent::ActivateExisting,
                )
                .at_revision(revision)
                .with_current_view(view)
                .with_related(related.clone());
                match self.session.read(cx).issue_reveal(request) {
                    Ok(receipt) => {
                        self.object_reveals.push(PendingObjectReveal {
                            receipt,
                            diagnostics: Vec::new(),
                            headline: "Editable construction created".into(),
                        });
                    }
                    Err(error) => {
                        reveal_warning = Some(error.to_string());
                    }
                }
                let mut status = match hydrated {
                    Ok(document_count) => format!(
                        "Editable construction committed at revision {revision} · {} created object(s) · {document_count} reverse documents refreshed",
                        created_count
                    ),
                    Err(error) => format!(
                        "Editable construction committed at revision {revision}; reverse surfaces need refresh · {error}"
                    ),
                };
                if let Some(error) = reveal_warning {
                    status.push_str(&format!(" · destination reveal unavailable: {error}"));
                }
                self.constructive_status = Some(status);
                Ok(AppliedReverseConstruction {
                    artifact,
                    revision,
                    primary,
                    related,
                })
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn request_comparison_product(
        &mut self,
        view: WorkspaceViewId,
        request: ComparisonSelectionRequest,
        cx: &mut Context<Self>,
    ) {
        let Some(controller) = self.reverse_surface_factory.controller(view) else {
            return;
        };
        let owner = controller
            .lock()
            .map(|controller| controller.owner())
            .unwrap_or_else(|poisoned| poisoned.into_inner().owner());
        let _ = self.audio_controller.stop_scoped_audition(owner);
        let semantics = match self.comparison_semantics_for(&request, cx) {
            Ok(semantics) => semantics,
            Err(message) => {
                if let Ok(mut controller) = controller.lock() {
                    let _ = controller.fail_request(&request, message.clone());
                }
                self.constructive_status = Some(message);
                self.reverse_surface_factory.refresh_controller(view, cx);
                self.publish_audio_status(cx);
                return;
            }
        };
        let capture = self.comparison_executor.capture(
            owner,
            request.clone(),
            self.session.read(cx),
            &self.audio_controller,
            semantics,
            ComparisonProductRecipe::default(),
        );
        match capture {
            Ok(job) => {
                self.constructive_status = Some(format!(
                    "Rendering aligned comparison {:?} {:?}",
                    request.comparison, request.channel
                ));
                let execution = cx.background_spawn(async move { job.execute() });
                cx.spawn(async move |this, cx| {
                    let result = execution.await;
                    let _ = this.update(cx, |this, cx| {
                        this.complete_comparison_product(view, owner, request, result, cx)
                    });
                })
                .detach();
            }
            Err(error) => {
                if let Ok(mut controller) = controller.lock() {
                    let _ = controller.fail_request(&request, error.to_string());
                }
                self.constructive_status = Some(error.to_string());
            }
        }
        self.reverse_surface_factory.refresh_controller(view, cx);
        self.publish_audio_status(cx);
    }

    pub(super) fn comparison_semantics_for(
        &self,
        request: &ComparisonSelectionRequest,
        cx: &App,
    ) -> Result<ComparisonSemanticSnapshot, String> {
        let store = self
            .reverse_surface_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let comparison = store
            .get(&ObjectRef::Comparison(request.comparison))
            .and_then(|document| match &document.body {
                ReverseSurfaceBody::Comparison(comparison) => Some(comparison.clone()),
                _ => None,
            })
            .ok_or_else(|| {
                format!(
                    "Comparison {:?} has no hydrated semantic document",
                    request.comparison
                )
            })?;
        let explanation = store
            .get(&ObjectRef::Explanation(comparison.definition.explanation))
            .and_then(|document| match &document.body {
                ReverseSurfaceBody::Explanation(explanation) => {
                    Some(explanation.definition.clone())
                }
                _ => None,
            })
            .ok_or_else(|| {
                format!(
                    "Explanation {:?} has no hydrated semantic document",
                    comparison.definition.explanation
                )
            })?;
        let observation = comparison.observation.ok_or_else(|| {
            format!(
                "Comparison {:?} has no recorded observation",
                request.comparison
            )
        })?;
        drop(store);

        let mut interpretations = InterpretationStore::new();
        interpretations
            .apply(&[
                InterpretationCommand::PutExplanation {
                    before: None,
                    after: Some(explanation),
                },
                InterpretationCommand::PutComparison {
                    before: None,
                    after: Some(comparison.definition),
                },
                InterpretationCommand::PutObservation {
                    comparison: request.comparison,
                    before: None,
                    after: Some(observation),
                },
            ])
            .map_err(|error| format!("Comparison semantic hydration failed · {error}"))?;
        let source_artifacts = self.session.read(cx);
        let source_artifacts = source_artifacts.deprojection_workspace_artifacts();
        let mut artifacts = ArtifactCatalog::new();
        for descriptor in source_artifacts.descriptors().cloned() {
            let payload = source_artifacts
                .get::<ArtifactComparisonPayload>(descriptor.id)
                .map_err(|error| format!("Comparison artifact hydration failed · {error}"))?;
            artifacts
                .insert(descriptor, payload)
                .map_err(|error| format!("Comparison artifact hydration failed · {error}"))?;
        }
        Ok(ComparisonSemanticSnapshot {
            interpretations: Arc::new(interpretations),
            artifacts: Arc::new(artifacts),
        })
    }

    pub(super) fn complete_comparison_product(
        &mut self,
        view: WorkspaceViewId,
        owner: AuditionOwner,
        request: ComparisonSelectionRequest,
        result: Result<ComparisonProductCompletion, ComparisonProductExecutorError>,
        cx: &mut Context<Self>,
    ) {
        let Some(shared_controller) = self.reverse_surface_factory.controller(view) else {
            self.comparison_executor.cancel_owner(owner);
            return;
        };
        let mut controller = shared_controller
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match result {
            Ok(completion) => {
                match self.comparison_executor.publish(
                    self.session.read(cx),
                    &mut controller,
                    completion,
                ) {
                    Ok(published) => {
                        let applied = self.audio.as_ref().ok_or_else(|| {
                        "comparison product is ready, but the project audio host is unavailable"
                            .to_owned()
                    }).and_then(|host| {
                        controller
                            .apply_audio_effect(
                                &mut self.audio_controller,
                                host,
                                published.effect,
                                AuditionAlignment::SeekToStart { play: true },
                            )
                            .map_err(|error| error.to_string())
                    });
                        self.constructive_status = Some(match applied {
                            Ok(()) => format!(
                                "Comparison {:?} {:?} is aligned to the project transport",
                                request.comparison, request.channel
                            ),
                            Err(error) => {
                                let _ = controller.fail_request(&request, error.clone());
                                error
                            }
                        });
                    }
                    Err(error) => {
                        let _ = controller.fail_request(&request, error.to_string());
                        self.constructive_status = Some(error.to_string());
                    }
                }
            }
            Err(error) => {
                let _ = controller.fail_request(&request, error.to_string());
                self.constructive_status = Some(error.to_string());
            }
        }
        drop(controller);
        self.reverse_surface_factory.refresh_controller(view, cx);
        self.publish_audio_status(cx);
        cx.notify();
    }
}
