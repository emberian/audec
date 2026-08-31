//! GPUI-independent project-session contracts and published interaction state.
//!
//! [`ProjectSession`] is the sole runtime owner of one [`LiveProject`] and its
//! analysis context. It owns no editor entities or windows. A GPUI host should
//! wrap this value in `Entity<ProjectSession>`, emit the recorded events, and
//! keep hardware/audio-task handles in a sibling controller. Editors read the
//! cached snapshot and submit command envelopes through the future command
//! service; they do not retain mutable domain mirrors.
//!
//! This module does not claim that an analysis is project truth. `DawProject`
//! remains the constructive/AIR aggregate; retained analysis is evidence and
//! task input attached to that aggregate.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::analysis::Analysis;
use crate::audio::{FrameRange, ProjectFrame, TransportMode, TransportSnapshot};
use crate::change_set::ChangeSet;
use crate::daw_project::ProjectRevisions;
use crate::live_project::{LiveProject, LiveProjectSnapshot};
use crate::project_selection::{EditCursor, ProjectSelection, ProjectSelectionState};
use crate::view_links::{LinkedViewPatch, ViewLinkDelivery, ViewLinkError, ViewLinkRegistry};
use crate::workspace_items::WorkspaceViewId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectSessionId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectLifecycle {
    Empty,
    Loading { source: PathBuf },
    Ready,
    Failed { message: String },
}

/// Cached read publication. It is replaced only after a coherent aggregate
/// snapshot has validated, so views never observe a partially reconciled edit.
#[derive(Clone, Debug)]
pub struct ProjectReadSnapshot {
    pub generation: u64,
    pub lifecycle: ProjectLifecycle,
    pub project: Option<LiveProjectSnapshot>,
    pub analysis: Option<Arc<Analysis>>,
}

impl ProjectReadSnapshot {
    pub fn revisions(&self) -> Option<ProjectRevisions> {
        self.project.as_ref().map(LiveProjectSnapshot::revisions)
    }
}

