//! Project-wide semantic selection and edit-cursor state.
//!
//! Selection is interaction state, not project truth: changing it never dirties
//! the project and never enters command history.  The types here are GPUI-free
//! so a docked editor, a floating lens, the command palette, and a headless
//! client can name the same targets without sharing widget state.  A selected
//! AIR hypothesis remains a hypothesis; selection does not accept or promote it.

use std::collections::BTreeSet;

use crate::arrangement::{ClipId, TrackId};
use crate::aspect::{Aspect, FrameSpan, SignalLayer};
use crate::assets::AssetId;
use crate::automation::{AutomationLaneId, AutomationPointId};
use crate::mixer::BusId;
use crate::ontology::{EvidenceId, HypothesisId, ObjectId};
use crate::project_controller::ObjectRef;
use crate::sequencer::{NoteId, PatternId, StepLaneId};
use crate::workspace_document::WorkspaceViewId;

/// An addressable step. `StepPattern` intentionally has no independent step
/// IDs, so the stable address includes its definition, lane, and grid index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StepAddress {
    pub pattern: PatternId,
    pub lane: StepLaneId,
    pub index: u32,
}

/// A selectable AIR entity. These variants preserve epistemic kind instead of
/// collapsing every analytic identity into a raw integer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AirSelection {
    Object(ObjectId),
    Hypothesis(HypothesisId),
    Evidence(EvidenceId),
}

/// The primary item supplies keyboard/action context when several typed sets
/// are populated. It is semantic and may safely cross windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SelectableId {
    Track(TrackId),
    Clip(ClipId),
    Pattern(PatternId),
    Note {
        pattern: PatternId,
        note: NoteId,
    },
    Step(StepAddress),
    AutomationLane(AutomationLaneId),
    AutomationPoint {
        lane: AutomationLaneId,
        point: AutomationPointId,
    },
    MixerBus(BusId),
    Asset(AssetId),
    Air(AirSelection),
}

impl SelectableId {
    /// Compatibility lowering into the durable object vocabulary. `None`
    /// means the legacy identity lacks context required by `ObjectRef`; the
    /// caller must not guess a scope or collapse a child into its parent.
    pub fn to_object_ref(self) -> Option<ObjectRef> {
        match self {
            Self::Track(track) => Some(ObjectRef::Track(track)),
            Self::Clip(clip) => Some(ObjectRef::AudioClip(clip)),
            Self::Pattern(pattern) => Some(ObjectRef::Pattern(pattern)),
            Self::AutomationLane(lane) => Some(ObjectRef::Automation(lane)),
            Self::MixerBus(bus) => Some(ObjectRef::Bus(bus)),
            Self::Asset(asset) => Some(ObjectRef::Material(asset)),
            Self::Note { .. } | Self::Step(_) | Self::AutomationPoint { .. } | Self::Air(_) => None,
        }
    }

    /// Lossless compatibility projection. Occurrences, pads/zones, samples,
    /// scoped findings, explanations, comparisons, and readings deliberately
    /// remain unprojectable rather than becoming ambiguous parent IDs.
    pub fn from_object_ref(object: &ObjectRef) -> Option<Self> {
        match object {
            ObjectRef::Material(asset) => Some(Self::Asset(*asset)),
            ObjectRef::Pattern(pattern) => Some(Self::Pattern(*pattern)),
            ObjectRef::AudioClip(clip) => Some(Self::Clip(*clip)),
            ObjectRef::Track(track) => Some(Self::Track(*track)),
            ObjectRef::Bus(bus) => Some(Self::MixerBus(*bus)),
            ObjectRef::Automation(lane) => Some(Self::AutomationLane(*lane)),
            ObjectRef::Sample(_)
            | ObjectRef::Instrument(_)
            | ObjectRef::Pad(_)
            | ObjectRef::PatternOccurrence(_)
            | ObjectRef::AutomationOccurrence(_)
            | ObjectRef::Finding(_)
            | ObjectRef::Explanation(_)
            | ObjectRef::Comparison(_)
            | ObjectRef::Reading(_) => None,
        }
    }
}

/// Selection and reveal agree on identity. The conversions are partial in both
/// directions and say so: a note, a step, or an automation point is selectable
/// without being a durable object, and an occurrence, pad, or scoped finding is
/// a durable object without being one of the legacy selectable ids. Neither
/// direction may guess a scope or collapse a child into its parent.
impl TryFrom<SelectableId> for ObjectRef {
    type Error = SelectableId;

    fn try_from(id: SelectableId) -> Result<Self, Self::Error> {
        id.to_object_ref().ok_or(id)
    }
}

impl TryFrom<&ObjectRef> for SelectableId {
    type Error = crate::project_controller::ObjectKind;

    fn try_from(object: &ObjectRef) -> Result<Self, Self::Error> {
        Self::from_object_ref(object).ok_or_else(|| object.kind())
    }
}

