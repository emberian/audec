//! Coherent project construction across the editor session and AIR graph.
//!
//! The session model owns arrangement identity (clips, events, and reusable
//! clusters), while [`AuditoryIr`] owns perceptual/analytic identity.  This
//! module deliberately does not force either ID space into the other.  A
//! compact [`ProjectIdentityMap`] is the sole typed association between them.
//!
//! Importing an [`Analysis`] is intentionally conservative: onset clusters,
//! beat grids, and NMF factors remain mixed-audio hypotheses.  No instrument
//! or physical-source identity is inferred here.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::analysis::Analysis;
use crate::ontology::{
    Articulation, AudioSource, AuditoryIr, AuditoryObject, ChannelSelection, ComponentDomain,
    Evidence, EvidenceId, EvidenceKind, GroupingBasis, Hypothesis, HypothesisClaim, HypothesisId,
    InsertError, MeasurementValue, ObjectId, ObjectKind, Producer, Provenance,
    SampleRange as SourceSampleRange, SourceAnchor, SourceId, SourceSpan, SpanId, TimelineRange,
    ValidationIssue,
};
use crate::session::{
    ClipId, ClusterId, EventId, LaneId, Sample, SampleRange, Session, SessionError, TrackId,
    TrackKind,
};

const ANALYZER_NAME: &str = "audec deterministic analysis";
const ANALYZER_CONFIGURATION: &str =
    "rhythm:100hz-median-mad+fft4096-log40-kmeans14;nmf:r6-i60-l1=0.004";
const DEFAULT_HISTORY_LIMIT: usize = 256;
const ONSET_FINGERPRINT_PRE_ROLL: u64 = 256;
const ONSET_FINGERPRINT_FRAMES: u64 = 4_096;

/// Stable editor entities created by [`ProjectDocument::from_analysis`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectLayout {
    pub source_track: TrackId,
    pub source_lane: LaneId,
    pub source_clip: ClipId,
    pub hypotheses_track: TrackId,
    pub onset_lane: LaneId,
    pub beat_lane: LaneId,
    pub component_lane: LaneId,
}

/// Typed cross-domain associations without duplicated reverse indexes.
///
/// Reverse lookups are methods rather than another stored map, so the project
/// cannot silently develop two contradictory identities for one entity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectIdentityMap {
    pub clip_objects: BTreeMap<ClipId, ObjectId>,
    pub event_objects: BTreeMap<EventId, ObjectId>,
    pub cluster_hypotheses: BTreeMap<ClusterId, HypothesisId>,
}

impl ProjectIdentityMap {
    pub fn object_for_clip(&self, clip: ClipId) -> Option<ObjectId> {
        self.clip_objects.get(&clip).copied()
    }

    pub fn object_for_event(&self, event: EventId) -> Option<ObjectId> {
        self.event_objects.get(&event).copied()
    }

    pub fn hypothesis_for_cluster(&self, cluster: ClusterId) -> Option<HypothesisId> {
        self.cluster_hypotheses.get(&cluster).copied()
    }

    pub fn clip_for_object(&self, object: ObjectId) -> Option<ClipId> {
        unique_reverse_lookup(&self.clip_objects, object)
    }

    pub fn event_for_object(&self, object: ObjectId) -> Option<EventId> {
        unique_reverse_lookup(&self.event_objects, object)
    }

    pub fn cluster_for_hypothesis(&self, hypothesis: HypothesisId) -> Option<ClusterId> {
        unique_reverse_lookup(&self.cluster_hypotheses, hypothesis)
    }
}

fn unique_reverse_lookup<K, V>(map: &BTreeMap<K, V>, value: V) -> Option<K>
where
    K: Copy + Ord,
    V: Copy + Eq,
{
    let mut matches = map
        .iter()
        .filter_map(|(candidate, mapped)| (*mapped == value).then_some(*candidate));
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitationScope {
    SourceMix,
    Rhythm,
    OnsetClustering,
    ComponentDecomposition,
}

/// A user-presentable boundary on what the imported analysis establishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpistemicLimitation {
    pub scope: LimitationScope,
    pub summary: String,
}

/// One validated reverse-DAW document.
#[derive(Clone, Debug)]
pub struct ProjectDocument {
    pub session: Session,
    pub air: AuditoryIr,
    pub identities: ProjectIdentityMap,
    pub layout: ProjectLayout,
    pub limitations: Vec<EpistemicLimitation>,
}

