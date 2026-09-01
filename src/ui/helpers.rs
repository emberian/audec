//! Selection, reveal, and audio-recipe helpers shared across the ui module.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;
pub(super) use crate::project_audio_controller::project_audio_snapshot_digest;

pub(super) fn arrangement_selection_from_project(
    selection: &ProjectSelection,
) -> ArrangementSelection {
    ArrangementSelection {
        clips: selection.clips.clone(),
        tracks: selection.tracks.clone(),
        time: selection.time.and_then(|range| {
            ArrangementFrameRange::new(
                ArrangementFrame::new(range.start),
                ArrangementFrame::new(range.end),
            )
            .ok()
        }),
    }
}

pub(super) fn apply_project_id_selection(
    current: &mut BTreeSet<crate::arrangement::ClipId>,
    incoming: BTreeSet<crate::arrangement::ClipId>,
    mode: SelectionMode,
) {
    match mode {
        SelectionMode::Replace => *current = incoming,
        SelectionMode::Add => current.extend(incoming),
        SelectionMode::Toggle => {
            for id in incoming {
                if !current.remove(&id) {
                    current.insert(id);
                }
            }
        }
    }
}

pub(super) fn selected_arrangement_frame_span(
    arrangement: &crate::arrangement::ArrangementState,
    clips: &BTreeSet<crate::arrangement::ClipId>,
) -> Option<FrameSpan> {
    let mut selected = clips
        .iter()
        .filter_map(|clip| arrangement.clip(*clip))
        .map(|clip| clip.placement);
    let first = selected.next()?;
    let (start, end) = selected.fold(
        (first.start.get(), first.end.get()),
        |(start, end), range| (start.min(range.start.get()), end.max(range.end.get())),
    );
    Some(FrameSpan { start, end })
}

pub(super) fn workspace_pattern_source(
    descriptor: &WorkspaceViewDescriptor,
    project: &LiveProjectSnapshot,
    preferred_occurrence: Option<crate::pattern_use_graph::PatternOccurrenceTarget>,
) -> SequencerEditorSource {
    let sequencer = project.project.state().domains.sequencer.clone();
    let requested = match descriptor.target {
        WorkspaceTarget::PatternDefinition { id } if id != 0 => {
            Some(crate::sequencer::PatternId::from_raw(id))
        }
        _ => None,
    };
    let selected = requested
        .filter(|id| sequencer.patterns().get(*id).is_some())
        .or_else(|| {
            sequencer
                .patterns()
                .patterns()
                .next()
                .map(|pattern| pattern.id)
        });
    let Some(pattern) = selected else {
        return SequencerEditorSource::new(
            Arc::new(Mutex::new(sequencer)),
            None,
            None,
            workspace_view_title(descriptor),
        );
    };
    let mode = match sequencer
        .patterns()
        .get(pattern)
        .map(|pattern| &pattern.content)
    {
        Some(PatternContent::Notes(_)) => PatternEditorMode::PianoRoll,
        Some(PatternContent::Steps(_)) => PatternEditorMode::Steps,
        None => {
            return SequencerEditorSource::new(
                Arc::new(Mutex::new(sequencer)),
                None,
                None,
                workspace_view_title(descriptor),
            );
        }
    };
    hydrated_pattern_source(
        project,
        sequencer,
        PatternEditorTarget::new(pattern, mode),
        preferred_occurrence,
        workspace_view_title(descriptor),
    )
}

pub(super) fn hydrated_pattern_source(
    project: &LiveProjectSnapshot,
    sequencer: crate::sequencer::Sequencer,
    target: PatternEditorTarget,
    preferred_occurrence: Option<crate::pattern_use_graph::PatternOccurrenceTarget>,
    title: SharedString,
) -> SequencerEditorSource {
    let snapshot = PatternUseSnapshot::from_project(&project.project);
    let hydration = hydrate_pattern_editor(snapshot, target, None).and_then(|definition| {
        preferred_occurrence
            .filter(|preferred| {
                definition
                    .uses
                    .occurrences
                    .iter()
                    .any(|occurrence| occurrence.target == *preferred)
            })
            .or_else(|| {
                definition
                    .uses
                    .occurrences
                    .first()
                    .map(|occurrence| occurrence.target)
            })
            .map(|occurrence| hydrate_pattern_editor(snapshot, target, Some(occurrence)))
            .unwrap_or(Ok(definition))
    });
    match hydration {
        Ok(hydration) => SequencerEditorSource::from_workflow_hydration(
            Arc::new(Mutex::new(sequencer)),
            hydration,
            title,
        ),
        Err(error) => {
            eprintln!("hydrating pattern editor: {error}");
            SequencerEditorSource::targeted(Arc::new(Mutex::new(sequencer)), target, title)
        }
    }
}

