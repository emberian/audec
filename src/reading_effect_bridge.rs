//! Typed execution plans for reading/query audition and reveal effects.
//!
//! Reading panes emit portable identities and geometry. This bridge resolves
//! them against one frozen project/session publication and returns either an
//! exact source citation, an existing comparison channel, or a semantic
//! selection. It owns no transport, player, device, renderer, or GPUI entity.
//! Unsupported frequency isolation and ambiguous identity mappings are
//! refused rather than approximated as an apparently successful audition.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use crate::air_query::workbench::{AuditionTarget, RevealTarget};
use crate::arrangement::{AudioLoopMode, ClipContent, PlaybackTransform};
use crate::assets::{AssetFrameRange, AssetId, SampleFrames};
use crate::comparison::{ComparisonDefinition, ComparisonId, SourceCitation};
use crate::comparison_controller::ComparisonChannel;
use crate::comparison_runtime::{ComparisonSourceResolver, PcmComparisonSourceResolver};
use crate::daw_project::ProjectRevisions;
use crate::daw_render::RenderCancellation;
use crate::interpretation::InterpretationStore;
use crate::interpretation_navigation::{
    AspectGeometryDto, EntityRefDto, RegionDto, SignalLayerDto,
};
use crate::ontology::{self, HypothesisClaim, ParameterOwner};
use crate::project_controller::ObjectRef;
use crate::project_selection::{
    AirSelection, ProjectSelection, SelectionGuard, SelectionProvenance, SelectionSource,
};
use crate::project_session::{ProjectSession, ProjectSessionError};
use crate::render_plan::{RenderFormat, RenderSpan};
use crate::render_runtime::{
    canonical_pcm_digest, AuditionMix, AuditionOwner, AuditionSubject, TimelineAudition,
    TimelineAuditionId,
};
use crate::workspace_document::WorkspaceViewId;

pub const READING_AUDITION_OWNER_NAMESPACE: u128 = u128::from_be_bytes(*b"audec-reading-v1");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadingEffectPin {
    pub document_generation: u64,
    pub revisions: ProjectRevisions,
}