/// What one row reveals.
///
/// A product object is the only durable identity. An AIR row names a region of
/// the analysis field, which has none, so it carries its selection instead of
/// being dressed up as an object. Surfaces that reveal rows share this type
/// rather than each declaring a subject enum of their own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevealSubject {
    Object(crate::project_controller::RevealRequest),
    Air(AirSelection),
}

/// Host-assigned identity of the open project document. This is intentionally
/// distinct from project revision and workspace view identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SelectionDocumentId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SelectionGuard {
    pub document: SelectionDocumentId,
    pub project_revision: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SelectionSource {
    #[default]
    Programmatic,
    Reveal,
    Arrangement,
    PatternEditor,
    AssetBrowser,
    Sampler,
    Mixer,
    Automation,
    Inspector,
    Reading,
    LinkedView,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SelectionProvenance {
    pub source: SelectionSource,
    pub source_view: Option<WorkspaceViewId>,
}

/// Authoritative object identity for selection, reveal, and Inspector. The
/// ordered secondary vector preserves reveal/breadcrumb priority while
/// construction removes duplicates and the primary object.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObjectSelection {
    pub primary: Option<ObjectRef>,
    pub secondary: Vec<ObjectRef>,
    pub guard: Option<SelectionGuard>,
    pub provenance: SelectionProvenance,
}

impl ObjectSelection {
    pub fn guarded(
        primary: ObjectRef,
        secondary: impl IntoIterator<Item = ObjectRef>,
        guard: SelectionGuard,
        provenance: SelectionProvenance,
    ) -> Self {
        let secondary = deduplicate_secondary(&primary, secondary);
        Self {
            primary: Some(primary),
            secondary,
            guard: Some(guard),
            provenance,
        }
    }

    pub fn inspector_target(&self) -> Option<&ObjectRef> {
        self.primary.as_ref()
    }

    pub fn is_empty(&self) -> bool {
        self.primary.is_none() && self.secondary.is_empty()
    }
}

fn deduplicate_secondary(
    primary: &ObjectRef,
    secondary: impl IntoIterator<Item = ObjectRef>,
) -> Vec<ObjectRef> {
    let mut result = Vec::new();
    for object in secondary {
        if object != *primary && !result.contains(&object) {
            result.push(object);
        }
    }
    result
}

/// Exact insertion/edit position, deliberately separate from transport and
/// time selection. Beat-domain cursors can be added without changing the
/// project-frame meaning already used by arrangement and analysis views.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EditCursor {
    pub frame: i64,
}

/// One semantic selection snapshot. The optional [`Aspect`] is the common
/// noun shared by production and forensic views: a time/frequency/object
/// region may coexist with ordinary DAW object selection.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProjectSelection {
    /// Authoritative durable object identity. Geometry remains in `aspect`.
    pub objects: ObjectSelection,
    /// Legacy compatibility projection for views not yet migrated to
    /// `objects.primary`. It is populated only where conversion is lossless.
    pub primary: Option<SelectableId>,
    pub time: Option<FrameSpan>,
    pub tracks: BTreeSet<TrackId>,
    pub clips: BTreeSet<ClipId>,
    pub patterns: BTreeSet<PatternId>,
    pub notes: BTreeSet<(PatternId, NoteId)>,
    pub steps: BTreeSet<StepAddress>,
    pub automation_lanes: BTreeSet<AutomationLaneId>,
    pub automation_points: BTreeSet<(AutomationLaneId, AutomationPointId)>,
    pub mixer_buses: BTreeSet<BusId>,
    pub assets: BTreeSet<AssetId>,
    pub air: BTreeSet<AirSelection>,
    pub aspect: Option<Aspect>,
    /// The signal addressed by `aspect`, when chosen explicitly. `None` is
    /// legacy/default source selection; keeping absence distinct lets a
    /// normalizer detect a legacy `ResidualOf` term instead of silently
    /// conflicting with a derived `Source` value.
    pub signal: Option<SignalLayer>,
}

impl ProjectSelection {
    /// Resolve the one contiguous project-time range this selection denotes.
    ///
    /// Timeline gestures historically populated `time`, aspect-aware panes
    /// populated `aspect`, and some compatibility paths populated both. This
    /// boundary prevents those representations from drifting into two loop or
    /// audition ranges: agreement is accepted, disagreement is explicit, and
    /// non-contiguous/complemented aspect time is never guessed into a loop.
    pub fn timeline_span(&self) -> Result<Option<FrameSpan>, SelectionTimelineError> {
        let aspect = match self.aspect.as_ref() {
            Some(aspect) => aspect_timeline_span(aspect)?,
            None => None,
        };
        match (self.time, aspect) {
            (Some(legacy), Some(semantic)) if legacy != semantic => {
                Err(SelectionTimelineError::ConflictingTimeGeometry {
                    direct: legacy,
                    aspect: semantic,
                })
            }
            (Some(span), _) | (None, Some(span)) => Ok(Some(span)),
            (None, None) => Ok(None),
        }
    }

