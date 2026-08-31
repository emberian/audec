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
use crate::sequencer::{NoteId, PatternId, StepLaneId};

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
        self.primary.is_none()
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
}