impl Default for ProjectReadSnapshot {
    fn default() -> Self {
        Self {
            generation: 0,
            lifecycle: ProjectLifecycle::Empty,
            project: None,
            analysis: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderActivity {
    Idle,
    Rendering { generation: u64, revision: u64 },
    Ready { revision: u64 },
    Failed { generation: u64 },
}

/// Cheap UI-facing state owned by the eventual audio controller. It carries
/// exact frame values and status only, never PCM or a second transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectAudioStatus {
    pub transport: TransportSnapshot,
    pub render: RenderActivity,
    pub preview_active: bool,
    pub diagnostic: Option<String>,
}

impl Default for ProjectAudioStatus {
    fn default() -> Self {
        Self {
            transport: TransportSnapshot {
                mode: TransportMode::Stopped,
                frame: ProjectFrame(0),
                loop_region: None,
                loop_enabled: false,
                revision: 0,
            },
            render: RenderActivity::Idle,
            preview_active: false,
            diagnostic: None,
        }
    }
}

impl ProjectAudioStatus {
    pub fn with_loop(mut self, range: Option<FrameRange>, enabled: bool) -> Self {
        self.transport.loop_region = range;
        self.transport.loop_enabled = enabled && range.is_some();
        self
    }
}

#[derive(Clone, Debug)]
pub enum ProjectSessionEvent {
    LifecycleChanged(ProjectLifecycle),
    ProjectPublished {
        generation: u64,
        revisions: ProjectRevisions,
        /// `None` is an initial/load publication rather than an edit.
        change_set: Option<ChangeSet>,
    },
    SelectionChanged {
        revision: u64,
    },
    LinkedViews(Vec<ViewLinkDelivery>),
    AudioChanged(ProjectAudioStatus),
    DiagnosticsChanged,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProjectEventFilter(u16);

impl ProjectEventFilter {
    pub const LIFECYCLE: Self = Self(1 << 0);
    pub const PROJECT: Self = Self(1 << 1);
    pub const SELECTION: Self = Self(1 << 2);
    pub const LINKS: Self = Self(1 << 3);
    pub const AUDIO: Self = Self(1 << 4);
    pub const DIAGNOSTICS: Self = Self(1 << 5);
    pub const ALL: Self = Self(
        Self::LIFECYCLE.0
            | Self::PROJECT.0
            | Self::SELECTION.0
            | Self::LINKS.0
            | Self::AUDIO.0
            | Self::DIAGNOSTICS.0,
    );

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    fn admits(self, event: &ProjectSessionEvent) -> bool {
        self.contains(match event {
            ProjectSessionEvent::LifecycleChanged(_) => Self::LIFECYCLE,
            ProjectSessionEvent::ProjectPublished { .. } => Self::PROJECT,
            ProjectSessionEvent::SelectionChanged { .. } => Self::SELECTION,
            ProjectSessionEvent::LinkedViews(_) => Self::LINKS,
            ProjectSessionEvent::AudioChanged(_) => Self::AUDIO,
            ProjectSessionEvent::DiagnosticsChanged => Self::DIAGNOSTICS,
        })
    }
}

#[derive(Clone, Debug)]
struct StampedEvent {
    sequence: u64,
    event: ProjectSessionEvent,
}

/// Cursor-based subscription contract. The GPUI wrapper can keep one cursor
/// per pane and translate batches into `cx.notify()` without callbacks that
/// re-enter the session during mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectEventSubscription {
    pub filter: ProjectEventFilter,
    next_sequence: u64,
}

#[derive(Clone, Debug)]
pub struct ProjectEventBatch {
    pub events: Vec<ProjectSessionEvent>,
    /// True when the subscriber fell behind the bounded log and should refresh
    /// its complete read snapshot before applying incremental events.
    pub missed_events: bool,
}

#[derive(Clone, Debug)]
struct ProjectEventLog {
    next_sequence: u64,
    capacity: usize,
    events: VecDeque<StampedEvent>,
}

impl ProjectEventLog {
    fn new(capacity: usize) -> Self {
        Self {
            next_sequence: 1,
            capacity: capacity.max(1),
            events: VecDeque::new(),
        }
    }

    fn push(&mut self, event: ProjectSessionEvent) {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1).max(1);
        self.events.push_back(StampedEvent { sequence, event });
        while self.events.len() > self.capacity {
            self.events.pop_front();
        }
    }

    fn subscribe(&self, filter: ProjectEventFilter) -> ProjectEventSubscription {
        ProjectEventSubscription {
            filter,
            next_sequence: self.next_sequence,
        }
    }

    fn poll(&self, subscription: &mut ProjectEventSubscription) -> ProjectEventBatch {
        let oldest = self
            .events
            .front()
            .map_or(self.next_sequence, |event| event.sequence);
        let missed_events = subscription.next_sequence < oldest;
        let start = subscription.next_sequence.max(oldest);
        let events = self
            .events
            .iter()
            .filter(|event| event.sequence >= start)
            .filter(|event| subscription.filter.admits(&event.event))
            .map(|event| event.event.clone())
            .collect();
        subscription.next_sequence = self.next_sequence;
        ProjectEventBatch {
            events,
            missed_events,
        }
    }
}

/// Pure session core. The GPUI entity wrapper is responsible for background
/// jobs, hardware handles, and calling `cx.emit` after these methods record an
/// event. The command lane will add `apply_envelope` at this boundary.
pub struct ProjectSession {
    id: ProjectSessionId,
    live: Option<LiveProject>,
    published: ProjectReadSnapshot,
    selection: ProjectSelectionState,
    links: ViewLinkRegistry,
    audio: ProjectAudioStatus,
    diagnostics: Vec<String>,
    events: ProjectEventLog,
}

impl ProjectSession {
    pub fn new(id: ProjectSessionId) -> Result<Self, ProjectSessionError> {
        if id.0 == 0 {
            return Err(ProjectSessionError::ZeroId);
        }
        Ok(Self {
            id,
            live: None,
            published: ProjectReadSnapshot::default(),
            selection: ProjectSelectionState::default(),
            links: ViewLinkRegistry::default(),
            audio: ProjectAudioStatus::default(),
            diagnostics: Vec::new(),
            events: ProjectEventLog::new(512),
        })
    }

    pub const fn id(&self) -> ProjectSessionId {
        self.id
    }

    pub fn snapshot(&self) -> &ProjectReadSnapshot {
        &self.published
    }

    pub fn live_project(&self) -> Option<&LiveProject> {
        self.live.as_ref()
    }

    pub fn selection(&self) -> &ProjectSelectionState {
        &self.selection
    }

    pub fn links(&self) -> &ViewLinkRegistry {
        &self.links
    }

    pub fn links_mut(&mut self) -> &mut ViewLinkRegistry {
        &mut self.links
    }

    pub fn audio_status(&self) -> &ProjectAudioStatus {
        &self.audio
    }

    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    pub fn begin_loading(&mut self, source: PathBuf) {
        self.live = None;
        self.published.project = None;
        self.published.analysis = None;
        self.published.generation = self.published.generation.wrapping_add(1);
        self.published.lifecycle = ProjectLifecycle::Loading { source };
        self.events.push(ProjectSessionEvent::LifecycleChanged(
            self.published.lifecycle.clone(),
        ));
    }

    pub fn install(
        &mut self,
        live: LiveProject,
        analysis: Option<Arc<Analysis>>,
    ) -> Result<ProjectRevisions, ProjectSessionError> {
        let snapshot = live
            .snapshot()
            .map_err(|error| ProjectSessionError::Project(error.to_string()))?;
        let revisions = snapshot.revisions();
        self.live = Some(live);
        self.published = ProjectReadSnapshot {
            generation: self.published.generation.wrapping_add(1),
            lifecycle: ProjectLifecycle::Ready,
            project: Some(snapshot),
            analysis,
        };
        self.events.push(ProjectSessionEvent::LifecycleChanged(
            ProjectLifecycle::Ready,
        ));
        self.events.push(ProjectSessionEvent::ProjectPublished {
            generation: self.published.generation,
            revisions,
            change_set: None,
        });
        Ok(revisions)
    }