    /// The effective layer for render/audition consumers. `None` deliberately
    /// means source, but callers that edit/link selection should preserve the
    /// option and use [`normalize_aspect_signal`](Self::normalize_aspect_signal)
    /// first.
    pub fn selected_signal(&self) -> SignalLayer {
        self.signal.unwrap_or_default()
    }

    /// Move legacy signal-bearing aspect terms into the separate signal field.
    ///
    /// This transformation is lossless only for a single signal layer. Mixed
    /// source/construction/residual unions, incompatible nested references,
    /// or an explicit field that disagrees with the term are rejected rather
    /// than being guessed at. The resulting `aspect` contains geometry only.
    pub fn normalize_aspect_signal(&mut self) -> Result<bool, SelectionAspectError> {
        let Some(aspect) = self.aspect.clone() else {
            return Ok(false);
        };
        let (geometry, legacy_signal) = split_signal(aspect)?;
        if let (Some(explicit), Some(legacy)) = (self.signal, legacy_signal) {
            if explicit != legacy {
                return Err(SelectionAspectError::ContradictorySignal { explicit, legacy });
            }
        }
        let changed = self.aspect.as_ref() != Some(&geometry)
            || legacy_signal.is_some_and(|signal| self.signal != Some(signal));
        self.aspect = Some(geometry);
        if self.signal.is_none() {
            self.signal = legacy_signal;
        }
        Ok(changed)
    }
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
            && self.primary.is_none()
            && self.time.is_none()
            && self.tracks.is_empty()
            && self.clips.is_empty()
            && self.patterns.is_empty()
            && self.notes.is_empty()
            && self.steps.is_empty()
            && self.automation_lanes.is_empty()
            && self.automation_points.is_empty()
            && self.mixer_buses.is_empty()
            && self.assets.is_empty()
            && self.air.is_empty()
            && self.aspect.is_none()
            && self.signal.is_none()
    }

    pub fn clear_objects(&mut self) {
        self.objects = ObjectSelection::default();
        self.primary = None;
        self.tracks.clear();
        self.clips.clear();
        self.patterns.clear();
        self.notes.clear();
        self.steps.clear();
        self.automation_lanes.clear();
        self.automation_points.clear();
        self.mixer_buses.clear();
        self.assets.clear();
        self.air.clear();
    }

    pub fn clear_all(&mut self) {
        *self = Self::default();
    }

    /// Build the exact reveal → selection handoff. Related objects retain
    /// reveal order, the primary is never duplicated, and only lossless
    /// compatibility identities are projected into the legacy typed sets.
    pub fn from_reveal(
        primary: ObjectRef,
        related: impl IntoIterator<Item = ObjectRef>,
        guard: SelectionGuard,
        source_view: Option<WorkspaceViewId>,
    ) -> Self {
        let mut selection = Self {
            objects: ObjectSelection::guarded(
                primary,
                related,
                guard,
                SelectionProvenance {
                    source: SelectionSource::Reveal,
                    source_view,
                },
            ),
            ..Self::default()
        };
        selection.rebuild_legacy_projection();
        selection
    }

    pub fn primary_object(&self) -> Option<&ObjectRef> {
        self.objects.primary.as_ref()
    }

    pub fn inspector_handoff(
        &self,
    ) -> Result<Option<InspectorSelectionHandoff>, SelectionGuardError> {
        let Some(target) = self.objects.primary.clone() else {
            return Ok(None);
        };
        Ok(Some(InspectorSelectionHandoff {
            target,
            related: self.objects.secondary.clone(),
            guard: self
                .objects
                .guard
                .ok_or(SelectionGuardError::MissingGuard)?,
            provenance: self.objects.provenance,
        }))
    }

    /// Reconcile a guarded selection against the current immutable project
    /// publication. A document mismatch clears object identity but preserves
    /// time/aspect geometry. A newer selection than the supplied snapshot is
    /// rejected because existence cannot be proven against an older state.
    pub fn reconcile_objects(
        &mut self,
        document: SelectionDocumentId,
        project_revision: u64,
        mut exists: impl FnMut(&ObjectRef) -> bool,
    ) -> Result<SelectionReconcileReport, SelectionGuardError> {
        let Some(guard) = self.objects.guard else {
            if self.objects.is_empty() {
                return Ok(SelectionReconcileReport::default());
            }
            return Err(SelectionGuardError::MissingGuard);
        };
        if guard.document != document {
            let removed = self.object_count();
            let primary_removed = self.objects.primary.is_some();
            self.clear_objects();
            return Ok(SelectionReconcileReport {
                removed,
                primary_removed,
                document_mismatch: true,
                revision_advanced: false,
            });
        }
        if guard.project_revision > project_revision {
            return Err(SelectionGuardError::SnapshotOlderThanSelection {
                selection: guard.project_revision,
                snapshot: project_revision,
            });
        }

        let primary_removed = self
            .objects
            .primary
            .as_ref()
            .is_some_and(|primary| !exists(primary));
        let mut removed = usize::from(primary_removed);
        if primary_removed {
            self.objects.primary = None;
        }
        self.objects.secondary.retain(|object| {
            let retained = exists(object);
            removed += usize::from(!retained);
            retained
        });
        if self.objects.primary.is_none() && !self.objects.secondary.is_empty() {
            self.objects.primary = Some(self.objects.secondary.remove(0));
        }
        let revision_advanced = guard.project_revision != project_revision;
        self.objects.guard = Some(SelectionGuard {
            document,
            project_revision,
        });
        self.rebuild_legacy_projection();
        Ok(SelectionReconcileReport {
            removed,
            primary_removed,
            document_mismatch: false,
            revision_advanced,
        })
    }

    fn object_count(&self) -> usize {
        usize::from(self.objects.primary.is_some()) + self.objects.secondary.len()
    }

    fn clear_legacy_objects(&mut self) {
        self.primary = None;
        self.tracks.clear();
        self.clips.clear();
        self.patterns.clear();
        self.notes.clear();
        self.steps.clear();
        self.automation_lanes.clear();
        self.automation_points.clear();
        self.mixer_buses.clear();
        self.assets.clear();
        self.air.clear();
    }

    fn rebuild_legacy_projection(&mut self) {
        self.clear_legacy_objects();
        self.primary = self
            .objects
            .primary
            .as_ref()
            .and_then(SelectableId::from_object_ref);
        let objects = self
            .objects
            .primary
            .iter()
            .chain(self.objects.secondary.iter())
            .filter_map(SelectableId::from_object_ref)
            .collect::<Vec<_>>();
        for object in objects {
            match object {
                SelectableId::Track(id) => {
                    self.tracks.insert(id);
                }
                SelectableId::Clip(id) => {
                    self.clips.insert(id);
                }
                SelectableId::Pattern(id) => {
                    self.patterns.insert(id);
                }
                SelectableId::AutomationLane(id) => {
                    self.automation_lanes.insert(id);
                }
                SelectableId::MixerBus(id) => {
                    self.mixer_buses.insert(id);
                }
                SelectableId::Asset(id) => {
                    self.assets.insert(id);
                }
                SelectableId::Note { .. }
                | SelectableId::Step(_)
                | SelectableId::AutomationPoint { .. }
                | SelectableId::Air(_) => unreachable!("not produced by ObjectRef lowering"),
            }
        }
    }

    /// Retain only identities still present in a caller-supplied project
    /// snapshot. This is the predictable post-command pruning path; undo does
    /// not otherwise rewrite selection.
    pub fn retain(&mut self, mut exists: impl FnMut(SelectableId) -> bool) {
        self.tracks.retain(|id| exists(SelectableId::Track(*id)));
        self.clips.retain(|id| exists(SelectableId::Clip(*id)));
        self.patterns
            .retain(|id| exists(SelectableId::Pattern(*id)));
        self.notes.retain(|(pattern, note)| {
            exists(SelectableId::Note {
                pattern: *pattern,
                note: *note,
            })
        });
        self.steps.retain(|step| exists(SelectableId::Step(*step)));
        self.automation_lanes
            .retain(|id| exists(SelectableId::AutomationLane(*id)));
        self.automation_points.retain(|(lane, point)| {
            exists(SelectableId::AutomationPoint {
                lane: *lane,
                point: *point,
            })
        });
        self.mixer_buses
            .retain(|id| exists(SelectableId::MixerBus(*id)));
        self.assets.retain(|id| exists(SelectableId::Asset(*id)));
        self.air.retain(|id| exists(SelectableId::Air(*id)));
        if self.primary.is_some_and(|primary| !exists(primary)) {
            self.primary = None;
        }
    }
}

