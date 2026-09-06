//! Portable workspace item identities and the pane-kind vocabulary the action
//! layer projects onto.
//!
//! These types deliberately know nothing about GPUI, Guise `ItemId`, native
//! windows, or renderer entities. A [`WorkspaceViewId`] names one persisted
//! view instance; the UI host owns a separate runtime map from that identity
//! to Guise. Moving or floating a view therefore never changes its identity or
//! reconstructs its editor state.
//!
//! There is one [`EditorTarget`] in the product and it lives in
//! [`crate::workspace_document`], where the document that persists it is
//! defined; this module re-exports nothing of its own for targets.

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

/// Item kind controls factory and lifecycle policy; it is not instance
/// identity and does not imply a musical target. This is the action layer's
/// projection of [`crate::workspace_document::WorkspaceItemKind`]: it folds the
/// pattern editor's mode and the sampler extension into one name each, because
/// action scope does not vary with them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkspaceItemKind {
    Overview,
    Arrangement,
    Browser,
    Inspector,
    PatternEditor,
    AutomationEditor,
    Mixer,
    SamplerEditor,
    Analysis(AnalysisViewKind),
}
