//! Portable workspace item identities, targets, descriptors, and policies.
//!
//! These types deliberately know nothing about GPUI, Guise `ItemId`, native
//! windows, or renderer entities. A [`WorkspaceViewId`] names one persisted
//! view instance; the UI host owns a separate runtime map from that identity
//! to Guise. Moving or floating a view therefore never changes its identity or
//! reconstructs its editor state.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::automation::AutomationLaneId;
use crate::mixer::BusId;
use crate::ontology::SourceId;
use crate::reconstruction::ReconstructionProposalId;
use crate::sequencer::PatternId;
use crate::workspace_document::LinkGroupId;
pub use crate::workspace_document::WorkspaceViewId;

/// Broad presentation family of a forensic or reverse-production pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AnalysisViewKind {
    Waveform,
    Spectrum,
    Waterfall,
    Rhythm,
    Components,
    Separation,
    Loom,
    Coverage,
    Comparison,
    AirQuery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PatternEditorMode {
    PianoRoll,
    Steps,
}

/// The semantic target survives view recreation and persistence. Runtime
/// entity handles and array indexes are never targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorTarget {
    Project,
    Arrangement,
    Assets,
    Inspector,
    Pattern {
        definition: PatternId,
        mode: PatternEditorMode,
    },
    AutomationLane(AutomationLaneId),
    Mixer {
        bus: Option<BusId>,
    },
    Analysis {
        source: Option<SourceId>,
        kind: AnalysisViewKind,
    },
    Explanation(ReconstructionProposalId),
}