/// Why a semantic selection cannot become one transport selection. The
/// original selection remains valid for analysis; only the transport lowering
/// is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionTimelineError {
    InvalidTimeSpan(FrameSpan),
    EmptyTimeIntersection,
    NonContiguousTimeUnion,
    ComplementedTime,
    ConflictingTimeGeometry {
        direct: FrameSpan,
        aspect: FrameSpan,
    },
}

impl std::fmt::Display for SelectionTimelineError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTimeSpan(span) => write!(
                formatter,
                "selection time span {}..{} is empty or reversed",
                span.start, span.end
            ),
            Self::EmptyTimeIntersection => {
                formatter.write_str("selection time constraints have an empty intersection")
            }
            Self::NonContiguousTimeUnion => formatter
                .write_str("a non-contiguous time union cannot become one transport selection"),
            Self::ComplementedTime => formatter
                .write_str("a complemented time aspect cannot become one transport selection"),
            Self::ConflictingTimeGeometry { direct, aspect } => write!(
                formatter,
                "selection time {}..{} disagrees with aspect time {}..{}",
                direct.start, direct.end, aspect.start, aspect.end
            ),
        }
    }
}

impl std::error::Error for SelectionTimelineError {}

fn aspect_timeline_span(aspect: &Aspect) -> Result<Option<FrameSpan>, SelectionTimelineError> {
    match aspect {
        Aspect::Empty
        | Aspect::All
        | Aspect::Band(_)
        | Aspect::Channels(_)
        | Aspect::Family { .. }
        | Aspect::Object(_)
        | Aspect::ExplainedBy(_)
        | Aspect::ResidualOf(_) => Ok(None),
        Aspect::Time(span) => FrameSpan::new(span.start, span.end)
            .map(Some)
            .ok_or(SelectionTimelineError::InvalidTimeSpan(*span)),
        Aspect::Intersect(children) => {
            let mut time: Option<FrameSpan> = None;
            for child in children {
                if let Some(child) = aspect_timeline_span(child)? {
                    time = Some(match time {
                        Some(current) => current
                            .intersect(child)
                            .ok_or(SelectionTimelineError::EmptyTimeIntersection)?,
                        None => child,
                    });
                }
            }
            Ok(time)
        }
        Aspect::Union(children) => {
            let mut spans = children
                .iter()
                .map(aspect_timeline_span)
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            if spans.is_empty() {
                return Ok(None);
            }
            spans.sort();
            let mut joined = spans[0];
            for span in spans.into_iter().skip(1) {
                if span.start > joined.end {
                    return Err(SelectionTimelineError::NonContiguousTimeUnion);
                }
                joined.end = joined.end.max(span.end);
            }
            Ok(Some(joined))
        }
        Aspect::Complement(child) => {
            if aspect_timeline_span(child)?.is_some() {
                Err(SelectionTimelineError::ComplementedTime)
            } else {
                Ok(None)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectorSelectionHandoff {
    pub target: ObjectRef,
    pub related: Vec<ObjectRef>,
    pub guard: SelectionGuard,
    pub provenance: SelectionProvenance,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelectionReconcileReport {
    pub removed: usize,
    pub primary_removed: bool,
    pub document_mismatch: bool,
    pub revision_advanced: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionGuardError {
    MissingGuard,
    DocumentMismatch {
        expected: SelectionDocumentId,
        actual: SelectionDocumentId,
    },
    ProjectRevisionConflict {
        expected: u64,
        actual: u64,
    },
    SnapshotOlderThanSelection {
        selection: u64,
        snapshot: u64,
    },
}

impl std::fmt::Display for SelectionGuardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingGuard => formatter.write_str("object selection has no document guard"),
            Self::DocumentMismatch { expected, actual } => write!(
                formatter,
                "selection belongs to document {} but session owns {}",
                expected.0, actual.0
            ),
            Self::ProjectRevisionConflict { expected, actual } => write!(
                formatter,
                "selection expected project revision {expected}, current revision is {actual}"
            ),
            Self::SnapshotOlderThanSelection {
                selection,
                snapshot,
            } => write!(
                formatter,
                "selection revision {selection} is newer than reconciliation snapshot {snapshot}"
            ),
        }
    }
}

impl std::error::Error for SelectionGuardError {}

/// A semantic-selection normalization error. These are interaction errors,
/// not project validation failures: a pane can show the incompatible aspect
/// intact and ask the user to choose a single layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectionAspectError {
    ContradictorySignal {
        explicit: SignalLayer,
        legacy: SignalLayer,
    },
    MixedSignalUnion,
    SignalInsideComplement,
}

impl std::fmt::Display for SelectionAspectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ContradictorySignal { explicit, legacy } => write!(
                formatter,
                "selection explicitly chose {explicit:?} but its aspect denotes {legacy:?}"
            ),
            Self::MixedSignalUnion => formatter.write_str(
                "one aspect union cannot address both source and construction/residual layers",
            ),
            Self::SignalInsideComplement => formatter
                .write_str("a signal-bearing aspect cannot be complemented into separate geometry"),
        }
    }
}