impl ProjectDocument {
    /// Seeds an editable document from the exact retained PCM extent and the
    /// deterministic mixed-audio claims already present in `analysis`.
    pub fn from_analysis(analysis: &Analysis) -> Result<Self, ProjectError> {
        validate_analysis_shape(analysis)?;

        let frame_count = analysis.waveform_pyramid.frame_count() as u64;
        let channels = u16::try_from(analysis.channels)
            .map_err(|_| ProjectError::InvalidAnalysis("channel count exceeds u16"))?;
        let mut session = Session::new(analysis.sample_rate)?;
        // Imported entities are the clean baseline, not undoable user edits.
        session.set_history_limit(0);

        let source_track = session.create_track("Source audio", TrackKind::Audio)?;
        let source_lane = session.create_lane(source_track, "Decoded source mix")?;
        let source_clip = session.create_clip(
            source_lane,
            analysis.title.clone(),
            SampleRange::from_start_and_len(Sample::ZERO, frame_count),
        )?;

        let hypotheses_track = session.create_track("Analysis hypotheses", TrackKind::Events)?;
        let onset_lane = session.create_lane(hypotheses_track, "Inferred onsets")?;
        let beat_lane = session.create_lane(hypotheses_track, "Candidate beat grid")?;
        let component_lane =
            session.create_lane(hypotheses_track, "Mixed-audio recurrence components")?;

        let layout = ProjectLayout {
            source_track,
            source_lane,
            source_clip,
            hypotheses_track,
            onset_lane,
            beat_lane,
            component_lane,
        };
        let mut air = AuditoryIr::new(analysis.sample_rate);
        let mut ids = AirIdAllocator::default();
        let mut identities = ProjectIdentityMap::default();

        let source_id = ids.source();
        let full_span_id = ids.span();
        air.insert_source(AudioSource {
            id: source_id,
            uri: file_uri(&analysis.path),
            content_digest: None,
            sample_rate: analysis.sample_rate,
            channels,
            frame_count,
        })?;
        air.insert_span(SourceSpan {
            id: full_span_id,
            source: source_id,
            range: SourceSampleRange::new(0, frame_count)
                .ok_or(ProjectError::InvalidAnalysis("source audio is empty"))?,
            channels: ChannelSelection::All,
        })?;

        let source_evidence = ids.evidence();
        air.insert_evidence(Evidence {
            id: source_evidence,
            kind: EvidenceKind::SourceMeasurement {
                spans: vec![full_span_id],
                feature: "decoded source media properties".into(),
                value: MeasurementValue::Text(format!(
                    "{frame_count} frames, {} Hz, {channels} channels, {} bits/sample",
                    analysis.sample_rate, analysis.bits_per_sample
                )),
            },
            strength: 1.0,
            provenance: import_provenance(),
        })?;

        let source_object = ids.object();
        air.insert_object(AuditoryObject {
            id: source_object,
            label: "Decoded source mix".into(),
            kind: ObjectKind::Stream {
                basis: GroupingBasis::Joint,
                members: Vec::new(),
            },
            timeline: TimelineRange::new(0, frame_count),
            source_anchors: vec![SourceAnchor {
                span: full_span_id,
                object_offset_frames: 0,
                weight: 1.0,
            }],
            pitches: Vec::new(),
            evidence: vec![source_evidence],
            transform_chain: Vec::new(),
            tags: tags(["decoded", "mixed-audio", "source-backed"]),
            enabled: true,
        })?;
        insert_unique_link(
            &mut identities.clip_objects,
            source_clip,
            source_object,
            "source clip/object",
        )?;

        seed_rhythm(
            analysis,
            frame_count,
            source_id,
            full_span_id,
            onset_lane,
            beat_lane,
            &mut session,
            &mut air,
            &mut ids,
            &mut identities,
        )?;
        seed_components(
            analysis,
            frame_count,
            full_span_id,
            component_lane,
            &mut session,
            &mut air,
            &mut ids,
            &mut identities,
        )?;

        session.set_history_limit(DEFAULT_HISTORY_LIMIT);
        session.mark_saved();
        let document = Self {
            session,
            air,
            identities,
            layout,
            limitations: default_limitations(),
        };
        let issues = document.validate();
        if issues.is_empty() {
            Ok(document)
        } else {
            Err(ProjectError::InvalidProject(issues))
        }
    }

    /// Validates both documents and every cross-domain identity link.
    pub fn validate(&self) -> Vec<ProjectValidationIssue> {
        let mut issues: Vec<_> = self
            .air
            .validate()
            .into_iter()
            .map(ProjectValidationIssue::Air)
            .collect();
        if let Err(error) = self.session.arrangement().validate() {
            issues.push(ProjectValidationIssue::Session(error.to_string()));
        }
        if self.session.sample_rate() != self.air.sample_rate {
            issues.push(ProjectValidationIssue::Bridge(
                "session and AIR sample rates differ".into(),
            ));
        }

        validate_link_map(
            &self.identities.clip_objects,
            |id| self.session.arrangement().clip(id).is_some(),
            |id| self.air.objects.contains_key(&id),
            "clip/object",
            &mut issues,
        );
        validate_link_map(
            &self.identities.event_objects,
            |id| self.session.arrangement().event(id).is_some(),
            |id| self.air.objects.contains_key(&id),
            "event/object",
            &mut issues,
        );
        validate_link_map(
            &self.identities.cluster_hypotheses,
            |id| self.session.arrangement().cluster(id).is_some(),
            |id| self.air.hypotheses.contains_key(&id),
            "cluster/hypothesis",
            &mut issues,
        );

        let source_clip = self.session.arrangement().clip(self.layout.source_clip);
        let source = self.air.sources.values().next();
        match (source_clip, source) {
            (Some(clip), Some(source))
                if clip.timeline.start == Sample::ZERO
                    && clip.timeline.len() == source.frame_count
                    && clip.source_start == 0 => {}
            _ => issues.push(ProjectValidationIssue::Bridge(
                "source clip does not cover the exact AIR source extent".into(),
            )),
        }
        issues
    }
}