pub(super) fn browser_state_from_descriptor(
    descriptor: &WorkspaceViewDescriptor,
) -> Option<AssetBrowserState> {
    let WorkspaceViewState::Browser {
        search,
        selected_asset_id,
    } = &descriptor.state
    else {
        return None;
    };
    let mut state = AssetBrowserState::default();
    state.search = search.clone();
    state.selected = selected_asset_id.map(crate::assets::AssetId);
    Some(state)
}

pub(super) fn sampler_target_from_descriptor(
    descriptor: &WorkspaceViewDescriptor,
) -> Option<SamplerTarget> {
    let WorkspaceTarget::Extension { namespace, key } = &descriptor.target else {
        return None;
    };
    if namespace != "audec" {
        return None;
    }
    key.strip_prefix("kit:")
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|raw| *raw != 0)
        .map(crate::sample_kit::KitId::from_raw)
        .map(SamplerTarget::Kit)
}

pub(super) fn add_product_object_to_selection(
    selection: &mut ProjectSelection,
    object: &ObjectRef,
    project: &crate::daw_project::DawProject,
) {
    let arrangement = &project.state().domains.arrangement;
    match object {
        ObjectRef::Material(asset) => {
            selection.assets.insert(*asset);
        }
        ObjectRef::Sample(material) => {
            selection.assets.insert(match material {
                SourceMaterialRef::Asset(asset) => *asset,
                SourceMaterialRef::VirtualSlice(slice) => slice.source_asset,
            });
        }
        ObjectRef::Pattern(pattern) => {
            selection.patterns.insert(*pattern);
        }
        ObjectRef::PatternOccurrence(occurrence) => {
            selection.clips.insert(occurrence.arrangement_clip);
            if let Some(clip) = arrangement.clip(occurrence.arrangement_clip) {
                selection.tracks.insert(clip.track_id);
                selection.time = Some(FrameSpan {
                    start: clip.placement.start.get(),
                    end: clip.placement.end.get(),
                });
                selection.aspect = selection.time.map(Aspect::Time);
            }
        }
        ObjectRef::AudioClip(clip) => {
            selection.clips.insert(*clip);
            if let Some(clip) = arrangement.clip(*clip) {
                selection.tracks.insert(clip.track_id);
                selection.time = Some(FrameSpan {
                    start: clip.placement.start.get(),
                    end: clip.placement.end.get(),
                });
                selection.aspect = selection.time.map(Aspect::Time);
            }
        }
        ObjectRef::AutomationOccurrence(occurrence) => {
            selection.clips.insert(occurrence.arrangement_clip);
            selection.automation_lanes.insert(occurrence.lane);
            if let Some(clip) = arrangement.clip(occurrence.arrangement_clip) {
                selection.tracks.insert(clip.track_id);
                selection.time = Some(FrameSpan {
                    start: clip.placement.start.get(),
                    end: clip.placement.end.get(),
                });
                selection.aspect = selection.time.map(Aspect::Time);
            }
        }
        ObjectRef::Track(track) => {
            selection.tracks.insert(*track);
        }
        ObjectRef::Bus(bus) => {
            selection.mixer_buses.insert(*bus);
        }
        ObjectRef::Automation(lane) => {
            selection.automation_lanes.insert(*lane);
        }
        ObjectRef::Instrument(_)
        | ObjectRef::Pad(_)
        | ObjectRef::Finding(_)
        | ObjectRef::Explanation(_)
        | ObjectRef::Comparison(_)
        | ObjectRef::Reading(_) => {}
    }
}

pub(super) fn object_asset(object: &ObjectRef) -> Option<crate::assets::AssetId> {
    match object {
        ObjectRef::Material(asset) => Some(*asset),
        ObjectRef::Sample(SourceMaterialRef::Asset(asset)) => Some(*asset),
        ObjectRef::Sample(SourceMaterialRef::VirtualSlice(slice)) => Some(slice.source_asset),
        _ => None,
    }
}