impl std::error::Error for SelectionAspectError {}

fn split_signal(aspect: Aspect) -> Result<(Aspect, Option<SignalLayer>), SelectionAspectError> {
    match aspect {
        Aspect::ExplainedBy(reference) => {
            Ok((Aspect::All, Some(SignalLayer::Explanation(reference))))
        }
        Aspect::ResidualOf(reference) => Ok((Aspect::All, Some(SignalLayer::Residual(reference)))),
        Aspect::Intersect(children) => {
            let mut geometry = Vec::with_capacity(children.len());
            let mut signal = None;
            for child in children {
                let (child_geometry, child_signal) = split_signal(child)?;
                signal = unify_signal(signal, child_signal)?;
                geometry.push(child_geometry);
            }
            Ok((
                crate::aspect::normalize(Aspect::Intersect(geometry)),
                signal,
            ))
        }
        Aspect::Union(children) => {
            let mut geometry = Vec::with_capacity(children.len());
            let mut signal = None;
            let mut signal_free = false;
            for child in children {
                let (child_geometry, child_signal) = split_signal(child)?;
                if child_signal.is_none() {
                    signal_free = true;
                }
                signal = unify_signal(signal, child_signal)?;
                geometry.push(child_geometry);
            }
            if signal.is_some() && signal_free {
                return Err(SelectionAspectError::MixedSignalUnion);
            }
            Ok((crate::aspect::normalize(Aspect::Union(geometry)), signal))
        }
        Aspect::Complement(child) => {
            let (geometry, signal) = split_signal(*child)?;
            if signal.is_some() {
                return Err(SelectionAspectError::SignalInsideComplement);
            }
            Ok((
                crate::aspect::normalize(Aspect::Complement(Box::new(geometry))),
                None,
            ))
        }
        geometry => Ok((crate::aspect::normalize(geometry), None)),
    }
}