#[allow(clippy::too_many_arguments)]
fn seed_rhythm(
    analysis: &Analysis,
    frame_count: u64,
    source_id: SourceId,
    full_span_id: SpanId,
    onset_lane: LaneId,
    beat_lane: LaneId,
    session: &mut Session,
    air: &mut AuditoryIr,
    ids: &mut AirIdAllocator,
    identities: &mut ProjectIdentityMap,
) -> Result<(), ProjectError> {
    let provenance = analysis_provenance(
        "Onsets are novelty peaks; clusters use cosine similarity of mixed-audio spectral fingerprints.",
    );
    let mut cluster_members = vec![Vec::new(); analysis.rhythm.event_clusters.len()];
    let mut cluster_evidence = vec![Vec::new(); analysis.rhythm.event_clusters.len()];
    let mut imported_onsets = Vec::with_capacity(analysis.rhythm.onsets.len());

    for (index, onset) in analysis.rhythm.onsets.iter().enumerate() {
        let Some(members) = cluster_members.get_mut(onset.cluster) else {
            return Err(ProjectError::InvalidAnalysis(
                "onset references a missing event cluster",
            ));
        };
        let sample = frame_at_seconds(onset.time_seconds, analysis.sample_rate, frame_count)?;
        let support_start = sample.saturating_sub(ONSET_FINGERPRINT_PRE_ROLL);
        let support_end = support_start
            .saturating_add(ONSET_FINGERPRINT_FRAMES)
            .min(frame_count)
            .max(sample.saturating_add(1).min(frame_count));
        let span_id = ids.span();
        air.insert_span(SourceSpan {
            id: span_id,
            source: source_id,
            range: SourceSampleRange::new(support_start, support_end).ok_or(
                ProjectError::InvalidAnalysis("onset support window is empty"),
            )?,
            channels: ChannelSelection::All,
        })?;

        let evidence_id = ids.evidence();
        air.insert_evidence(Evidence {
            id: evidence_id,
            kind: EvidenceKind::SourceMeasurement {
                spans: vec![span_id],
                feature: "mixed-audio onset novelty and 40-band fingerprint assignment".into(),
                value: MeasurementValue::Vector(vec![
                    checked_unit(onset.strength, "onset strength")?,
                    checked_unit(onset.low, "onset low-band share")?,
                    checked_unit(onset.mid, "onset mid-band share")?,
                    checked_unit(onset.high, "onset high-band share")?,
                    checked_unit(onset.template_similarity, "onset template similarity")?,
                ]),
            },
            strength: checked_unit(onset.strength, "onset strength")?,
            provenance: provenance.clone(),
        })?;

        let object_id = ids.object();
        air.insert_object(AuditoryObject {
            id: object_id,
            label: format!("Inferred onset {:03}", index + 1),
            kind: ObjectKind::Event {
                articulation: Articulation::Impulsive,
                onset_strength: Some(checked_unit(onset.strength, "onset strength")?),
            },
            timeline: TimelineRange::new(sample as i64, 1),
            source_anchors: vec![SourceAnchor {
                span: span_id,
                object_offset_frames: support_start as i64 - sample as i64,
                weight: 1.0,
            }],
            pitches: Vec::new(),
            evidence: vec![evidence_id],
            transform_chain: Vec::new(),
            tags: tags([
                "deterministic-analysis",
                "inferred-onset",
                "mixed-audio",
                "not-source-identity",
            ]),
            enabled: true,
        })?;

        let cluster_id = onset.cluster;
        members.push(object_id);
        cluster_evidence[cluster_id].push(evidence_id);
        imported_onsets.push((cluster_id, object_id, sample));
    }

    // Session IDs are allocator-owned; create clusters, then replay onset
    // objects in analysis order to preserve a deterministic editor layout.
    let mut session_clusters = Vec::with_capacity(analysis.rhythm.event_clusters.len());
    for (cluster_index, cluster) in analysis.rhythm.event_clusters.iter().enumerate() {
        let actual_count = cluster_members[cluster_index].len();
        if cluster.event_count != actual_count {
            return Err(ProjectError::InvalidAnalysis(
                "event cluster count disagrees with onset assignments",
            ));
        }
        validate_component_vector(&cluster.spectrum, "event cluster spectrum")?;
        let session_cluster = session.create_cluster(cluster.label.clone())?;
        session_clusters.push(session_cluster);

        let template_evidence = ids.evidence();
        air.insert_evidence(Evidence {
            id: template_evidence,
            kind: EvidenceKind::SourceMeasurement {
                spans: vec![full_span_id],
                feature: format!(
                    "cluster {cluster_index} normalized 40-band mixed-audio spectral centroid"
                ),
                value: MeasurementValue::Vector(cluster.spectrum.clone()),
            },
            strength: checked_unit(cluster.consistency, "cluster consistency")?,
            provenance: provenance.clone(),
        })?;

        let derived_evidence = ids.evidence();
        let mut premises = cluster_evidence[cluster_index].clone();
        premises.push(template_evidence);
        air.insert_evidence(Evidence {
            id: derived_evidence,
            kind: EvidenceKind::Derived {
                premises,
                method: "deterministic weighted k-means over onset fingerprints; the grouping does not establish a shared instrument or source".into(),
            },
            strength: checked_unit(cluster.consistency, "cluster consistency")?,
            provenance: provenance.clone(),
        })?;

        let hypothesis_id = ids.hypothesis();
        air.insert_hypothesis(Hypothesis {
            id: hypothesis_id,
            label: format!("{} recurrence grouping", cluster.label),
            claims: vec![
                HypothesisClaim::GroupsObjects(cluster_members[cluster_index].clone()),
                HypothesisClaim::FreeformPerceptualDescription {
                    objects: cluster_members[cluster_index].clone(),
                    description: format!(
                        "These mixed-audio attacks have similar spectral fingerprints (centroid approximately {:.1} Hz); this is a reusable recurrence hypothesis, not a source label.",
                        cluster.centroid_hz
                    ),
                },
            ],
            support: checked_unit(cluster.consistency, "cluster consistency")?,
            evidence: vec![derived_evidence],
            provenance: provenance.clone(),
        })?;
        insert_unique_link(
            &mut identities.cluster_hypotheses,
            session_cluster,
            hypothesis_id,
            "rhythm cluster/hypothesis",
        )?;
    }

    for (cluster_index, object_id, sample) in imported_onsets {
        let event_id = session.create_event(
            onset_lane,
            session_clusters[cluster_index],
            Sample::new(sample as i64),
        )?;
        session.edit_event(event_id, "Set imported onset extent", |event| {
            event.duration = 1;
        })?;
        insert_unique_link(
            &mut identities.event_objects,
            event_id,
            object_id,
            "onset event/object",
        )?;
    }

    seed_beats(
        analysis,
        frame_count,
        source_id,
        full_span_id,
        beat_lane,
        session,
        air,
        ids,
        identities,
    )
}