pub(super) fn object_from_promoted_created(
    created: &crate::deprojection_execution::promotion::CreatedObject,
) -> Option<ObjectRef> {
    use crate::deprojection_execution::promotion::CreatedObject;

    match created {
        CreatedObject::ArrangementTrack(id) => Some(ObjectRef::Track(*id)),
        CreatedObject::AudioClip(id)
        | CreatedObject::ExactAudioFallbackClip(id)
        | CreatedObject::ArrangementPatternClip(id)
        | CreatedObject::ArrangementAutomationClip(id) => Some(ObjectRef::AudioClip(*id)),
        CreatedObject::SequencerPattern(id) => Some(ObjectRef::Pattern(*id)),
        CreatedObject::AutomationLane(id) => Some(ObjectRef::Automation(*id)),
        CreatedObject::SampleKit(id) => Some(ObjectRef::Instrument(InstrumentRef::SampleKit(*id))),
        CreatedObject::SampleZone(target) => Some(ObjectRef::Pad(PadRef {
            kit: target.kit,
            pad: target.pad,
            zone: Some(target.zone),
        })),
        CreatedObject::MixerBus(id) => Some(ObjectRef::Bus(*id)),
        CreatedObject::SequencerPatternClip(_)
        | CreatedObject::SequencerLane(_)
        | CreatedObject::SamplePad(_) => None,
    }
}

pub(super) fn promotion_reveal_rank(object: &ObjectRef) -> u8 {
    match object {
        ObjectRef::PatternOccurrence(_) => 0,
        ObjectRef::AudioClip(_) => 1,
        ObjectRef::Pattern(_) => 2,
        ObjectRef::AutomationOccurrence(_) => 3,
        ObjectRef::Automation(_) => 4,
        ObjectRef::Instrument(_) => 5,
        ObjectRef::Pad(_) => 6,
        ObjectRef::Track(_) => 7,
        ObjectRef::Bus(_) => 8,
        ObjectRef::Material(_) | ObjectRef::Sample(_) => 9,
        ObjectRef::Finding(_)
        | ObjectRef::Explanation(_)
        | ObjectRef::Comparison(_)
        | ObjectRef::Reading(_) => 10,
    }
}

pub(super) fn project_contains_object(
    project: &crate::daw_project::DawProject,
    object: &ObjectRef,
) -> bool {
    let domains = &project.state().domains;
    match object {
        ObjectRef::Material(asset) => domains.assets.get(*asset).is_some(),
        ObjectRef::Sample(material) => domains.assets.get(material.asset_id()).is_some(),
        ObjectRef::Instrument(InstrumentRef::SampleKit(kit)) => {
            domains.sample_kits.kits.contains_key(kit)
        }
        ObjectRef::Pad(pad) => domains.sample_kits.kits.get(&pad.kit).is_some_and(|kit| {
            kit.pads.contains_key(&pad.pad)
                && pad
                    .zone
                    .is_none_or(|zone| kit.zones.get(&zone).is_some_and(|zone| zone.pad == pad.pad))
        }),
        ObjectRef::Pattern(pattern) => domains.sequencer.patterns().get(*pattern).is_some(),
        ObjectRef::PatternOccurrence(occurrence) => {
            domains
                .arrangement
                .clip(occurrence.arrangement_clip)
                .is_some()
                && occurrence
                    .sequencer_clip
                    .is_none_or(|clip| domains.sequencer.clip(clip).is_some())
                && occurrence
                    .pattern
                    .is_none_or(|pattern| domains.sequencer.patterns().get(pattern).is_some())
        }
        ObjectRef::AudioClip(clip) => domains.arrangement.clip(*clip).is_some(),
        ObjectRef::Track(track) => domains.arrangement.track(*track).is_some(),
        ObjectRef::Bus(bus) => domains.mixer.bus(*bus).is_some(),
        ObjectRef::Automation(lane) => domains.automation.lane(*lane).is_some(),
        ObjectRef::AutomationOccurrence(occurrence) => {
            domains
                .arrangement
                .clip(occurrence.arrangement_clip)
                .is_some()
                && domains.automation.lane(occurrence.lane).is_some()
        }
        // These product lanes have their own durable catalogs. The project
        // selection boundary must not erase them merely because this view's
        // project aggregate cannot authoritatively query those stores.
        ObjectRef::Finding(_)
        | ObjectRef::Explanation(_)
        | ObjectRef::Comparison(_)
        | ObjectRef::Reading(_) => true,
    }
}