/// Item kind controls factory and lifecycle policy; it is not instance
/// identity and does not imply a musical target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkspaceItemKind {
    Overview,
    Arrangement,
    Browser,
    Inspector,
    PatternEditor,
    AutomationEditor,
    Mixer,
    Analysis(AnalysisViewKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemMultiplicity {
    SingletonByKind,
    SingletonByTarget,
    Multiple,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemCloseBehavior {
    Pinned,
    Hide,
    RemoveDescriptor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkspaceItemPolicy {
    pub multiplicity: ItemMultiplicity,
    pub close: ItemCloseBehavior,
    pub can_float: bool,
    pub project_bearing: bool,
}

impl WorkspaceItemKind {
    pub const fn policy(self) -> WorkspaceItemPolicy {
        match self {
            Self::Overview => WorkspaceItemPolicy {
                multiplicity: ItemMultiplicity::SingletonByKind,
                close: ItemCloseBehavior::Pinned,
                can_float: false,
                project_bearing: true,
            },
            Self::Browser | Self::Inspector => WorkspaceItemPolicy {
                multiplicity: ItemMultiplicity::SingletonByKind,
                close: ItemCloseBehavior::Hide,
                can_float: true,
                project_bearing: false,
            },
            Self::Mixer => WorkspaceItemPolicy {
                multiplicity: ItemMultiplicity::SingletonByTarget,
                close: ItemCloseBehavior::Hide,
                can_float: true,
                project_bearing: true,
            },
            Self::Arrangement | Self::PatternEditor | Self::AutomationEditor => {
                WorkspaceItemPolicy {
                    multiplicity: ItemMultiplicity::SingletonByTarget,
                    close: ItemCloseBehavior::Hide,
                    can_float: true,
                    project_bearing: true,
                }
            }
            Self::Analysis(_) => WorkspaceItemPolicy {
                multiplicity: ItemMultiplicity::Multiple,
                close: ItemCloseBehavior::RemoveDescriptor,
                can_float: true,
                project_bearing: false,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorTool {
    Pointer,
    Draw,
    Knife,
    Erase,
    Audition,
    Probe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameViewport {
    pub start: i64,
    pub end: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeatViewport {
    pub start_tick: i64,
    pub end_tick: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OverviewViewState {
    pub viewport: FrameViewport,
    pub follow: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArrangementViewState {
    pub viewport: FrameViewport,
    pub tool: EditorTool,
    pub snap_ticks: Option<u64>,
    pub header_width: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PatternViewState {
    pub viewport: BeatViewport,
    pub tool: EditorTool,
    pub quantize_ticks: u64,
    pub vertical_origin: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AutomationViewState {
    pub viewport: BeatViewport,
    pub tool: EditorTool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BrowserViewState {
    pub search: String,
    pub selected_asset: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnalysisViewState {
    pub viewport: FrameViewport,
    pub min_frequency_hz: f32,
    pub max_frequency_hz: f32,
    pub follow: bool,
    /// A stable recipe fingerprint; the analysis subsystem owns its meaning.
    pub recipe: u128,
}

/// Typed state saved with the descriptor. Unknown durable variants belong in
/// the persistence DTO, not as untyped JSON inside the runtime model.
#[derive(Clone, Debug, PartialEq)]
pub enum EditorViewState {
    Overview(OverviewViewState),
    Arrangement(ArrangementViewState),
    Browser(BrowserViewState),
    Inspector,
    Pattern(PatternViewState),
    Automation(AutomationViewState),
    Mixer,
    Analysis(AnalysisViewState),
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceItemDescriptor {
    pub id: WorkspaceViewId,
    pub kind: WorkspaceItemKind,
    pub target: EditorTarget,
    pub title_override: Option<String>,
    pub link_group: LinkGroupId,
    pub state: EditorViewState,
}

impl WorkspaceItemDescriptor {
    pub fn policy(&self) -> WorkspaceItemPolicy {
        self.kind.policy()
    }

    pub fn validate(&self) -> Result<(), WorkspaceItemError> {
        if self.id.0 == 0 {
            return Err(WorkspaceItemError::ZeroId);
        }
        if self
            .title_override
            .as_ref()
            .is_some_and(|title| title.trim().is_empty())
        {
            return Err(WorkspaceItemError::EmptyTitle(self.id));
        }
        if !kind_accepts_target(self.kind, &self.target) {
            return Err(WorkspaceItemError::TargetKindMismatch(self.id));
        }
        if !state_matches_kind(self.kind, &self.state) {
            return Err(WorkspaceItemError::StateKindMismatch(self.id));
        }
        Ok(())
    }
}

fn kind_accepts_target(kind: WorkspaceItemKind, target: &EditorTarget) -> bool {
    matches!(
        (kind, target),
        (WorkspaceItemKind::Overview, EditorTarget::Project)
            | (WorkspaceItemKind::Arrangement, EditorTarget::Arrangement)
            | (WorkspaceItemKind::Browser, EditorTarget::Assets)
            | (WorkspaceItemKind::Inspector, EditorTarget::Inspector)
            | (
                WorkspaceItemKind::PatternEditor,
                EditorTarget::Pattern { .. }
            )
            | (
                WorkspaceItemKind::AutomationEditor,
                EditorTarget::AutomationLane(_)
            )
            | (WorkspaceItemKind::Mixer, EditorTarget::Mixer { .. })
            | (
                WorkspaceItemKind::Analysis(_),
                EditorTarget::Analysis { .. } | EditorTarget::Explanation(_)
            )
    )
}

fn state_matches_kind(kind: WorkspaceItemKind, state: &EditorViewState) -> bool {
    matches!(
        (kind, state),
        (WorkspaceItemKind::Overview, EditorViewState::Overview(_))
            | (
                WorkspaceItemKind::Arrangement,
                EditorViewState::Arrangement(_)
            )
            | (WorkspaceItemKind::Browser, EditorViewState::Browser(_))
            | (WorkspaceItemKind::Inspector, EditorViewState::Inspector)
            | (
                WorkspaceItemKind::PatternEditor,
                EditorViewState::Pattern(_)
            )
            | (
                WorkspaceItemKind::AutomationEditor,
                EditorViewState::Automation(_)
            )
            | (WorkspaceItemKind::Mixer, EditorViewState::Mixer)
            | (WorkspaceItemKind::Analysis(_), EditorViewState::Analysis(_))
    )
}

/// Pure descriptor catalog. Guise allocation and entity construction live in
/// the UI host; this catalog only guarantees stable, never-reused IDs.
#[derive(Clone, Debug)]
pub struct WorkspaceItemCatalog {
    items: BTreeMap<WorkspaceViewId, WorkspaceItemDescriptor>,
    retired: BTreeSet<WorkspaceViewId>,
    next_id: u64,
}

impl Default for WorkspaceItemCatalog {
    fn default() -> Self {
        Self {
            items: BTreeMap::new(),
            retired: BTreeSet::new(),
            next_id: WorkspaceViewId::FIRST_DYNAMIC,
        }
    }
}

impl WorkspaceItemCatalog {
    pub fn descriptors(&self) -> impl ExactSizeIterator<Item = &WorkspaceItemDescriptor> {
        self.items.values()
    }

    pub fn get(&self, id: WorkspaceViewId) -> Option<&WorkspaceItemDescriptor> {
        self.items.get(&id)
    }

    pub fn insert_existing(
        &mut self,
        descriptor: WorkspaceItemDescriptor,
    ) -> Result<(), WorkspaceItemError> {
        descriptor.validate()?;
        let id = descriptor.id;
        if self.items.contains_key(&id) || self.retired.contains(&id) {
            return Err(WorkspaceItemError::DuplicateId(id));
        }
        self.next_id = self
            .next_id
            .max(id.0.checked_add(1).ok_or(WorkspaceItemError::IdExhausted)?);
        self.items.insert(id, descriptor);
        Ok(())
    }

    pub fn allocate(
        &mut self,
        build: impl FnOnce(WorkspaceViewId) -> WorkspaceItemDescriptor,
    ) -> Result<WorkspaceViewId, WorkspaceItemError> {
        let id = WorkspaceViewId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(WorkspaceItemError::IdExhausted)?;
        let descriptor = build(id);
        if descriptor.id != id {
            return Err(WorkspaceItemError::AllocatorIdMismatch {
                expected: id,
                actual: descriptor.id,
            });
        }
        descriptor.validate()?;
        self.items.insert(id, descriptor);
        Ok(id)
    }

    pub fn remove(
        &mut self,
        id: WorkspaceViewId,
    ) -> Result<WorkspaceItemDescriptor, WorkspaceItemError> {
        let descriptor = self
            .items
            .remove(&id)
            .ok_or(WorkspaceItemError::UnknownId(id))?;
        if descriptor.policy().close == ItemCloseBehavior::Pinned {
            self.items.insert(id, descriptor);
            return Err(WorkspaceItemError::Pinned(id));
        }
        self.retired.insert(id);
        Ok(descriptor)
    }

    pub const fn next_id(&self) -> u64 {
        self.next_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceItemError {
    ZeroId,
    EmptyTitle(WorkspaceViewId),
    DuplicateId(WorkspaceViewId),
    UnknownId(WorkspaceViewId),
    Pinned(WorkspaceViewId),
    TargetKindMismatch(WorkspaceViewId),
    StateKindMismatch(WorkspaceViewId),
    AllocatorIdMismatch {
        expected: WorkspaceViewId,
        actual: WorkspaceViewId,
    },
    IdExhausted,
}

impl fmt::Display for WorkspaceItemError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroId => formatter.write_str("workspace view ID zero is reserved"),
            Self::EmptyTitle(id) => write!(formatter, "workspace view {} has an empty title", id.0),
            Self::DuplicateId(id) => write!(formatter, "workspace view {} already exists", id.0),
            Self::UnknownId(id) => write!(formatter, "workspace view {} is unknown", id.0),
            Self::Pinned(id) => write!(formatter, "workspace view {} is pinned", id.0),
            Self::TargetKindMismatch(id) => {
                write!(
                    formatter,
                    "workspace view {} has an incompatible target",
                    id.0
                )
            }
            Self::StateKindMismatch(id) => {
                write!(
                    formatter,
                    "workspace view {} has incompatible view state",
                    id.0
                )
            }
            Self::AllocatorIdMismatch { expected, actual } => write!(
                formatter,
                "workspace allocator supplied {}, descriptor returned {}",
                expected.0, actual.0
            ),
            Self::IdExhausted => formatter.write_str("workspace view IDs are exhausted"),
        }
    }
}

impl Error for WorkspaceItemError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn analysis(id: WorkspaceViewId) -> WorkspaceItemDescriptor {
        WorkspaceItemDescriptor {
            id,
            kind: WorkspaceItemKind::Analysis(AnalysisViewKind::Waterfall),
            target: EditorTarget::Analysis {
                source: None,
                kind: AnalysisViewKind::Waterfall,
            },
            title_override: None,
            link_group: LinkGroupId::UNLINKED,
            state: EditorViewState::Analysis(AnalysisViewState {
                viewport: FrameViewport { start: 0, end: 1 },
                min_frequency_hz: 20.0,
                max_frequency_hz: 20_000.0,
                follow: false,
                recipe: 0,
            }),
        }
    }

    #[test]
    fn dynamic_ids_are_monotonic_and_never_reused() {
        let mut catalog = WorkspaceItemCatalog::default();
        let first = catalog.allocate(analysis).unwrap();
        catalog.remove(first).unwrap();
        let second = catalog.allocate(analysis).unwrap();
        assert_eq!(first, WorkspaceViewId(7));
        assert_eq!(second, WorkspaceViewId(8));
    }

    #[test]
    fn descriptor_rejects_kind_target_mirrors() {
        let mut descriptor = analysis(WorkspaceViewId(9));
        descriptor.target = EditorTarget::Arrangement;
        assert_eq!(
            descriptor.validate(),
            Err(WorkspaceItemError::TargetKindMismatch(WorkspaceViewId(9)))
        );
    }
}