#[allow(clippy::too_many_arguments)]
fn seed_beats(
    analysis: &Analysis,
    frame_count: u64,
    source_id: SourceId,
    full_span_id: SpanId,
    beat_lane: LaneId,
    session: &mut Session,
    air: &mut AuditoryIr,
    ids: &mut AirIdAllocator,
    identities: &mut ProjectIdentityMap,
) -> Result<(), ProjectError> {
    if analysis.rhythm.beat_times.is_empty() {
        return Ok(());
    }
    let pulse_support = checked_unit(analysis.rhythm.pulse_contrast, "pulse contrast")?;
    if !analysis.rhythm.tempo_bpm.is_finite() || analysis.rhythm.tempo_bpm <= 0.0 {
        return Err(ProjectError::InvalidAnalysis(
            "beat grid has an invalid tempo",
        ));
    }
    let provenance = analysis_provenance(
        "Beat positions are the strongest tested autocorrelation lag/phase candidate, not ground truth.",
    );
    let pulse_evidence = ids.evidence();
    air.insert_evidence(Evidence {
        id: pulse_evidence,
        kind: EvidenceKind::SourceMeasurement {
            spans: vec![full_span_id],
            feature: "mixed-audio onset-envelope periodicity candidate".into(),
            value: MeasurementValue::Text(format!("{:.6} BPM", analysis.rhythm.tempo_bpm)),
        },
        strength: pulse_support,
        provenance: provenance.clone(),
    })?;

    let cluster_id = session.create_cluster(format!(
        "Candidate {:.1} BPM pulse",
        analysis.rhythm.tempo_bpm
    ))?;
    let mut members = Vec::with_capacity(analysis.rhythm.beat_times.len());
    for (index, time) in analysis.rhythm.beat_times.iter().copied().enumerate() {
        let sample = frame_at_seconds(time, analysis.sample_rate, frame_count)?;
        let span_id = ids.span();
        air.insert_span(SourceSpan {
            id: span_id,
            source: source_id,
            range: SourceSampleRange::new(sample, sample + 1)
                .ok_or(ProjectError::InvalidAnalysis("beat source span is empty"))?,
            channels: ChannelSelection::All,
        })?;
        let object_id = ids.object();
        air.insert_object(AuditoryObject {
            id: object_id,
            label: format!("Candidate beat {:03}", index + 1),
            kind: ObjectKind::Event {
                articulation: Articulation::Unknown,
                onset_strength: None,
            },
            timeline: TimelineRange::new(sample as i64, 1),
            source_anchors: vec![SourceAnchor {
                span: span_id,
                object_offset_frames: 0,
                weight: 1.0,
            }],
            pitches: Vec::new(),
            evidence: vec![pulse_evidence],
            transform_chain: Vec::new(),
            tags: tags(["candidate-beat", "deterministic-analysis", "mixed-audio"]),
            enabled: true,
        })?;
        members.push(object_id);
        let event_id = session.create_event(beat_lane, cluster_id, Sample::new(sample as i64))?;
        session.edit_event(event_id, "Set imported beat extent", |event| {
            event.duration = 1;
        })?;
        insert_unique_link(
            &mut identities.event_objects,
            event_id,
            object_id,
            "beat event/object",
        )?;
    }

    let hypothesis_id = ids.hypothesis();
    air.insert_hypothesis(Hypothesis {
        id: hypothesis_id,
        label: format!("Candidate {:.1} BPM beat grid", analysis.rhythm.tempo_bpm),
        claims: vec![
            HypothesisClaim::GroupsObjects(members.clone()),
            HypothesisClaim::FreeformPerceptualDescription {
                objects: members,
                description: "A periodic placement candidate derived from the mixed-audio onset envelope; phase and tempo remain editable hypotheses.".into(),
            },
        ],
        support: pulse_support,
        evidence: vec![pulse_evidence],
        provenance,
    })?;
    insert_unique_link(
        &mut identities.cluster_hypotheses,
        cluster_id,
        hypothesis_id,
        "beat cluster/hypothesis",
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn seed_components(
    analysis: &Analysis,
    frame_count: u64,
    full_span_id: SpanId,
    component_lane: LaneId,
    session: &mut Session,
    air: &mut AuditoryIr,
    ids: &mut AirIdAllocator,
    identities: &mut ProjectIdentityMap,
) -> Result<(), ProjectError> {
    let provenance = analysis_provenance(
        "NMF factors are rank-one patterns in a mixed magnitude field. They are neither stems nor source identities.",
    );
    for (index, component) in analysis.components.components.iter().enumerate() {
        if component.spectral_template.len() != analysis.components.frequency_bins
            || component.activation.len() != analysis.components.frames
        {
            return Err(ProjectError::InvalidAnalysis(
                "component factor shape disagrees with decomposition dimensions",
            ));
        }
        validate_component_vector(&component.spectral_template, "component spectral template")?;
        validate_component_vector(&component.activation, "component activation")?;
        let support = checked_unit(component.confidence, "component confidence")?;

        let template_evidence = ids.evidence();
        air.insert_evidence(Evidence {
            id: template_evidence,
            kind: EvidenceKind::SourceMeasurement {
                spans: vec![full_span_id],
                feature: format!("NMF component {index} L1-normalized spectral template"),
                value: MeasurementValue::Vector(component.spectral_template.clone()),
            },
            strength: support,
            provenance: provenance.clone(),
        })?;
        let activation_evidence = ids.evidence();
        air.insert_evidence(Evidence {
            id: activation_evidence,
            kind: EvidenceKind::SourceMeasurement {
                spans: vec![full_span_id],
                feature: format!(
                    "NMF component {index} nonnegative activation over {} analysis frames",
                    analysis.components.frames
                ),
                value: MeasurementValue::Vector(component.activation.clone()),
            },
            strength: support,
            provenance: provenance.clone(),
        })?;

        let object_id = ids.object();
        air.insert_object(AuditoryObject {
            id: object_id,
            label: format!("Mixed recurrence component {:02}", index + 1),
            kind: ObjectKind::Component {
                domain: ComponentDomain::MagnitudeSpectrum,
                index: index as u32,
                members: Vec::new(),
            },
            timeline: TimelineRange::new(0, frame_count),
            source_anchors: vec![SourceAnchor {
                span: full_span_id,
                object_offset_frames: 0,
                weight: 1.0,
            }],
            pitches: Vec::new(),
            evidence: vec![template_evidence, activation_evidence],
            transform_chain: Vec::new(),
            tags: tags([
                "latent-component",
                "mixed-audio",
                "not-a-stem",
                "not-source-identity",
            ]),
            enabled: true,
        })?;

        let cluster_id = session.create_cluster(format!("Component {:02}", index + 1))?;
        let event_id = session.create_event(component_lane, cluster_id, Sample::ZERO)?;
        session.edit_event(event_id, "Set component hypothesis extent", |event| {
            event.duration = frame_count;
        })?;
        insert_unique_link(
            &mut identities.event_objects,
            event_id,
            object_id,
            "component event/object",
        )?;

        let hypothesis_id = ids.hypothesis();
        air.insert_hypothesis(Hypothesis {
            id: hypothesis_id,
            label: format!("Mixed-audio recurrence component {:02}", index + 1),
            claims: vec![HypothesisClaim::FreeformPerceptualDescription {
                objects: vec![object_id],
                description: format!(
                    "A recurring rank-one magnitude pattern explains an estimated {:.1}% of summed independent component energy. Overlap means this is not an isolated stem.",
                    component.energy_share * 100.0
                ),
            }],
            support,
            evidence: vec![template_evidence, activation_evidence],
            provenance: provenance.clone(),
        })?;
        insert_unique_link(
            &mut identities.cluster_hypotheses,
            cluster_id,
            hypothesis_id,
            "component cluster/hypothesis",
        )?;
    }
    Ok(())
}

fn validate_analysis_shape(analysis: &Analysis) -> Result<(), ProjectError> {
    if analysis.sample_rate == 0 {
        return Err(ProjectError::InvalidAnalysis("sample rate is zero"));
    }
    if analysis.channels == 0 {
        return Err(ProjectError::InvalidAnalysis("channel count is zero"));
    }
    if analysis.waveform_pyramid.frame_count() == 0 {
        return Err(ProjectError::InvalidAnalysis("source audio is empty"));
    }
    if analysis.waveform_pyramid.channel_count() == 0 {
        return Err(ProjectError::InvalidAnalysis(
            "retained PCM has no channels",
        ));
    }
    if analysis.components.components.len() > u32::MAX as usize {
        return Err(ProjectError::InvalidAnalysis(
            "too many decomposition components",
        ));
    }
    Ok(())
}

fn frame_at_seconds(time: f64, sample_rate: u32, frame_count: u64) -> Result<u64, ProjectError> {
    if !time.is_finite() || time < 0.0 {
        return Err(ProjectError::InvalidAnalysis(
            "analysis time is negative or non-finite",
        ));
    }
    let frame = time * f64::from(sample_rate);
    if frame > u64::MAX as f64 {
        return Err(ProjectError::InvalidAnalysis(
            "analysis time exceeds frame coordinate range",
        ));
    }
    Ok((frame.round() as u64).min(frame_count.saturating_sub(1)))
}

fn checked_unit(value: f32, name: &'static str) -> Result<f32, ProjectError> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(ProjectError::InvalidAnalysis(name))
    }
}

fn validate_component_vector(values: &[f32], name: &'static str) -> Result<(), ProjectError> {
    if values
        .iter()
        .all(|value| value.is_finite() && *value >= 0.0)
    {
        Ok(())
    } else {
        Err(ProjectError::InvalidAnalysis(name))
    }
}

fn insert_unique_link<K, V>(
    links: &mut BTreeMap<K, V>,
    key: K,
    value: V,
    kind: &'static str,
) -> Result<(), ProjectError>
where
    K: Copy + Ord,
    V: Copy + Eq,
{
    if links.contains_key(&key) || links.values().any(|candidate| *candidate == value) {
        return Err(ProjectError::DuplicateIdentity(kind));
    }
    links.insert(key, value);
    Ok(())
}

fn validate_link_map<K, V, FK, FV>(
    links: &BTreeMap<K, V>,
    key_exists: FK,
    value_exists: FV,
    kind: &str,
    issues: &mut Vec<ProjectValidationIssue>,
) where
    K: Copy + Ord + fmt::Display,
    V: Copy + Ord + fmt::Display,
    FK: Fn(K) -> bool,
    FV: Fn(V) -> bool,
{
    let mut values = BTreeSet::new();
    for (key, value) in links {
        if !key_exists(*key) {
            issues.push(ProjectValidationIssue::Bridge(format!(
                "{kind} link references missing editor entity {key}"
            )));
        }
        if !value_exists(*value) {
            issues.push(ProjectValidationIssue::Bridge(format!(
                "{kind} link references missing semantic entity {value}"
            )));
        }
        if !values.insert(*value) {
            issues.push(ProjectValidationIssue::Bridge(format!(
                "{kind} link assigns semantic entity {value} more than once"
            )));
        }
    }
}

fn tags<const N: usize>(values: [&str; N]) -> BTreeSet<String> {
    values.into_iter().map(str::to_owned).collect()
}

fn file_uri(path: &std::path::Path) -> String {
    let path = path.to_string_lossy();
    let mut uri = String::from("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~' | b':') {
            uri.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    uri
}

fn import_provenance() -> Provenance {
    Provenance {
        producer: Producer::Importer {
            format: "decoded audio".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        created_unix_ms: None,
        source_revision: None,
        note: Some("PCM extent is taken from audec's retained waveform pyramid.".into()),
    }
}

fn analysis_provenance(note: &str) -> Provenance {
    Provenance {
        producer: Producer::Analyzer {
            name: ANALYZER_NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            configuration_digest: Some(ANALYZER_CONFIGURATION.into()),
        },
        created_unix_ms: None,
        source_revision: None,
        note: Some(note.into()),
    }
}

fn default_limitations() -> Vec<EpistemicLimitation> {
    vec![
        EpistemicLimitation {
            scope: LimitationScope::SourceMix,
            summary: "All deterministic measurements currently operate on the decoded mix; correlated production layers may remain entangled.".into(),
        },
        EpistemicLimitation {
            scope: LimitationScope::Rhythm,
            summary: "Tempo and beat phase are strongest tested periodicity candidates, not calibrated probabilities or immutable grid truth.".into(),
        },
        EpistemicLimitation {
            scope: LimitationScope::OnsetClustering,
            summary: "Onset clusters encode spectral-fingerprint recurrence. Membership does not establish that events share an instrument or physical source.".into(),
        },
        EpistemicLimitation {
            scope: LimitationScope::ComponentDecomposition,
            summary: "NMF factors are overlapping rank-one magnitude hypotheses. They are not phase-complete stems and need not correspond one-to-one with audible sources.".into(),
        },
    ]
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectValidationIssue {
    Air(ValidationIssue),
    Session(String),
    Bridge(String),
}

impl fmt::Display for ProjectValidationIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Air(issue) => write!(formatter, "AIR {}: {}", issue.path, issue.message),
            Self::Session(message) => write!(formatter, "session: {message}"),
            Self::Bridge(message) => write!(formatter, "bridge: {message}"),
        }
    }
}

#[derive(Debug)]
pub enum ProjectError {
    InvalidAnalysis(&'static str),
    DuplicateIdentity(&'static str),
    Session(SessionError),
    AirInsert(InsertError),
    InvalidProject(Vec<ProjectValidationIssue>),
}

impl fmt::Display for ProjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAnalysis(message) => write!(formatter, "invalid analysis: {message}"),
            Self::DuplicateIdentity(kind) => write!(formatter, "duplicate {kind} identity link"),
            Self::Session(error) => error.fmt(formatter),
            Self::AirInsert(error) => error.fmt(formatter),
            Self::InvalidProject(issues) => {
                write!(
                    formatter,
                    "constructed project has {} validation issues",
                    issues.len()
                )
            }
        }
    }
}