#[derive(Clone, Debug)]
pub struct ReadingEffectSnapshot {
    pin: ReadingEffectPin,
    project: crate::live_project::LiveProjectSnapshot,
    interpretations: InterpretationStore,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadingSourceAuditionPlan {
    pub pin: ReadingEffectPin,
    pub generation: u64,
    pub entity: EntityRefDto,
    pub citation: SourceCitation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadingComparisonAuditionPlan {
    pub pin: ReadingEffectPin,
    pub entity: EntityRefDto,
    pub comparison: ComparisonId,
    pub channel: ComparisonChannel,
    /// The comparison product may cover a larger experiment. This is the
    /// reading row's exact requested focus for transport location/selection.
    pub focus: RenderSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadingAuditionPlan {
    Source(ReadingSourceAuditionPlan),
    Comparison(ReadingComparisonAuditionPlan),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadingRevealSubject {
    Object(ObjectRef),
    Air(AirSelection),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadingRevealPlan {
    pub pin: ReadingEffectPin,
    pub subject: ReadingRevealSubject,
    pub extent: Option<AspectGeometryDto>,
}

impl ReadingEffectSnapshot {
    pub fn capture(session: &ProjectSession) -> Result<Self, ReadingEffectBridgeError> {
        let project = session.project_snapshot()?.clone();
        Ok(Self {
            pin: ReadingEffectPin {
                document_generation: session.document_generation(),
                revisions: project.revisions(),
            },
            project,
            interpretations: session.deprojection_workspace_interpretations().clone(),
        })
    }

    pub const fn pin(&self) -> ReadingEffectPin {
        self.pin
    }

    pub fn plan_audition(
        &self,
        target: &AuditionTarget,
        generation: u64,
    ) -> Result<ReadingAuditionPlan, ReadingEffectBridgeError> {
        if generation == 0 {
            return Err(ReadingEffectBridgeError::ZeroGeneration);
        }
        target
            .entity
            .validate()
            .map_err(|_| ReadingEffectBridgeError::InvalidEntity(target.entity.clone()))?;
        let region = one_region(&target.extent)?;
        let focus = RenderSpan::new(region.start_frame, region.end_frame)
            .map_err(|error| ReadingEffectBridgeError::InvalidGeometry(error.to_string()))?;
        match &target.extent.signal {
            SignalLayerDto::Source => {
                let citation = self.source_citation(&target.entity, region)?;
                Ok(ReadingAuditionPlan::Source(ReadingSourceAuditionPlan {
                    pin: self.pin,
                    generation,
                    entity: target.entity.clone(),
                    citation,
                }))
            }
            SignalLayerDto::Explanation { reference } => {
                let comparison = self.comparison_for(reference, focus)?;
                Ok(ReadingAuditionPlan::Comparison(
                    ReadingComparisonAuditionPlan {
                        pin: self.pin,
                        entity: target.entity.clone(),
                        comparison,
                        channel: ComparisonChannel::Construction,
                        focus,
                    },
                ))
            }
            SignalLayerDto::Residual { reference } => {
                let comparison = self.comparison_for(reference, focus)?;
                Ok(ReadingAuditionPlan::Comparison(
                    ReadingComparisonAuditionPlan {
                        pin: self.pin,
                        entity: target.entity.clone(),
                        comparison,
                        channel: ComparisonChannel::Residual,
                        focus,
                    },
                ))
            }
        }
    }

    pub fn plan_reveal(
        &self,
        target: &RevealTarget,
    ) -> Result<ReadingRevealPlan, ReadingEffectBridgeError> {
        target
            .entity
            .validate()
            .map_err(|_| ReadingEffectBridgeError::InvalidEntity(target.entity.clone()))?;
        if let Some(extent) = &target.extent {
            for region in &extent.regions {
                region
                    .validate()
                    .map_err(|_| ReadingEffectBridgeError::InvalidRegion(*region))?;
            }
        }
        Ok(ReadingRevealPlan {
            pin: self.pin,
            subject: self.reveal_subject(&target.entity)?,
            extent: target.extent.clone(),
        })
    }

    pub fn render_source(
        &self,
        plan: &ReadingSourceAuditionPlan,
        owner: AuditionOwner,
        output: RenderFormat,
    ) -> Result<Arc<TimelineAudition>, ReadingEffectBridgeError> {
        self.require_pin(plan.pin)?;
        let source = PcmComparisonSourceResolver {
            assets: &self.project.project.state().domains.assets,
            pcm: &self.project.pcm,
        }
        .resolve_source(plan.citation, &RenderCancellation::new())
        .map_err(|error| ReadingEffectBridgeError::SourceRender(error.to_string()))?;
        if source.audio.format().sample_rate.get() != output.sample_rate.get() {
            return Err(ReadingEffectBridgeError::OutputFormat(format!(
                "source is {} Hz but the active renderer is {} Hz",
                source.audio.format().sample_rate.get(),
                output.sample_rate.get()
            )));
        }
        let samples = project_channels(
            source.audio.interleaved(),
            source.audio.format().channels.get(),
            output.channels.get(),
        )?;
        let samples: Arc<[f32]> = samples.into();
        let span = RenderSpan::new(
            plan.citation.project_span.start,
            plan.citation.project_span.end,
        )
        .map_err(|error| ReadingEffectBridgeError::InvalidGeometry(error.to_string()))?;
        Ok(Arc::new(
            TimelineAudition::new(
                TimelineAuditionId {
                    owner,
                    revision: plan.generation,
                    content: canonical_pcm_digest(&samples),
                },
                AuditionSubject::Source,
                AuditionMix::Replace,
                span,
                output,
                samples,
            )
            .map_err(|error| ReadingEffectBridgeError::SourceRender(error.to_string()))?,
        ))
    }

    pub fn comparison_definition(&self, id: ComparisonId) -> Option<&ComparisonDefinition> {
        self.interpretations.comparison(id)
    }

    pub fn comparison_observation(
        &self,
        id: ComparisonId,
    ) -> Option<&crate::comparison::ComparisonObservation> {
        self.interpretations.observation(id)
    }

    pub fn reveal_selection(
        &self,
        plan: &ReadingRevealPlan,
        guard: SelectionGuard,
        source_view: WorkspaceViewId,
    ) -> Result<ProjectSelection, ReadingEffectBridgeError> {
        self.require_pin(plan.pin)?;
        let mut selection = match &plan.subject {
            ReadingRevealSubject::Object(object) => {
                let mut selection =
                    ProjectSelection::from_reveal(object.clone(), [], guard, Some(source_view));
                selection.objects.provenance = SelectionProvenance {
                    source: SelectionSource::Reading,
                    source_view: Some(source_view),
                };
                selection
            }
            ReadingRevealSubject::Air(air) => {
                let mut selection = ProjectSelection::default();
                selection.air.insert(*air);
                selection
            }
        };
        if let Some(extent) = &plan.extent {
            // Project-local query geometry is lossless. Imported reading
            // geometry remains qualified and cannot be rebound without the
            // reading's exact source/material resolution, so identity reveal
            // still succeeds while its unproven geometry stays untouched.
            if geometry_is_project_local(extent) {
                selection.aspect = Some(
                    crate::air_query::workbench::compile_aspect(extent).map_err(|error| {
                        ReadingEffectBridgeError::InvalidGeometry(error.to_string())
                    })?,
                );
                selection.normalize_aspect_signal().map_err(|error| {
                    ReadingEffectBridgeError::InvalidGeometry(error.to_string())
                })?;
                selection.time = selection.timeline_span().map_err(|error| {
                    ReadingEffectBridgeError::InvalidGeometry(error.to_string())
                })?;
            }
        }
        Ok(selection)
    }

    fn require_pin(&self, pin: ReadingEffectPin) -> Result<(), ReadingEffectBridgeError> {
        if pin != self.pin {
            return Err(ReadingEffectBridgeError::StalePublication {
                expected: pin,
                actual: self.pin,
            });
        }
        Ok(())
    }

    fn comparison_for(
        &self,
        reference: &EntityRefDto,
        focus: RenderSpan,
    ) -> Result<ComparisonId, ReadingEffectBridgeError> {
        match reference {
            EntityRefDto::Project { kind, local_id } if kind == "comparison" => {
                let id = ComparisonId(*local_id);
                let definition = self
                    .interpretations
                    .comparison(id)
                    .ok_or(ReadingEffectBridgeError::MissingComparison(id))?;
                require_focus(definition, focus)?;
                Ok(id)
            }
            EntityRefDto::Project { kind, local_id } if kind == "explanation" => {
                let explanation = crate::explanation::ExplanationId(*local_id);
                let candidates = self
                    .interpretations
                    .comparisons()
                    .values()
                    .filter(|comparison| comparison.explanation == explanation)
                    .filter(|comparison| comparison_contains(comparison, focus))
                    .map(|comparison| comparison.id)
                    .collect::<Vec<_>>();
                match candidates.as_slice() {
                    [comparison] => Ok(*comparison),
                    [] => Err(ReadingEffectBridgeError::MissingExplanationComparison(
                        explanation,
                    )),
                    _ => Err(ReadingEffectBridgeError::AmbiguousExplanationComparison {
                        explanation,
                        candidates,
                    }),
                }
            }
            _ => Err(ReadingEffectBridgeError::UnsupportedSignalReference(
                reference.clone(),
            )),
        }
    }

    fn source_citation(
        &self,
        entity: &EntityRefDto,
        region: RegionDto,
    ) -> Result<SourceCitation, ReadingEffectBridgeError> {
        let focus = crate::aspect::FrameSpan::new(region.start_frame, region.end_frame)
            .ok_or(ReadingEffectBridgeError::InvalidRegion(region))?;
        if let EntityRefDto::Project { kind, local_id } = entity {
            if kind == "comparison" {
                let definition = self
                    .interpretations
                    .comparison(ComparisonId(*local_id))
                    .ok_or(ReadingEffectBridgeError::MissingComparison(ComparisonId(
                        *local_id,
                    )))?;
                return crop_citation(definition.source, focus, region.channels);
            }
            if kind == "explanation" {
                let comparison = self.comparison_for(
                    entity,
                    RenderSpan::new(focus.start, focus.end).map_err(|error| {
                        ReadingEffectBridgeError::InvalidGeometry(error.to_string())
                    })?,
                )?;
                return crop_citation(
                    self.interpretations
                        .comparison(comparison)
                        .expect("comparison was resolved from this store")
                        .source,
                    focus,
                    region.channels,
                );
            }
        }

        let state = self.project.project.state();
        let sources = entity_sources(entity, &state.domains.air)?;
        let source = exactly_one(sources, "AIR source")?;
        let assets = state
            .bindings
            .air
            .assets
            .iter()
            .filter_map(|(asset, candidate)| (*candidate == source).then_some(*asset))
            .collect::<Vec<_>>();
        let asset = exactly_one(assets, "media asset bound to the AIR source")?;
        let record = state
            .domains
            .assets
            .get(asset)
            .ok_or(ReadingEffectBridgeError::MissingAsset(asset))?;
        let nyquist = record.metadata().sample_rate_hz as f32 * 0.5;
        if region.min_hz() > 0.0 || region.max_hz() < nyquist {
            return Err(ReadingEffectBridgeError::FrequencyIsolationUnavailable {
                min_hz: region.min_hz(),
                max_hz: region.max_hz(),
                nyquist,
            });
        }
        let object = match entity {
            EntityRefDto::Project { kind, local_id } if kind == "air-object" => {
                Some(ontology::ObjectId::new(*local_id))
            }
            _ => None,
        };
        let aliases = state
            .bindings
            .assets
            .arrangement_assets
            .iter()
            .filter_map(|(alias, candidate)| (*candidate == asset).then_some(*alias))
            .collect::<BTreeSet<_>>();
        let candidates = state
            .domains
            .arrangement
            .clips
            .values()
            .filter(|clip| {
                object.is_none_or(|object| state.bindings.air.clips.get(&clip.id) == Some(&object))
            })
            .filter_map(|clip| match &clip.content {
                ClipContent::Audio(audio)
                    if aliases.contains(&audio.asset)
                        && clip.placement.start.get() <= focus.start
                        && focus.end <= clip.placement.end.get()
                        && audio.playback == PlaybackTransform::default()
                        && audio.loop_mode == AudioLoopMode::Off =>
                {
                    Some((clip, audio))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let (clip, audio) = exactly_one(candidates, "unity source placement")?;
        let offset = u64::try_from(focus.start - clip.placement.start.get())
            .map_err(|_| ReadingEffectBridgeError::SourceMapping("negative clip offset".into()))?;
        let source_start = audio.source.start.checked_add(offset).ok_or_else(|| {
            ReadingEffectBridgeError::SourceMapping("source offset overflow".into())
        })?;
        let frames = u64::try_from(focus.end - focus.start)
            .map_err(|_| ReadingEffectBridgeError::SourceMapping("source span overflow".into()))?;
        let source_end = source_start
            .checked_add(frames)
            .ok_or_else(|| ReadingEffectBridgeError::SourceMapping("source end overflow".into()))?;
        let source_range =
            AssetFrameRange::new(SampleFrames(source_start), SampleFrames(source_end))
                .map_err(|error| ReadingEffectBridgeError::SourceMapping(error.to_string()))?;
        Ok(SourceCitation {
            asset,
            source_range,
            project_span: focus,
            channels: crate::aspect::ChannelMask(region.channels),
        })
    }

    fn reveal_subject(
        &self,
        entity: &EntityRefDto,
    ) -> Result<ReadingRevealSubject, ReadingEffectBridgeError> {
        match entity {
            EntityRefDto::Project { kind, local_id } => match kind.as_str() {
                "comparison" => Ok(ReadingRevealSubject::Object(ObjectRef::Comparison(
                    ComparisonId(*local_id),
                ))),
                "explanation" => Ok(ReadingRevealSubject::Object(ObjectRef::Explanation(
                    crate::explanation::ExplanationId(*local_id),
                ))),
                "air-object" => Ok(ReadingRevealSubject::Air(AirSelection::Object(
                    ontology::ObjectId::new(*local_id),
                ))),
                "air-hypothesis" => Ok(ReadingRevealSubject::Air(AirSelection::Hypothesis(
                    ontology::HypothesisId::new(*local_id),
                ))),
                "air-source" => {
                    let source = ontology::SourceId::new(*local_id);
                    let assets = self
                        .project
                        .project
                        .state()
                        .bindings
                        .air
                        .assets
                        .iter()
                        .filter_map(|(asset, candidate)| (*candidate == source).then_some(*asset))
                        .collect::<Vec<_>>();
                    Ok(ReadingRevealSubject::Object(ObjectRef::Material(
                        exactly_one(assets, "media asset bound to the AIR source")?,
                    )))
                }
                "air-parameter" => {
                    let parameter = self
                        .project
                        .project
                        .state()
                        .domains
                        .air
                        .parameters
                        .get(&ontology::ParameterId::new(*local_id))
                        .ok_or_else(|| {
                            ReadingEffectBridgeError::UnresolvedEntity(entity.clone())
                        })?;
                    let object = match parameter.owner {
                        ParameterOwner::Object(object) => object,
                        ParameterOwner::Transform(transform) => self
                            .project
                            .project
                            .state()
                            .domains
                            .air
                            .transforms
                            .get(&transform)
                            .map(|transform| transform.owner)
                            .ok_or_else(|| {
                                ReadingEffectBridgeError::UnresolvedEntity(entity.clone())
                            })?,
                    };
                    Ok(ReadingRevealSubject::Air(AirSelection::Object(object)))
                }
                _ => Err(ReadingEffectBridgeError::UnresolvedEntity(entity.clone())),
            },
            EntityRefDto::Reading(foreign) => {
                let note = format!(
                    "foreign:{}:{}:{}",
                    foreign.reading, foreign.kind, foreign.local_id
                );
                let matches = self
                    .project
                    .project
                    .state()
                    .domains
                    .air
                    .hypotheses
                    .values()
                    .filter(|hypothesis| hypothesis.provenance.note.as_deref() == Some(&note))
                    .map(|hypothesis| hypothesis.id)
                    .collect::<Vec<_>>();
                Ok(ReadingRevealSubject::Air(AirSelection::Hypothesis(
                    exactly_one(matches, "imported reading hypothesis")?,
                )))
            }
        }
    }
}

pub const fn reading_audition_owner(view: WorkspaceViewId) -> AuditionOwner {
    AuditionOwner {
        namespace: READING_AUDITION_OWNER_NAMESPACE,
        local: view.0,
    }
}

fn one_region(extent: &AspectGeometryDto) -> Result<RegionDto, ReadingEffectBridgeError> {
    match extent.regions.as_slice() {
        [region] => {
            region
                .validate()
                .map_err(|_| ReadingEffectBridgeError::InvalidRegion(*region))?;
            Ok(*region)
        }
        regions => Err(ReadingEffectBridgeError::NonContiguousGeometry(
            regions.len(),
        )),
    }
}

fn crop_citation(
    citation: SourceCitation,
    focus: crate::aspect::FrameSpan,
    channels: u16,
) -> Result<SourceCitation, ReadingEffectBridgeError> {
    if citation.project_span.start > focus.start || focus.end > citation.project_span.end {
        return Err(ReadingEffectBridgeError::FocusOutsideComparison {
            focus,
            comparison: citation.project_span,
        });
    }
    let source_frames = citation.source_range.len().0;
    let project_frames = u64::try_from(citation.project_span.end - citation.project_span.start)
        .map_err(|_| ReadingEffectBridgeError::SourceMapping("comparison span overflow".into()))?;
    if source_frames != project_frames {
        return Err(ReadingEffectBridgeError::SourceMapping(
            "comparison source requires resampling".into(),
        ));
    }
    let start_offset = u64::try_from(focus.start - citation.project_span.start).map_err(|_| {
        ReadingEffectBridgeError::SourceMapping("negative comparison offset".into())
    })?;
    let end_offset = u64::try_from(focus.end - citation.project_span.start).map_err(|_| {
        ReadingEffectBridgeError::SourceMapping("comparison offset overflow".into())
    })?;
    let start = citation
        .source_range
        .start
        .0
        .checked_add(start_offset)
        .ok_or_else(|| ReadingEffectBridgeError::SourceMapping("source offset overflow".into()))?;
    let end = citation
        .source_range
        .start
        .0
        .checked_add(end_offset)
        .ok_or_else(|| ReadingEffectBridgeError::SourceMapping("source end overflow".into()))?;
    let selected = crate::aspect::ChannelMask(channels).intersect(citation.channels);
    if selected.is_empty() {
        return Err(ReadingEffectBridgeError::SourceMapping(
            "requested channels do not intersect the comparison source".into(),
        ));
    }
    Ok(SourceCitation {
        asset: citation.asset,
        source_range: AssetFrameRange::new(SampleFrames(start), SampleFrames(end))
            .map_err(|error| ReadingEffectBridgeError::SourceMapping(error.to_string()))?,
        project_span: focus,
        channels: selected,
    })
}

fn entity_sources(
    entity: &EntityRefDto,
    air: &ontology::AuditoryIr,
) -> Result<Vec<ontology::SourceId>, ReadingEffectBridgeError> {
    let mut objects = Vec::new();
    let mut direct = Vec::new();
    match entity {
        EntityRefDto::Project { kind, local_id } => match kind.as_str() {
            "air-source" => direct.push(ontology::SourceId::new(*local_id)),
            "air-object" => objects.push(ontology::ObjectId::new(*local_id)),
            "air-parameter" => {
                let parameter = air
                    .parameters
                    .get(&ontology::ParameterId::new(*local_id))
                    .ok_or_else(|| ReadingEffectBridgeError::UnresolvedEntity(entity.clone()))?;
                objects.push(match parameter.owner {
                    ParameterOwner::Object(object) => object,
                    ParameterOwner::Transform(transform) => air
                        .transforms
                        .get(&transform)
                        .map(|transform| transform.owner)
                        .ok_or_else(|| {
                            ReadingEffectBridgeError::UnresolvedEntity(entity.clone())
                        })?,
                });
            }
            "air-hypothesis" => {
                let hypothesis = air
                    .hypotheses
                    .get(&ontology::HypothesisId::new(*local_id))
                    .ok_or_else(|| ReadingEffectBridgeError::UnresolvedEntity(entity.clone()))?;
                for claim in &hypothesis.claims {
                    match claim {
                        HypothesisClaim::GroupsObjects(values)
                        | HypothesisClaim::SeparatesObjects(values)
                        | HypothesisClaim::FreeformPerceptualDescription {
                            objects: values, ..
                        } => objects.extend(values),
                        HypothesisClaim::PitchTrack { object, .. } => objects.push(*object),
                        HypothesisClaim::TransformApplies(transform) => {
                            if let Some(transform) = air.transforms.get(transform) {
                                objects.push(transform.owner);
                            }
                        }
                        HypothesisClaim::Relation(relation) => {
                            if let Some(relation) = air.relations.get(relation) {
                                objects.extend([relation.from, relation.to]);
                            }
                        }
                    }
                }
            }
            _ => return Err(ReadingEffectBridgeError::UnresolvedEntity(entity.clone())),
        },
        EntityRefDto::Reading(_) => {
            return Err(ReadingEffectBridgeError::UnresolvedEntity(entity.clone()))
        }
    }
    let mut visited = BTreeSet::new();
    while let Some(object) = objects.pop() {
        if !visited.insert(object) {
            continue;
        }
        let object = air
            .objects
            .get(&object)
            .ok_or_else(|| ReadingEffectBridgeError::UnresolvedEntity(entity.clone()))?;
        objects.extend(object.kind.members());
        direct.extend(
            object
                .source_anchors
                .iter()
                .filter_map(|anchor| air.spans.get(&anchor.span))
                .map(|span| span.source),
        );
    }
    direct.sort();
    direct.dedup();
    Ok(direct)
}

fn exactly_one<T>(mut values: Vec<T>, what: &'static str) -> Result<T, ReadingEffectBridgeError> {
    match values.len() {
        1 => Ok(values.remove(0)),
        count => Err(ReadingEffectBridgeError::AmbiguousResolution { what, count }),
    }
}

fn require_focus(
    definition: &ComparisonDefinition,
    focus: RenderSpan,
) -> Result<(), ReadingEffectBridgeError> {
    if comparison_contains(definition, focus) {
        Ok(())
    } else {
        Err(ReadingEffectBridgeError::FocusOutsideComparison {
            focus: crate::aspect::FrameSpan {
                start: focus.start,
                end: focus.end,
            },
            comparison: definition.source.project_span,
        })
    }
}

fn comparison_contains(definition: &ComparisonDefinition, focus: RenderSpan) -> bool {
    definition.source.project_span.start <= focus.start
        && focus.end <= definition.source.project_span.end
}

fn project_channels(
    input: &[f32],
    input_channels: u16,
    output_channels: u16,
) -> Result<Vec<f32>, ReadingEffectBridgeError> {
    match (input_channels, output_channels) {
        (input_channels, output_channels) if input_channels == output_channels => {
            Ok(input.to_vec())
        }
        (1, 2) => Ok(input.iter().flat_map(|sample| [*sample, *sample]).collect()),
        (2, 1) => Ok(input
            .chunks_exact(2)
            .map(|frame| (frame[0] + frame[1]) * 0.5)
            .collect()),
        _ => Err(ReadingEffectBridgeError::OutputFormat(format!(
            "cannot project {input_channels} source channels into {output_channels} renderer channels"
        ))),
    }
}

fn geometry_is_project_local(extent: &AspectGeometryDto) -> bool {
    extent
        .objects
        .iter()
        .all(|entity| matches!(entity, EntityRefDto::Project { .. }))
        && match &extent.signal {
            SignalLayerDto::Source => true,
            SignalLayerDto::Explanation { reference } | SignalLayerDto::Residual { reference } => {
                matches!(reference, EntityRefDto::Project { .. })
            }
        }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ReadingEffectBridgeError {
    Session(String),
    ZeroGeneration,
    InvalidEntity(EntityRefDto),
    InvalidRegion(RegionDto),
    InvalidGeometry(String),
    NonContiguousGeometry(usize),
    UnresolvedEntity(EntityRefDto),
    UnsupportedSignalReference(EntityRefDto),
    MissingComparison(ComparisonId),
    MissingExplanationComparison(crate::explanation::ExplanationId),
    AmbiguousExplanationComparison {
        explanation: crate::explanation::ExplanationId,
        candidates: Vec<ComparisonId>,
    },
    FocusOutsideComparison {
        focus: crate::aspect::FrameSpan,
        comparison: crate::aspect::FrameSpan,
    },
    MissingAsset(AssetId),
    AmbiguousResolution {
        what: &'static str,
        count: usize,
    },
    FrequencyIsolationUnavailable {
        min_hz: f32,
        max_hz: f32,
        nyquist: f32,
    },
    SourceMapping(String),
    SourceRender(String),
    OutputFormat(String),
    StalePublication {
        expected: ReadingEffectPin,
        actual: ReadingEffectPin,
    },
}

impl fmt::Display for ReadingEffectBridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(error) => formatter.write_str(error),
            Self::ZeroGeneration => formatter.write_str("reading audition generation is zero"),
            Self::InvalidEntity(entity) => write!(formatter, "invalid reading entity {entity:?}"),
            Self::InvalidRegion(region) => write!(formatter, "invalid audition region {region:?}"),
            Self::InvalidGeometry(error) => write!(formatter, "invalid reading geometry: {error}"),
            Self::NonContiguousGeometry(count) => write!(formatter, "reading audition has {count} regions; a contiguous render target is required"),
            Self::UnresolvedEntity(entity) => write!(formatter, "reading entity {entity:?} has no exact local identity"),
            Self::UnsupportedSignalReference(entity) => write!(formatter, "signal reference {entity:?} does not resolve to a persistent comparison"),
            Self::MissingComparison(id) => write!(formatter, "comparison {} is not retained", id.0),
            Self::MissingExplanationComparison(id) => write!(formatter, "explanation {} has no comparison covering this extent", id.0),
            Self::AmbiguousExplanationComparison { explanation, candidates } => write!(formatter, "explanation {} has {} comparisons covering this extent", explanation.0, candidates.len()),
            Self::FocusOutsideComparison { focus, comparison } => write!(formatter, "reading focus {}..{} is outside comparison {}..{}", focus.start, focus.end, comparison.start, comparison.end),
            Self::MissingAsset(asset) => write!(formatter, "source asset {} is missing", asset.0),
            Self::AmbiguousResolution { what, count } => write!(formatter, "expected one {what}, found {count}"),
            Self::FrequencyIsolationUnavailable { min_hz, max_hz, nyquist } => write!(formatter, "{min_hz:.1}–{max_hz:.1} Hz requests frequency isolation, but source audition currently requires the full 0–{nyquist:.1} Hz band"),
            Self::SourceMapping(error) => write!(formatter, "source mapping failed: {error}"),
            Self::SourceRender(error) => write!(formatter, "source audition render failed: {error}"),
            Self::OutputFormat(error) => write!(formatter, "source audition format failed: {error}"),
            Self::StalePublication { expected, actual } => write!(formatter, "reading effect was captured at document {}/revision {}, current is document {}/revision {}", expected.document_generation, expected.revisions.aggregate, actual.document_generation, actual.revisions.aggregate),
        }
    }
}

impl std::error::Error for ReadingEffectBridgeError {}

impl From<ProjectSessionError> for ReadingEffectBridgeError {
    fn from(error: ProjectSessionError) -> Self {
        Self::Session(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use crate::analysis::{Analysis, RhythmAnalysis};
    use crate::aspect::{Aspect, ChannelMask};
    use crate::assets::{
        AbsolutePath, AssetLocation, AssetOrigin, AssetProvenance, AssetRegistration,
        ContentFingerprint, DecodedAudioMetadata,
    };
    use crate::audio::AudioFormat;
    use crate::explanation::{ExplanationDefinition, ExplanationId, ExplanationScope};
    use crate::interpretation::{InterpretationCommand, InterpretationStore};
    use crate::live_project::{LiveProject, SourceMaterialMetadata};
    use crate::ontology::{Producer, Provenance};
    use crate::project_selection::SelectionDocumentId;
    use crate::pyramid::WaveformPyramid;

    struct Fixture {
        snapshot: ReadingEffectSnapshot,
        asset: AssetId,
        object: ontology::ObjectId,
        clip: crate::arrangement::ClipId,
    }

    fn fixture(interpretations: InterpretationStore) -> Fixture {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse("/audio/reading-source.wav").unwrap()),
            None,
        )
        .unwrap();
        let mut registry = crate::assets::AssetRegistry::new();
        let asset = registry
            .register(AssetRegistration {
                name: "reading source".into(),
                location: location.clone(),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: 48_000,
                    channels: 1,
                    frame_count: SampleFrames(4),
                    container: Some("wav".into()),
                    codec: Some("pcm_f32le".into()),
                    bit_depth: Some(32),
                },
                content: ContentFingerprint::from_bytes(b"reading bridge source"),
                provenance: AssetProvenance::new(
                    1,
                    AssetOrigin::ImportedFile {
                        importer: "test".into(),
                    },
                    location,
                ),
                tags: BTreeSet::new(),
                favorite: false,
            })
            .unwrap();
        let samples = Arc::<[f32]>::from([0.25, 0.5, 0.75, 1.0]);
        let pcm = crate::daw_render::PcmAsset::new(
            AudioFormat::new(48_000, 1).unwrap(),
            Arc::clone(&samples),
        )
        .unwrap();
        let analysis = Analysis {
            path: PathBuf::from("/audio/reading-source.wav"),
            title: "reading source".into(),
            album: "tests".into(),
            duration_seconds: 4.0 / 48_000.0,
            sample_rate: 48_000,
            channels: 1,
            bits_per_sample: 32,
            waveform: Vec::new(),
            waveform_pyramid: WaveformPyramid::from_interleaved(&samples, 1),
            mono_pcm: samples,
            features: Vec::new(),
            rhythm: RhythmAnalysis {
                tempo_bpm: 120.0,
                pulse_contrast: 0.0,
                beat_times: Vec::new(),
                onsets: Vec::new(),
                event_clusters: Vec::new(),
            },
            components: None,
            spectral_db: Vec::new(),
            spectral_peak_db: -3.0,
            spectrogram_png: Vec::new(),
        };
        let live = LiveProject::from_analyzed_source_material(
            SourceMaterialMetadata::new("reading", "reading source"),
            registry,
            asset,
            pcm,
            &analysis,
        )
        .unwrap();
        let source_ids = live.source_ids();
        let project = live.snapshot().unwrap();
        let object = *project
            .project
            .state()
            .bindings
            .air
            .clips
            .get(&source_ids.clip)
            .unwrap();
        let pin = ReadingEffectPin {
            document_generation: 1,
            revisions: project.revisions(),
        };
        Fixture {
            snapshot: ReadingEffectSnapshot {
                pin,
                project,
                interpretations,
            },
            asset,
            object,
            clip: source_ids.clip,
        }
    }

    fn provenance() -> Provenance {
        Provenance {
            producer: Producer::Human { name: None },
            created_unix_ms: None,
            source_revision: None,
            note: None,
        }
    }

    fn entity(kind: &str, local_id: u64) -> EntityRefDto {
        EntityRefDto::Project {
            kind: kind.into(),
            local_id,
        }
    }

    fn geometry(
        start_frame: i64,
        end_frame: i64,
        min_hz: f32,
        max_hz: f32,
        signal: SignalLayerDto,
    ) -> AspectGeometryDto {
        AspectGeometryDto {
            regions: vec![RegionDto {
                start_frame,
                end_frame,
                min_hz_bits: min_hz.to_bits(),
                max_hz_bits: max_hz.to_bits(),
                channels: 1,
            }],
            objects: Vec::new(),
            signal,
        }
    }

    fn comparison_store(
        clip: crate::arrangement::ClipId,
        asset: AssetId,
        comparisons: usize,
    ) -> InterpretationStore {
        let explanation = ExplanationDefinition {
            id: ExplanationId(1),
            label: "retained explanation".into(),
            scope: ExplanationScope::ArrangementClip(clip),
            extent: Aspect::All,
            evidence: Vec::new(),
            provenance: provenance(),
        };
        let mut commands = vec![InterpretationCommand::PutExplanation {
            before: None,
            after: Some(explanation),
        }];
        commands.extend(
            (0..comparisons).map(|index| InterpretationCommand::PutComparison {
                before: None,
                after: Some(ComparisonDefinition {
                    id: ComparisonId(index as u64 + 1),
                    label: format!("comparison {}", index + 1),
                    source: SourceCitation {
                        asset,
                        source_range: AssetFrameRange::new(SampleFrames(0), SampleFrames(4))
                            .unwrap(),
                        project_span: crate::aspect::FrameSpan { start: 0, end: 4 },
                        channels: ChannelMask(1),
                    },
                    explanation: ExplanationId(1),
                    provenance: provenance(),
                }),
            }),
        );
        let mut store = InterpretationStore::new();
        store.apply(&commands).unwrap();
        store
    }

    #[test]
    fn source_query_resolves_exact_frames_and_projects_into_shared_renderer_format() {
        let fixture = fixture(InterpretationStore::new());
        let object = entity("air-object", fixture.object.get());
        let target = AuditionTarget {
            entity: object.clone(),
            extent: geometry(1, 3, 0.0, 24_000.0, SignalLayerDto::Source),
        };
        let plan = match fixture.snapshot.plan_audition(&target, 7).unwrap() {
            ReadingAuditionPlan::Source(plan) => plan,
            ReadingAuditionPlan::Comparison(_) => panic!("source query planned a comparison"),
        };
        assert_eq!(plan.entity, object);
        assert_eq!(plan.citation.asset, fixture.asset);
        assert_eq!(plan.citation.source_range.start, SampleFrames(1));
        assert_eq!(plan.citation.source_range.end, SampleFrames(3));
        assert_eq!(plan.citation.project_span.start, 1);
        assert_eq!(plan.citation.project_span.end, 3);

        let owner = reading_audition_owner(WorkspaceViewId(23));
        let audition = fixture
            .snapshot
            .render_source(&plan, owner, RenderFormat::new(48_000, 2).unwrap())
            .unwrap();
        assert_eq!(audition.id.owner, owner);
        assert_eq!(audition.id.revision, 7);
        assert_eq!(audition.subject, AuditionSubject::Source);
        assert_eq!(audition.mix, AuditionMix::Replace);
        assert_eq!(audition.span, RenderSpan::new(1, 3).unwrap());
        assert_eq!(audition.interleaved(), &[0.5, 0.5, 0.75, 0.75]);
    }

    #[test]
    fn band_limited_source_query_is_refused_instead_of_auditioning_full_band() {
        let fixture = fixture(InterpretationStore::new());
        let target = AuditionTarget {
            entity: entity("air-object", fixture.object.get()),
            extent: geometry(0, 4, 200.0, 4_000.0, SignalLayerDto::Source),
        };
        assert!(matches!(
            fixture.snapshot.plan_audition(&target, 1),
            Err(ReadingEffectBridgeError::FrequencyIsolationUnavailable {
                min_hz,
                max_hz,
                nyquist,
            }) if min_hz == 200.0 && max_hz == 4_000.0 && nyquist == 24_000.0
        ));
    }

    #[test]
    fn residual_query_selects_the_retained_comparison_channel_and_focus() {
        let base = fixture(InterpretationStore::new());
        let store = comparison_store(base.clip, base.asset, 1);
        let fixture = fixture(store);
        let reference = entity("comparison", 1);
        let target = AuditionTarget {
            entity: entity("air-object", fixture.object.get()),
            extent: geometry(
                1,
                3,
                0.0,
                24_000.0,
                SignalLayerDto::Residual {
                    reference: reference.clone(),
                },
            ),
        };
        assert_eq!(
            fixture.snapshot.plan_audition(&target, 9).unwrap(),
            ReadingAuditionPlan::Comparison(ReadingComparisonAuditionPlan {
                pin: fixture.snapshot.pin(),
                entity: target.entity,
                comparison: ComparisonId(1),
                channel: ComparisonChannel::Residual,
                focus: RenderSpan::new(1, 3).unwrap(),
            })
        );
    }

    #[test]
    fn explanation_query_refuses_ambiguous_covering_comparisons() {
        let base = fixture(InterpretationStore::new());
        let store = comparison_store(base.clip, base.asset, 2);
        let fixture = fixture(store);
        let explanation = entity("explanation", 1);
        let target = AuditionTarget {
            entity: entity("air-object", fixture.object.get()),
            extent: geometry(
                0,
                4,
                0.0,
                24_000.0,
                SignalLayerDto::Explanation {
                    reference: explanation,
                },
            ),
        };
        assert!(matches!(
            fixture.snapshot.plan_audition(&target, 1),
            Err(ReadingEffectBridgeError::AmbiguousExplanationComparison {
                explanation: ExplanationId(1),
                candidates,
            }) if candidates == vec![ComparisonId(1), ComparisonId(2)]
        ));
    }

    #[test]
    fn stale_plan_cannot_render_and_reveal_retains_air_identity_and_geometry() {
        let fixture = fixture(InterpretationStore::new());
        let object = entity("air-object", fixture.object.get());
        let geometry = geometry(1, 3, 0.0, 24_000.0, SignalLayerDto::Source);
        let target = AuditionTarget {
            entity: object.clone(),
            extent: geometry.clone(),
        };
        let mut source = match fixture.snapshot.plan_audition(&target, 2).unwrap() {
            ReadingAuditionPlan::Source(plan) => plan,
            ReadingAuditionPlan::Comparison(_) => panic!("source query planned a comparison"),
        };
        source.pin.document_generation += 1;
        assert!(matches!(
            fixture.snapshot.render_source(
                &source,
                reading_audition_owner(WorkspaceViewId(4)),
                RenderFormat::new(48_000, 1).unwrap(),
            ),
            Err(ReadingEffectBridgeError::StalePublication { .. })
        ));

        let reveal = fixture
            .snapshot
            .plan_reveal(&RevealTarget {
                entity: object,
                extent: Some(geometry),
            })
            .unwrap();
        let selection = fixture
            .snapshot
            .reveal_selection(
                &reveal,
                SelectionGuard {
                    document: SelectionDocumentId(8),
                    project_revision: fixture.snapshot.pin().revisions.aggregate,
                },
                WorkspaceViewId(4),
            )
            .unwrap();
        assert_eq!(
            selection.air,
            BTreeSet::from([AirSelection::Object(fixture.object)])
        );
        assert_eq!(selection.time, crate::aspect::FrameSpan::new(1, 3));
        assert!(selection.aspect.is_some());
    }
}