    /// Temporary reconcile-era publication. Once editors emit envelopes, the
    /// command service calls this only with the already-applied change set and
    /// reconcile becomes a debug integrity check.
    pub fn refresh_published(
        &mut self,
        change_set: Option<ChangeSet>,
    ) -> Result<ProjectRevisions, ProjectSessionError> {
        let live = self.live.as_ref().ok_or(ProjectSessionError::NoProject)?;
        let snapshot = live
            .snapshot()
            .map_err(|error| ProjectSessionError::Project(error.to_string()))?;
        let revisions = snapshot.revisions();
        self.published.project = Some(snapshot);
        self.published.generation = self.published.generation.wrapping_add(1);
        self.published.lifecycle = ProjectLifecycle::Ready;
        self.events.push(ProjectSessionEvent::ProjectPublished {
            generation: self.published.generation,
            revisions,
            change_set,
        });
        Ok(revisions)
    }

    pub fn fail(&mut self, message: impl Into<String>) {
        let lifecycle = ProjectLifecycle::Failed {
            message: message.into(),
        };
        self.published.lifecycle = lifecycle.clone();
        self.published.generation = self.published.generation.wrapping_add(1);
        self.events
            .push(ProjectSessionEvent::LifecycleChanged(lifecycle));
    }

    pub fn replace_selection(&mut self, selection: ProjectSelection) -> bool {
        let changed = self.selection.replace(selection);
        if changed {
            self.events.push(ProjectSessionEvent::SelectionChanged {
                revision: self.selection.revision,
            });
        }
        changed
    }

    pub fn set_edit_cursor(&mut self, cursor: EditCursor) -> bool {
        let changed = self.selection.set_edit_cursor(cursor);
        if changed {
            self.events.push(ProjectSessionEvent::SelectionChanged {
                revision: self.selection.revision,
            });
        }
        changed
    }

    pub fn publish_linked_view_state(
        &mut self,
        source: WorkspaceViewId,
        patch: LinkedViewPatch,
    ) -> Result<Vec<ViewLinkDelivery>, ProjectSessionError> {
        let deliveries = self.links.publish(source, patch)?;
        if !deliveries.is_empty() {
            self.events
                .push(ProjectSessionEvent::LinkedViews(deliveries.clone()));
        }
        Ok(deliveries)
    }

    pub fn set_audio_status(&mut self, status: ProjectAudioStatus) -> bool {
        if self.audio == status {
            return false;
        }
        self.audio = status.clone();
        self.events.push(ProjectSessionEvent::AudioChanged(status));
        true
    }

    pub fn replace_diagnostics(&mut self, diagnostics: Vec<String>) -> bool {
        if self.diagnostics == diagnostics {
            return false;
        }
        self.diagnostics = diagnostics;
        self.events.push(ProjectSessionEvent::DiagnosticsChanged);
        true
    }

    pub fn subscribe(&self, filter: ProjectEventFilter) -> ProjectEventSubscription {
        self.events.subscribe(filter)
    }

    pub fn poll_events(&self, subscription: &mut ProjectEventSubscription) -> ProjectEventBatch {
        self.events.poll(subscription)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectSessionError {
    ZeroId,
    NoProject,
    Project(String),
    Links(ViewLinkError),
}

impl fmt::Display for ProjectSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroId => formatter.write_str("project session ID zero is reserved"),
            Self::NoProject => formatter.write_str("the project session has no live project"),
            Self::Project(message) => write!(formatter, "project session: {message}"),
            Self::Links(error) => error.fmt(formatter),
        }
    }
}

impl Error for ProjectSessionError {}

impl From<ViewLinkError> for ProjectSessionError {
    fn from(error: ViewLinkError) -> Self {
        Self::Links(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriptions_filter_and_do_not_reenter_the_session() {
        let mut session = ProjectSession::new(ProjectSessionId(1)).unwrap();
        let mut selection = session.subscribe(ProjectEventFilter::SELECTION);
        session.set_edit_cursor(EditCursor { frame: 12 });
        session.set_audio_status(ProjectAudioStatus {
            preview_active: true,
            ..ProjectAudioStatus::default()
        });
        let batch = session.poll_events(&mut selection);
        assert!(!batch.missed_events);
        assert_eq!(batch.events.len(), 1);
        assert!(matches!(
            batch.events[0],
            ProjectSessionEvent::SelectionChanged { revision: 1 }
        ));
    }

    #[test]
    fn selection_changes_do_not_change_project_generation() {
        let mut session = ProjectSession::new(ProjectSessionId(1)).unwrap();
        let generation = session.snapshot().generation;
        session.set_edit_cursor(EditCursor { frame: 99 });
        assert_eq!(session.snapshot().generation, generation);
    }
}
