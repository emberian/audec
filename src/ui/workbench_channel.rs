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
    /// One analysis lens's own cancellable transform. Each lens is its own
    /// authority, and its clock lives on the pane rather than here: a single
    /// shared analysis epoch would let cancelling the HPSS job drop an
    /// in-flight Loom result that nothing had invalidated.
    Lens(LensJob),
}

/// The analysis lenses that run background work they can cancel and re-request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LensJob {
    /// The spectral waterfall's own transform (FFT or constant-Q).
    Waterfall,
    Hpss,
    Rhythm,
    Loom,
}

impl LensJob {
    const fn label(self) -> &'static str {
        match self {
            Self::Waterfall => "waterfall lens",
            Self::Hpss => "HPSS lens",
            Self::Rhythm => "rhythm lens",
            Self::Loom => "Loom lens",
        }
    }
}

impl Authority {
    const fn label(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Project => "project",
            Self::Analysis => "analysis",
            Self::Lens(lens) => lens.label(),
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
/// result crosses a [`Freshness`] wearing one of these, whether the authority
/// is the Workbench's or a lens's own.
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

/// One authority's clock, and the only place an epoch is ever compared.
///
/// Everything that starts work it may later have to disown keeps one of these
/// per authority it can move: the `Workbench` keeps the three shell
/// authorities, and a `Visualizer` keeps one per lens transform. Holding the
/// comparison here is what lets a lens have an authority of its own without a
/// second drop rule written out per lens.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Freshness {
    authority: Authority,
    epoch: Epoch,
}

impl Freshness {
    pub(super) const fn new(authority: Authority) -> Self {
        Self {
            authority,
            epoch: Epoch(0),
        }
    }

    /// Where this authority stands now, which is what a request records.
    pub(super) const fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// Move this authority forward and answer with its new epoch. Everything
    /// requested before it is no longer this authority's truth.
    pub(super) fn bump(&mut self) -> Epoch {
        self.epoch.bump()
    }

    /// The one acceptance site. A result whose epoch no longer matches its
    /// authority is refused by name, never silently applied.
    pub(super) fn accept<T>(&self, fresh: Fresh<T>) -> Result<T, Stale> {
        if self.epoch == fresh.requested {
            Ok(fresh.value)
        } else {
            Err(Stale {
                authority: self.authority,
                requested: fresh.requested,
                current: self.epoch,
            })
        }
    }

    /// Guard a side effect that carries no value. Reports the refusal so a
    /// missed bump is visible instead of looking like a correct drop.
    pub(super) fn still_current(&self, requested: Epoch) -> bool {
        match self.accept(Fresh::new(self.authority, requested, ())) {
            Ok(()) => true,
            Err(stale) => {
                eprintln!("{stale}");
                false
            }
        }
    }
}

impl Workbench {
    /// The clock this Workbench keeps for one authority.
    fn clock(&self, authority: Authority) -> Freshness {
        Freshness {
            authority,
            epoch: self.epoch(authority),
        }
    }

    pub(super) fn epoch(&self, authority: Authority) -> Epoch {
        match authority {
            Authority::Document => self.document_epoch,
            Authority::Project => self.project_epoch,
            Authority::Analysis => self.analysis_epoch,
            // A lens's clock lives on the lens, so that cancelling one lens's
            // transform cannot drop another's result. The Workbench keeps
            // none: asked for one it stays at "nothing requested yet", so any
            // lens result offered here is refused by name rather than accepted
            // against an authority this type does not own.
            Authority::Lens(_) => Epoch(0),
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
            // A lens's clock lives on the lens (see `epoch`); there is none
            // here to move, and the unmoved answer refuses by name.
            Authority::Lens(_) => self.epoch(authority),
        }
    }

    /// Offer a result to the authority it was requested against; the refusal
    /// is [`Freshness::accept`]'s, so there is one comparison in the shell.
    pub(super) fn accept<T>(&self, fresh: Fresh<T>) -> Result<T, Stale> {
        self.clock(fresh.authority).accept(fresh)
    }

    /// Guard a side effect that carries no value.
    pub(super) fn still_current(&self, authority: Authority, requested: Epoch) -> bool {
        self.clock(authority).still_current(requested)
    }
}
