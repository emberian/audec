//! Stable module boundary for the UI-independent project controller.
//!
//! The implementation currently lives beside `LiveProject` while the legacy
//! lock façade is retired. Consumers should import through this module so the
//! implementation can move without changing GPUI, render, or persistence.

#![allow(unused_imports)]

#[path = "arrangement_actions.rs"]
mod arrangement_actions;
#[path = "constructive_controller.rs"]
mod constructive_controller;
#[path = "rhythm_promotion.rs"]
mod rhythm_promotion;
#[path = "workbench_sampling.rs"]
mod workbench_sampling;

pub use arrangement_actions::{
    execute_arrangement_event, lower_action as lower_arrangement_action, lower_arrangement_event,
    lower_gesture, ArrangementDispatch, ArrangementExecution, ArrangementExecutionError,
    ArrangementHistoryIntent, ArrangementHistoryKind, ArrangementLoweringError,
    ValidatedArrangementEnvelope,
};
pub use constructive_controller::{
    ConstructiveControllerError, ConstructiveOutcome, ConstructivePublication,
    ConstructivePublishedFocus, ConstructiveSourceSnapshot, PreparedSampleAction,
    SampleActionBackgroundWork, SampleActionOutcome,
};
pub use rhythm_promotion::{
    RhythmGridHypothesis, RhythmPromotionAlternative, RhythmPromotionDiagnostic,
    RhythmPromotionDiagnosticCode, RhythmPromotionError, RhythmPromotionIntent, RhythmPromotionSet,
};
pub use workbench_sampling::{
    WorkbenchSampleIntent, WorkbenchSampleOutcome, WorkbenchSamplingError,
};

pub use crate::live_project::{
    ProjectController, ProjectControllerConfig, ProjectControllerError, ProjectControllerUpdate,
    ProjectDomainOwnership,
};

pub use crate::command_journal::{
    decode_runtime_frame, encode_runtime_record, encode_runtime_records, CommandJournalRecord,
    CommandOperation, RuntimeCommandCodec, RuntimeJournalEncodeError,
};
