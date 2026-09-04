//! One inbox for everything a surface tells the Workbench, and one freshness
//! guard for everything a background task tells it later.
//!
//! Before this, each surface owned a mailbox field, a lock, a named drain in
//! the ticker, and a line in teardown; each async path owned a `u64`
//! generation counter with its own drop rule. Here there is one queue and one
//! comparison, so adding a surface adds a variant and adding an async result
//! adds an [`Authority`] argument.

use super::*;

/// Everything a view callback or a background task can tell the Workbench.
/// The envelope carries the source view where the handler needs to answer the
/// surface that asked; variants carrying an [`Entity`] are sent only from
/// main-thread callbacks, which is where they are constructed today.
pub(super) enum WorkbenchEvent {
    Arrangement {
        source: Option<WorkspaceViewId>,
        event: ArrangementViewEvent,
    },
    ArrangementTimeline {
        source: Option<WorkspaceViewId>,
        event: ArrangementTimelineEvent,
    },
    SampleRequest {
        source: Option<WorkspaceViewId>,
        request: SampleActionRequest,
        completion: Option<SampleCompletionTarget>,
    },
    SampleFocus {
        source: Option<WorkspaceViewId>,
        focus: SampleResultFocus,
    },
    ReverseSurface(ReverseSurfaceViewEvent),
    ReverseAnalysisResult(ReverseAnalysisResultEvent),
    ExplanationWorkbench {
        source: WorkspaceViewId,
        event: ExplanationWorkbenchEvent,
    },
    /// `editor` is the pane whose receipt this is. `None` is the standalone
    /// mixer/automation window, which has no workspace view of its own.
    Control {
        editor: Option<WorkspaceViewId>,
        action: ControlAction,
    },
    PatternWorkflow {
        request: PatternWorkflowRequest,
        completion: Entity<SequencerEditor>,
    },
    PatternAudition {
        request: PatternAuditionRequest,
        owner: AuditionOwner,
    },
    ReadingQueryEffect {
        source: WorkspaceViewId,
        effect: ReadingQueryViewEffect,
    },
    Asset(AssetBrowserEvent),
}

/// The write end. A view factory captures one clone of this instead of a
/// mailbox of its own type; it never reads, and it never waits on the
/// Workbench.
#[derive(Clone)]
pub(super) struct WorkbenchSender {
    queue: Arc<Mutex<Vec<WorkbenchEvent>>>,
}

impl WorkbenchSender {
    pub(super) fn send(&self, event: WorkbenchEvent) {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(event);
    }
}

/// The read end, drained once per tick in arrival order.
pub(super) struct WorkbenchInbox {
    queue: Arc<Mutex<Vec<WorkbenchEvent>>>,
}

impl Default for WorkbenchInbox {
    fn default() -> Self {
        Self {
            queue: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl WorkbenchInbox {
    pub(super) fn sender(&self) -> WorkbenchSender {
        WorkbenchSender {
            queue: Arc::clone(&self.queue),
        }
    }

    pub(super) fn drain(&self) -> Vec<WorkbenchEvent> {
        std::mem::take(
            &mut *self
                .queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    /// Teardown: everything the outgoing document's surfaces said and the
    /// Workbench has not handled is dropped here, in one place, instead of by
    /// fourteen hand-written resets.
    ///
    /// The queue is emptied rather than replaced, because the reverse-surface
    /// and explanation-workbench factories are built once per Workbench and
    /// hold their sender for its whole life. Handing them a sender into a queue
    /// nobody drains would silence those panes after the first project open.
    /// Every other sender belongs to a view that this teardown drops.
    pub(super) fn reset(&mut self) {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

/// Which truth a late result was computed against.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Authority {
    /// The installed document: opening, creating, or loading material moves it.
    Document,
    /// Project durability: every save or save-as request moves it.
    Project,
    /// Derived analysis of the current material. A document install moves it
    /// too, because analysis of the previous material is not this one's truth.
    Analysis,
}

impl Authority {
    const fn label(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Project => "project",
            Self::Analysis => "analysis",
        }
    }
}

/// One authority's position in time. Zero is "nothing requested yet"; a bump
/// never returns to zero, so a stored epoch is always comparable.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Epoch(u64);

impl Epoch {
    pub(super) fn bump(&mut self) -> Self {
        self.0 = self.0.wrapping_add(1).max(1);
        *self
    }

    pub(super) const fn get(self) -> u64 {
        self.0
    }
}

/// A value that is only truth for the epoch it was requested at. Every async
/// result crosses [`Workbench::accept`] wearing one of these.
pub(super) struct Fresh<T> {
    authority: Authority,
    requested: Epoch,
    value: T,
}

impl<T> Fresh<T> {
    pub(super) const fn new(authority: Authority, requested: Epoch, value: T) -> Self {
        Self {
            authority,
            requested,
            value,
        }
    }
}

/// A named refusal. The old counters dropped a stale result with a bare
/// `return`, so a missed bump and a correct drop looked identical.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Stale {
    pub(super) authority: Authority,
    pub(super) requested: Epoch,
    pub(super) current: Epoch,
}

impl std::fmt::Display for Stale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "dropped a result requested at {} epoch {}; the {} authority is now at {}",
            self.authority.label(),
            self.requested.get(),
            self.authority.label(),
            self.current.get()
        )
    }
}

impl Workbench {
    pub(super) fn epoch(&self, authority: Authority) -> Epoch {
        match authority {
            Authority::Document => self.document_epoch,
            Authority::Project => self.project_epoch,
            Authority::Analysis => self.analysis_epoch,
        }
    }

    /// Move one authority forward and answer with its new epoch, which is what
    /// a request records. Installing a different document also moves the
    /// analysis authority: the previous material's analysis is not this
    /// document's truth, and two counters checked together were how that was
    /// said before.
    pub(super) fn bump_epoch(&mut self, authority: Authority) -> Epoch {
        match authority {
            Authority::Document => {
                self.analysis_epoch.bump();
                self.document_epoch.bump()
            }
            Authority::Project => self.project_epoch.bump(),
            Authority::Analysis => self.analysis_epoch.bump(),
        }
    }

    /// The one acceptance site. A result whose epoch no longer matches its
    /// authority is refused by name, never silently applied.
    pub(super) fn accept<T>(&self, fresh: Fresh<T>) -> Result<T, Stale> {
        let current = self.epoch(fresh.authority);
        if current == fresh.requested {
            Ok(fresh.value)
        } else {
            Err(Stale {
                authority: fresh.authority,
                requested: fresh.requested,
                current,
            })
        }
    }

    /// Guard a side effect that carries no value. Reports the refusal so a
    /// missed bump is visible instead of looking like a correct drop.
    pub(super) fn still_current(&self, authority: Authority, requested: Epoch) -> bool {
        match self.accept(Fresh::new(authority, requested, ())) {
            Ok(()) => true,
            Err(stale) => {
                eprintln!("{stale}");
                false
            }
        }
    }
}
