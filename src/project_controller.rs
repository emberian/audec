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
#[path = "object_navigation.rs"]
mod object_navigation;
#[path = "pattern_audition_session.rs"]
mod pattern_audition_session;
#[path = "pattern_workflow.rs"]
mod pattern_workflow;
#[path = "receipt_navigation.rs"]
mod receipt_navigation;
#[path = "rhythm_promotion.rs"]
mod rhythm_promotion;
#[path = "rhythm_promotion_chooser.rs"]
mod rhythm_promotion_chooser;
#[path = "workbench_sampling.rs"]
mod workbench_sampling;

pub use crate::pattern_lang::PatternEvalDiagnostic;
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
pub use object_navigation::{
    descriptor_matches_object, object_from_descriptor, recommend_constructive,
    recommend_reconstruction, recommend_sample_result, request_from_sample_focus,
    AutomationOccurrenceRef, FindingKind, FindingLocalId, FindingRef, FindingScope,
    InspectorConsequence, InspectorVisibility, InstrumentRef, ObjectAddressError, ObjectKind,
    ObjectNavigator, ObjectRef, PadRef, PatternOccurrenceRef, RevealDiagnostic,
    RevealDiagnosticCode, RevealIntent, RevealPlan, RevealRecommendation, RevealRequest,
    SelectionConsequence, TargetMultiplicity, WorkspaceReveal,
};
pub use pattern_audition_session::{
    PatternAuditionAdoption, PatternAuditionSessionAdapter, PatternAuditionSessionError,
    PatternAuditionSessionInputs, PatternAuditionSessionJob, PatternAuditionSessionStatus,
    PatternAuditionSessionWorkResult, PatternAuditionStartRequest, SharedPatternAuditionCallback,
};
pub use pattern_workflow::{
    hydrate_pattern_editor, prepare_pattern_audition, publication_from_constructive,
    BeginPatternGestureIntent, PatternAuditionAdapter, PatternAuditionError, PatternAuditionPad,
    PatternAuditionRenderCompletion, PatternAuditionRenderInputs, PatternAuditionRenderJob,
    PatternAuditionRequest, PatternAuditionScope, PatternAuditionSelection,
    PatternCyclePublication, PatternEditPublication, PatternEditorHydration, PatternGestureKind,
    PatternGestureReceipt, PatternLoopAuditionIntent, PatternLoopAuditionPlan, PatternMutationKind,
    PatternWorkflowCallback, PatternWorkflowDispatchReceipt, PatternWorkflowError,
    PatternWorkflowIntent, PatternWorkflowOutcome, PatternWorkflowRequest,
    PatternWorkflowRequestId,
};
pub use receipt_navigation::{
    apply_interpretation_revealed, durable_reveal_rules, execute_arrangement_event_revealed,
    execute_envelope_revealed, execute_pattern_action_revealed, import_asset_revealed,
    recommend_asset, recommend_command_result, recommend_comparison_execution,
    recommend_constructive_application, recommend_coverage_artifact,
    recommend_interpretation_commands, recommend_legacy_migration, recommend_reading,
    register_asset_revealed, ArrangementRevealReceipt, AssetPublication,
    AssetRegistrationPublication, CurrentTerminal, DurableFlow, DurableRevealRule,
    InterpretationRevealReceipt, PatternRevealExecution, PatternRevealExecutionError,
    ProjectMutationReceipt, RevealIntegration,
};
pub use rhythm_promotion::{
    RhythmGridHypothesis, RhythmPromotionAlternative, RhythmPromotionDiagnostic,
    RhythmPromotionDiagnosticCode, RhythmPromotionError, RhythmPromotionIntent, RhythmPromotionSet,
};
pub use rhythm_promotion_chooser::{
    RhythmPromotionApplied, RhythmPromotionChoice, RhythmPromotionChoiceId, RhythmPromotionChooser,
    RhythmPromotionChooserError, RhythmPromotionExplanationKind, RhythmPromotionExplanationLink,
    RhythmPromotionPreviewHandle, RhythmPromotionProvenance, RhythmPromotionSelectionContext,
};
pub use workbench_sampling::{
    WorkbenchSampleIntent, WorkbenchSampleOutcome, WorkbenchSampleWorkflowOutcome,
    WorkbenchSamplingError,
};

pub use crate::live_project::{
    AssetImportControllerOutcome, AssetImportDisposition, ProjectController,
    ProjectControllerConfig, ProjectControllerError, ProjectControllerUpdate,
    ProjectDomainOwnership, ProjectGesture, ProjectJournalCheckpoint, ProjectJournalDelta,
};

pub use crate::command_journal::{
    decode_runtime_frame, encode_runtime_record, encode_runtime_records, CommandJournalRecord,
    CommandOperation, RuntimeCommandCodec, RuntimeJournalEncodeError,
};