impl Error for ProjectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Session(error) => Some(error),
            Self::AirInsert(error) => Some(error),
            _ => None,
        }
    }
}

impl From<SessionError> for ProjectError {
    fn from(value: SessionError) -> Self {
        Self::Session(value)
    }
}

impl From<InsertError> for ProjectError {
    fn from(value: InsertError) -> Self {
        Self::AirInsert(value)
    }
}

#[derive(Default)]
struct AirIdAllocator {
    source: u64,
    span: u64,
    object: u64,
    evidence: u64,
    hypothesis: u64,
}

impl AirIdAllocator {
    fn source(&mut self) -> SourceId {
        self.source += 1;
        SourceId::new(self.source)
    }

    fn span(&mut self) -> SpanId {
        self.span += 1;
        SpanId::new(self.span)
    }

    fn object(&mut self) -> ObjectId {
        self.object += 1;
        ObjectId::new(self.object)
    }

    fn evidence(&mut self) -> EvidenceId {
        self.evidence += 1;
        EvidenceId::new(self.evidence)
    }

    fn hypothesis(&mut self) -> HypothesisId {
        self.hypothesis += 1;
        HypothesisId::new(self.hypothesis)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::analysis::{EventCluster, OnsetEvent, RhythmAnalysis};
    use crate::decomposition::{ComponentDecomposition, ComponentHypothesis};
    use crate::pyramid::WaveformPyramid;

    use super::*;

    fn analysis() -> Analysis {
        let pcm = vec![
            0.0, 0.0, 0.2, -0.2, 0.8, 0.7, 0.1, 0.0, -0.4, -0.3, 0.0, 0.0, 0.2, 0.3, 0.0, 0.0,
        ];
        Analysis {
            path: PathBuf::from("/tmp/Test Mix.flac"),
            title: "Test Mix".into(),
            album: "Tests".into(),
            duration_seconds: 1.0,
            sample_rate: 8,
            channels: 2,
            bits_per_sample: 24,
            waveform: Vec::new(),
            waveform_pyramid: WaveformPyramid::from_interleaved(&pcm, 2),
            mono_pcm: pcm
                .chunks_exact(2)
                .map(|frame| (frame[0] + frame[1]) * 0.5)
                .collect::<Vec<_>>()
                .into(),
            features: Vec::new(),
            rhythm: RhythmAnalysis {
                tempo_bpm: 120.0,
                pulse_contrast: 0.7,
                beat_times: vec![0.0, 0.5],
                onsets: vec![OnsetEvent {
                    time_seconds: 0.25,
                    strength: 0.9,
                    low: 0.2,
                    mid: 0.5,
                    high: 0.3,
                    cluster: 0,
                    template_similarity: 0.85,
                }],
                event_clusters: vec![EventCluster {
                    label: "bright recurrence A".into(),
                    event_count: 1,
                    centroid_hz: 2_400.0,
                    consistency: 0.85,
                    spectrum: vec![0.2, 0.8],
                }],
            },
            components: ComponentDecomposition {
                frequency_bins: 2,
                frames: 4,
                components: vec![ComponentHypothesis {
                    spectral_template: vec![0.3, 0.7],
                    activation: vec![0.1, 0.8, 0.2, 0.6],
                    energy_share: 0.6,
                    spectral_distinctness: 0.75,
                    confidence: 0.65,
                }],
                iterations_run: 10,
                reconstruction_rmse: 0.1,
                relative_error: 0.2,
                explained_energy: 0.96,
                confidence: 0.7,
                silent: false,
            },
            spectral_db: Vec::new(),
            spectral_peak_db: -3.0,
            spectrogram_png: Vec::new(),
        }
    }

    #[test]
    fn source_clip_and_air_span_cover_exact_retained_frames() {
        let document = ProjectDocument::from_analysis(&analysis()).unwrap();
        let clip = document
            .session
            .arrangement()
            .clip(document.layout.source_clip)
            .unwrap();
        assert_eq!(clip.timeline, SampleRange::new(0, 8));
        assert_eq!(clip.source_start, 0);
        assert_eq!(document.air.sources.len(), 1);
        let source = document.air.sources.values().next().unwrap();
        assert_eq!(source.frame_count, 8);
        let full_span = document
            .air
            .spans
            .values()
            .find(|span| span.range == SourceSampleRange { start: 0, end: 8 })
            .unwrap();
        assert_eq!(full_span.source, source.id);
        assert_eq!(
            document.identities.clip_for_object(
                document
                    .identities
                    .object_for_clip(document.layout.source_clip)
                    .unwrap()
            ),
            Some(document.layout.source_clip)
        );
        assert!(!document.session.is_dirty());
        assert!(!document.session.can_undo());
        assert!(document.validate().is_empty());
    }

    #[test]
    fn creates_typed_links_for_onsets_beats_and_components() {
        let document = ProjectDocument::from_analysis(&analysis()).unwrap();
        assert_eq!(document.identities.event_objects.len(), 4);
        assert_eq!(document.identities.cluster_hypotheses.len(), 3);
        for (event_id, object_id) in &document.identities.event_objects {
            let event = document.session.arrangement().event(*event_id).unwrap();
            let object = &document.air.objects[object_id];
            assert_eq!(event.sample.get(), object.timeline.start);
            assert_eq!(event.duration, object.timeline.duration);
            assert_eq!(
                document.identities.event_for_object(*object_id),
                Some(*event_id)
            );
            assert!(object.source_anchors.iter().all(|anchor| {
                let span = &document.air.spans[&anchor.span];
                span.range.contains(event.sample.get() as u64)
                    || object.timeline.duration == source_frame_count(&document)
            }));
        }
    }

    #[test]
    fn mixed_audio_claims_are_provenanced_and_explicitly_limited() {
        let document = ProjectDocument::from_analysis(&analysis()).unwrap();
        let component = document
            .air
            .objects
            .values()
            .find(|object| matches!(object.kind, ObjectKind::Component { .. }))
            .unwrap();
        assert!(component.tags.contains("not-a-stem"));
        let component_hypothesis = document
            .air
            .hypotheses
            .values()
            .find(|hypothesis| hypothesis.label.contains("recurrence component"))
            .unwrap();
        assert!(component_hypothesis.claims.iter().any(|claim| matches!(
            claim,
            HypothesisClaim::FreeformPerceptualDescription { description, .. }
                if description.contains("not an isolated stem")
        )));
        assert!(matches!(
            component_hypothesis.provenance.producer,
            Producer::Analyzer { ref configuration_digest, .. }
                if configuration_digest.as_deref() == Some(ANALYZER_CONFIGURATION)
        ));
        assert_eq!(document.limitations.len(), 4);
        assert!(document.limitations.iter().any(|limitation| {
            limitation.scope == LimitationScope::OnsetClustering
                && limitation.summary.contains("does not establish")
        }));
    }

    #[test]
    fn onset_backlink_records_the_exact_clipped_fingerprint_window() {
        let document = ProjectDocument::from_analysis(&analysis()).unwrap();
        let onset_object = document
            .air
            .objects
            .values()
            .find(|object| object.tags.contains("inferred-onset"))
            .unwrap();
        assert_eq!(onset_object.timeline.start, 2);
        assert_eq!(onset_object.timeline.duration, 1);
        let anchor = &onset_object.source_anchors[0];
        let span = &document.air.spans[&anchor.span];
        assert_eq!(span.range, SourceSampleRange { start: 0, end: 8 });
        assert_eq!(anchor.object_offset_frames, -2);
    }

    #[test]
    fn duplicate_semantic_identity_is_reported() {
        let mut document = ProjectDocument::from_analysis(&analysis()).unwrap();
        let (&first_event, &object) = document.identities.event_objects.iter().next().unwrap();
        let other_event = document
            .identities
            .event_objects
            .keys()
            .copied()
            .find(|event| *event != first_event)
            .unwrap();
        document
            .identities
            .event_objects
            .insert(other_event, object);
        assert!(document.validate().iter().any(|issue| matches!(
            issue,
            ProjectValidationIssue::Bridge(message)
                if message.contains("more than once")
        )));
        assert_eq!(document.identities.event_for_object(object), None);
    }

    #[test]
    fn rejects_empty_and_malformed_analysis() {
        let mut empty = analysis();
        empty.waveform_pyramid = WaveformPyramid::from_interleaved(&[], 2);
        assert!(matches!(
            ProjectDocument::from_analysis(&empty),
            Err(ProjectError::InvalidAnalysis("source audio is empty"))
        ));

        let mut malformed = analysis();
        malformed.rhythm.onsets[0].cluster = 99;
        assert!(matches!(
            ProjectDocument::from_analysis(&malformed),
            Err(ProjectError::InvalidAnalysis(
                "onset references a missing event cluster"
            ))
        ));
    }

    fn source_frame_count(document: &ProjectDocument) -> u64 {
        document.air.sources.values().next().unwrap().frame_count
    }
}