fn unify_signal(
    current: Option<SignalLayer>,
    next: Option<SignalLayer>,
) -> Result<Option<SignalLayer>, SelectionAspectError> {
    match (current, next) {
        (Some(left), Some(right)) if left != right => Err(SelectionAspectError::MixedSignalUnion),
        (Some(signal), _) | (_, Some(signal)) => Ok(Some(signal)),
        (None, None) => Ok(None),
    }
}

/// Revisioned ephemeral state suitable for publishing through a project
/// session entity. The revision is independent of the project revision.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProjectSelectionState {
    pub selection: ProjectSelection,
    pub edit_cursor: EditCursor,
    pub revision: u64,
}

impl ProjectSelectionState {
    pub fn replace(&mut self, selection: ProjectSelection) -> bool {
        if self.selection == selection {
            return false;
        }
        self.selection = selection;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// Publish a durable object selection into session-owned ephemeral state.
    /// Existing legacy-only callers can continue to use [`replace`](Self::replace),
    /// while reveal/Inspector paths use this boundary to reject cross-document
    /// and out-of-revision handoffs before they become visible.
    pub fn replace_guarded(
        &mut self,
        selection: ProjectSelection,
        document: SelectionDocumentId,
        project_revision: u64,
    ) -> Result<bool, SelectionGuardError> {
        if !selection.objects.is_empty() {
            let guard = selection
                .objects
                .guard
                .ok_or(SelectionGuardError::MissingGuard)?;
            if guard.document != document {
                return Err(SelectionGuardError::DocumentMismatch {
                    expected: guard.document,
                    actual: document,
                });
            }
            if guard.project_revision != project_revision {
                return Err(SelectionGuardError::ProjectRevisionConflict {
                    expected: guard.project_revision,
                    actual: project_revision,
                });
            }
        }
        Ok(self.replace(selection))
    }

    /// Reconcile the currently published selection with a newer project
    /// snapshot and advance only this ephemeral state's revision when the
    /// visible selection or its guard actually changed.
    pub fn reconcile_guarded(
        &mut self,
        document: SelectionDocumentId,
        project_revision: u64,
        exists: impl FnMut(&ObjectRef) -> bool,
    ) -> Result<SelectionReconcileReport, SelectionGuardError> {
        let before = self.selection.clone();
        let report = self
            .selection
            .reconcile_objects(document, project_revision, exists)?;
        if self.selection != before {
            self.revision = self.revision.wrapping_add(1);
        }
        Ok(report)
    }

    pub fn set_edit_cursor(&mut self, cursor: EditCursor) -> bool {
        if self.edit_cursor == cursor {
            return false;
        }
        self.edit_cursor = cursor;
        self.revision = self.revision.wrapping_add(1);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aspect::ExplanationRef;
    use crate::project_controller::PatternOccurrenceRef;
    use crate::sequencer::PatternClipId;

    #[test]
    fn object_clear_preserves_time_and_aspect() {
        let mut selection = ProjectSelection {
            primary: Some(SelectableId::Clip(ClipId::from_raw(2))),
            time: Some(FrameSpan { start: 10, end: 20 }),
            clips: BTreeSet::from([ClipId::from_raw(2)]),
            aspect: Some(Aspect::Time(FrameSpan { start: 10, end: 20 })),
            ..ProjectSelection::default()
        };
        selection.clear_objects();
        assert!(selection.primary.is_none());
        assert!(selection.clips.is_empty());
        assert_eq!(selection.time, Some(FrameSpan { start: 10, end: 20 }));
        assert!(selection.aspect.is_some());
    }

    #[test]
    fn ephemeral_revision_changes_only_on_state_change() {
        let mut state = ProjectSelectionState::default();
        assert!(!state.set_edit_cursor(EditCursor::default()));
        assert!(state.set_edit_cursor(EditCursor { frame: 42 }));
        assert_eq!(state.revision, 1);
        assert!(!state.set_edit_cursor(EditCursor { frame: 42 }));
    }

    #[test]
    fn legacy_signal_aspect_round_trips_into_separate_signal_field() {
        let mut selection = ProjectSelection {
            aspect: Some(Aspect::Intersect(vec![
                Aspect::Time(FrameSpan { start: 10, end: 30 }),
                Aspect::ResidualOf(ExplanationRef::Definition(7)),
            ])),
            ..ProjectSelection::default()
        };
        assert!(selection.normalize_aspect_signal().unwrap());
        assert_eq!(
            selection.aspect,
            Some(Aspect::Time(FrameSpan { start: 10, end: 30 }))
        );
        assert_eq!(
            selection.signal,
            Some(SignalLayer::Residual(ExplanationRef::Definition(7)))
        );
        assert!(!selection.normalize_aspect_signal().unwrap());
    }

    #[test]
    fn contradictory_explicit_and_legacy_signal_is_rejected_without_rewrite() {
        let original = Aspect::Intersect(vec![
            Aspect::Time(FrameSpan { start: 10, end: 30 }),
            Aspect::ResidualOf(ExplanationRef::Definition(7)),
        ]);
        let mut selection = ProjectSelection {
            aspect: Some(original.clone()),
            signal: Some(SignalLayer::Explanation(ExplanationRef::Definition(8))),
            ..ProjectSelection::default()
        };
        assert!(matches!(
            selection.normalize_aspect_signal(),
            Err(SelectionAspectError::ContradictorySignal { .. })
        ));
        assert_eq!(selection.aspect, Some(original));
        assert_eq!(
            selection.signal,
            Some(SignalLayer::Explanation(ExplanationRef::Definition(8)))
        );
    }

    #[test]
    fn mixed_signal_union_is_rejected_without_promoting_a_layer() {
        let original = Aspect::Union(vec![
            Aspect::Time(FrameSpan { start: 0, end: 5 }),
            Aspect::ExplainedBy(ExplanationRef::Definition(2)),
        ]);
        let mut selection = ProjectSelection {
            aspect: Some(original.clone()),
            ..ProjectSelection::default()
        };
        assert_eq!(
            selection.normalize_aspect_signal(),
            Err(SelectionAspectError::MixedSignalUnion)
        );
        assert_eq!(selection.aspect, Some(original));
        assert_eq!(selection.signal, None);
    }

    #[test]
    fn reveal_selection_hands_the_exact_occurrence_to_inspector() {
        let pattern = PatternId::from_raw(17);
        let track = TrackId::from_raw(23);
        let occurrence = ObjectRef::PatternOccurrence(PatternOccurrenceRef {
            arrangement_clip: ClipId::from_raw(29),
            sequencer_clip: Some(PatternClipId::from_raw(31)),
            pattern: Some(pattern),
        });
        let guard = SelectionGuard {
            document: SelectionDocumentId(37),
            project_revision: 41,
        };
        let selection = ProjectSelection::from_reveal(
            occurrence.clone(),
            [
                ObjectRef::Pattern(pattern),
                ObjectRef::Track(track),
                ObjectRef::Pattern(pattern),
                occurrence.clone(),
            ],
            guard,
            Some(WorkspaceViewId(43)),
        );

        assert_eq!(selection.primary_object(), Some(&occurrence));
        assert_eq!(
            selection.objects.secondary,
            vec![ObjectRef::Pattern(pattern), ObjectRef::Track(track)]
        );
        assert_eq!(
            selection.primary, None,
            "an occurrence is not an audio clip"
        );
        assert_eq!(selection.patterns, BTreeSet::from([pattern]));
        assert_eq!(selection.tracks, BTreeSet::from([track]));
        assert_eq!(
            selection.inspector_handoff().unwrap(),
            Some(InspectorSelectionHandoff {
                target: occurrence,
                related: vec![ObjectRef::Pattern(pattern), ObjectRef::Track(track)],
                guard,
                provenance: SelectionProvenance {
                    source: SelectionSource::Reveal,
                    source_view: Some(WorkspaceViewId(43)),
                },
            })
        );
    }

    #[test]
    fn stale_primary_is_removed_and_first_live_secondary_is_promoted() {
        let pattern = PatternId::from_raw(47);
        let track = TrackId::from_raw(53);
        let stale_occurrence = ObjectRef::PatternOccurrence(PatternOccurrenceRef {
            arrangement_clip: ClipId::from_raw(59),
            sequencer_clip: Some(PatternClipId::from_raw(61)),
            pattern: Some(pattern),
        });
        let mut selection = ProjectSelection::from_reveal(
            stale_occurrence,
            [ObjectRef::Pattern(pattern), ObjectRef::Track(track)],
            SelectionGuard {
                document: SelectionDocumentId(67),
                project_revision: 71,
            },
            Some(WorkspaceViewId(73)),
        );
        selection.time = Some(FrameSpan { start: 10, end: 20 });
        selection.aspect = Some(Aspect::Time(FrameSpan { start: 10, end: 20 }));

        let report = selection
            .reconcile_objects(SelectionDocumentId(67), 79, |object| {
                matches!(object, ObjectRef::Pattern(_) | ObjectRef::Track(_))
            })
            .unwrap();

        assert_eq!(
            report,
            SelectionReconcileReport {
                removed: 1,
                primary_removed: true,
                document_mismatch: false,
                revision_advanced: true,
            }
        );
        assert_eq!(
            selection.primary_object(),
            Some(&ObjectRef::Pattern(pattern))
        );
        assert_eq!(selection.objects.secondary, vec![ObjectRef::Track(track)]);
        assert_eq!(selection.primary, Some(SelectableId::Pattern(pattern)));
        assert_eq!(selection.time, Some(FrameSpan { start: 10, end: 20 }));
        assert_eq!(
            selection.aspect,
            Some(Aspect::Time(FrameSpan { start: 10, end: 20 }))
        );
        assert_eq!(
            selection.objects.guard,
            Some(SelectionGuard {
                document: SelectionDocumentId(67),
                project_revision: 79,
            })
        );
    }

    #[test]
    fn guarded_session_boundary_rejects_cross_document_and_stale_publications() {
        let selection = ProjectSelection::from_reveal(
            ObjectRef::Track(TrackId::from_raw(83)),
            [],
            SelectionGuard {
                document: SelectionDocumentId(89),
                project_revision: 97,
            },
            None,
        );
        let mut state = ProjectSelectionState::default();

        assert!(matches!(
            state.replace_guarded(selection.clone(), SelectionDocumentId(101), 97),
            Err(SelectionGuardError::DocumentMismatch { .. })
        ));
        assert!(matches!(
            state.replace_guarded(selection.clone(), SelectionDocumentId(89), 103),
            Err(SelectionGuardError::ProjectRevisionConflict { .. })
        ));
        assert_eq!(state.revision, 0);
        assert!(state
            .replace_guarded(selection, SelectionDocumentId(89), 97)
            .unwrap());
        assert_eq!(state.revision, 1);
    }

    #[test]
    fn compatibility_projection_is_only_available_when_lossless() {
        let pattern = PatternId::from_raw(107);
        assert_eq!(
            SelectableId::from_object_ref(&ObjectRef::Pattern(pattern)),
            Some(SelectableId::Pattern(pattern))
        );
        assert_eq!(
            SelectableId::from_object_ref(&ObjectRef::PatternOccurrence(PatternOccurrenceRef {
                arrangement_clip: ClipId::from_raw(109),
                sequencer_clip: None,
                pattern: Some(pattern),
            })),
            None
        );
        assert_eq!(
            SelectableId::Note {
                pattern,
                note: NoteId::from_raw(113),
            }
            .to_object_ref(),
            None
        );
    }

    #[test]
    fn timeline_span_unifies_direct_and_aspect_geometry_without_guessing() {
        let span = FrameSpan { start: 20, end: 80 };
        let selection = ProjectSelection {
            time: Some(span),
            aspect: Some(Aspect::Intersect(vec![
                Aspect::Band(crate::aspect::BandSpan::new(80.0, 800.0).unwrap()),
                Aspect::Time(span),
            ])),
            ..ProjectSelection::default()
        };
        assert_eq!(selection.timeline_span().unwrap(), Some(span));

        let conflicting = ProjectSelection {
            time: Some(span),
            aspect: Some(Aspect::Time(FrameSpan { start: 21, end: 80 })),
            ..ProjectSelection::default()
        };
        assert!(matches!(
            conflicting.timeline_span(),
            Err(SelectionTimelineError::ConflictingTimeGeometry { .. })
        ));
    }

    #[test]
    fn timeline_span_accepts_contiguous_union_and_refuses_disjoint_or_complemented_time() {
        let contiguous = ProjectSelection {
            aspect: Some(Aspect::Union(vec![
                Aspect::Time(FrameSpan { start: 10, end: 20 }),
                Aspect::Time(FrameSpan { start: 20, end: 30 }),
            ])),
            ..ProjectSelection::default()
        };
        assert_eq!(
            contiguous.timeline_span().unwrap(),
            Some(FrameSpan { start: 10, end: 30 })
        );
        let disjoint = ProjectSelection {
            aspect: Some(Aspect::Union(vec![
                Aspect::Time(FrameSpan { start: 10, end: 20 }),
                Aspect::Time(FrameSpan { start: 21, end: 30 }),
            ])),
            ..ProjectSelection::default()
        };
        assert_eq!(
            disjoint.timeline_span(),
            Err(SelectionTimelineError::NonContiguousTimeUnion)
        );
        let complemented = ProjectSelection {
            aspect: Some(Aspect::Complement(Box::new(Aspect::Time(FrameSpan {
                start: 10,
                end: 20,
            })))),
            ..ProjectSelection::default()
        };
        assert_eq!(
            complemented.timeline_span(),
            Err(SelectionTimelineError::ComplementedTime)
        );
    }
}