pub(super) fn reveal_breadcrumb(object: &ObjectRef) -> &'static str {
    match object {
        ObjectRef::Material(_) => "Library › selected material",
        ObjectRef::Sample(_) => "Library › selected sample",
        ObjectRef::Instrument(_) => "Instrument › new kit",
        ObjectRef::Pad(_) => "Instrument › new kit › selected pad",
        ObjectRef::Pattern(_) => "Pattern › new pattern",
        ObjectRef::PatternOccurrence(_) => "Arrange › selected pattern occurrence",
        ObjectRef::AudioClip(_) => "Arrange › selected audio clip",
        ObjectRef::AutomationOccurrence(_) => "Arrange › selected automation occurrence",
        ObjectRef::Track(_) => "Arrange › selected track",
        ObjectRef::Bus(_) => "Mixer › selected bus",
        ObjectRef::Automation(_) => "Automation › selected lane",
        ObjectRef::Finding(_) => "Findings › selected finding",
        ObjectRef::Explanation(_) => "Explanation › selected construction",
        ObjectRef::Comparison(_) => "Compare › selected comparison",
        ObjectRef::Reading(_) => "Reading › selected reading",
    }
}

pub(super) fn pattern_workflow_reveal_request(
    outcome: &PatternWorkflowOutcome,
) -> Option<RevealRequest> {
    match outcome {
        PatternWorkflowOutcome::Published { publication, .. } => publication
            .reveal
            .as_ref()
            .map(|reveal| reveal.reveal_request(RevealIntent::ActivateExisting)),
        PatternWorkflowOutcome::Placed { publication, .. } => Some(
            publication
                .editor
                .reveal
                .reveal_request(RevealIntent::ActivateExisting),
        ),
        PatternWorkflowOutcome::Targeted(hydration) => Some(
            hydration
                .reveal
                .reveal_request(RevealIntent::ActivateExisting),
        ),
        PatternWorkflowOutcome::Navigate(reveal) => {
            Some(reveal.reveal_request(RevealIntent::ActivateExisting))
        }
        PatternWorkflowOutcome::Preview(_)
        | PatternWorkflowOutcome::Audition(_)
        | PatternWorkflowOutcome::History(_)
        | PatternWorkflowOutcome::GestureBegan(_)
        | PatternWorkflowOutcome::GestureEnded => None,
    }
}

pub(super) fn pattern_workflow_reveal_headline(object: &ObjectRef) -> &'static str {
    match object {
        ObjectRef::Pattern(_) => "Pattern created",
        ObjectRef::PatternOccurrence(_) => "Pattern placed",
        _ => "Pattern edit completed",
    }
}

pub(super) fn arrangement_reveal_headline(object: &ObjectRef) -> &'static str {
    match object {
        ObjectRef::AudioClip(_) => "Audio clip created",
        ObjectRef::PatternOccurrence(_) => "Pattern occurrence created",
        ObjectRef::AutomationOccurrence(_) => "Automation occurrence created",
        ObjectRef::Track(_) => "Track created",
        ObjectRef::Pattern(_) => "Pattern created",
        _ => "Arrangement edit completed",
    }
}

pub(super) fn project_audio_recipe(
    publication: &ProjectPublication,
    session: ProjectSessionId,
) -> Result<ProjectAudioRenderRecipe, String> {
    ProjectAudioRenderRecipe::session_audition(publication, session)
}

pub(super) fn stable_source_id(path: &str, frame_count: u64, sample_rate: u32) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in path
        .as_bytes()
        .iter()
        .copied()
        .chain(frame_count.to_le_bytes())
        .chain(sample_rate.to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub(super) fn format_frequency(frequency: f32) -> String {
    if frequency >= 1_000.0 {
        format!("{:.2} kHz", frequency / 1_000.0)
    } else {
        format!("{frequency:.1} Hz")
    }
}
