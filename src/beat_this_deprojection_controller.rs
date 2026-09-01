//! UI-neutral lifecycle facade for model-backed rhythm proposals.
//!
//! This state machine makes the entire reverse-product path reachable without
//! giving GPUI, the model worker, or an inference callback mutation authority:
//! observe a broker-attested completion, inspect its claim/evidence, plan the
//! existing constructive alternatives, explicitly select and preview one, and
//! only then accept it through `ProjectSession`. Refusal is retained workflow
//! state, never deletion of the underlying claim.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use super::{
    proposal_from_task, BeatThisCompletionWitness, BeatThisDeprojectionError, BeatThisEvidenceSpan,
    BeatThisRhythmProposal, BeatThisRhythmProposalId,
};
use crate::beat_this::{self, BeatThisRhythmEvidence};
use crate::model_task_service::{
    ModelTaskId, ModelTaskService, ModelTaskStatus, TaskDiagnostic, TaskMaterial, TaskServiceError,
};
use crate::project_controller::{
    RhythmPromotionApplied, RhythmPromotionChoiceId, RhythmPromotionChooser,
    RhythmPromotionChooserError, RhythmPromotionExplanationLink, RhythmPromotionIntent,
    RhythmPromotionPreviewHandle,
};
use crate::project_session::ProjectSession;
use crate::rhythm_explanation::ExplainBudget;

/// Product-facing phase names. They intentionally distinguish a published
/// claim from an interpreted proposal and a preview from accepted authorship.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelDeprojectionPhase {
    Queued,
    Running {
        completed_chunks: u64,
        total_chunks: u64,
        phase: crate::model_wire::ResultPhase,
    },
    Cancelling,
    ClaimOnly {
        cache_hit: bool,
    },
    ProposalReady,
    AlternativesReady,
    AlternativeSelected,
    PreviewReady,
    Applied,
    Refused,
    EvidenceRefused,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelDeprojectionRefusal {
    pub proposal_id: BeatThisRhythmProposalId,
    pub workflow_generation: u64,
    pub reason: String,
}

/// Evidence and musical facts sufficient for an inspector without exposing
/// live service/chooser borrows to the UI layer.
#[derive(Clone, Debug, PartialEq)]
pub struct BeatThisProposalView {
    pub id: BeatThisRhythmProposalId,
    pub claim_id: String,
    pub witness: BeatThisCompletionWitness,
    pub evidence: BeatThisRhythmEvidence,
    pub evidence_spans: Vec<BeatThisEvidenceSpan>,
    pub tempo_bpm: Option<f32>,
    pub meter_beats: Option<usize>,
    pub event_count: usize,
    pub pattern_count: usize,
    pub explanation_count: usize,
    pub caveats: Vec<String>,
}

impl From<&BeatThisRhythmProposal> for BeatThisProposalView {
    fn from(proposal: &BeatThisRhythmProposal) -> Self {
        Self {
            id: proposal.id.clone(),
            claim_id: proposal.claim_id.as_str().into(),
            witness: proposal.witness.clone(),
            evidence: proposal.evidence.clone(),
            evidence_spans: proposal.evidence_spans.clone(),
            tempo_bpm: proposal
                .rhythm
                .tempo_hypotheses
                .first()
                .map(|tempo| tempo.bpm),
            meter_beats: proposal
                .rhythm
                .downbeat_hypotheses
                .first()
                .map(|downbeat| downbeat.meter_beats),
            event_count: proposal.rhythm.hits.len(),
            pattern_count: proposal.rhythm.patterns.len(),
            explanation_count: proposal.explanations.alternatives.len(),
            caveats: proposal.caveats.clone(),
        }
    }
}

/// One inert constructive alternative. `evidence_rank` is presentation order,
/// never an acceptance decision.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRhythmChoiceView {
    pub id: RhythmPromotionChoiceId,
    pub evidence_rank: usize,
    pub bpm: f32,
    pub phase_source_frame: i64,
    pub support: f32,
    pub steps_per_quarter: u16,
    pub diagnostic_messages: Vec<String>,
    pub explanation_links: Vec<RhythmPromotionExplanationLink>,
}

/// Immutable UI snapshot. It can move between an entity, view model, or
/// headless client without carrying a mutable project or worker handle.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelDeprojectionView {
    pub task_id: ModelTaskId,
    pub model_id: String,
    pub workflow_generation: u64,
    pub phase: ModelDeprojectionPhase,
    pub task_diagnostics: Vec<TaskDiagnostic>,
    pub adapter_refusal: Option<String>,
    pub proposal: Option<BeatThisProposalView>,
    pub choices: Vec<ModelRhythmChoiceView>,
    pub selected_choice: Option<RhythmPromotionChoiceId>,
    pub preview_generation: Option<u64>,
    pub applied_choice: Option<RhythmPromotionChoiceId>,
    pub refusal: Option<ModelDeprojectionRefusal>,
}

/// A preview token pinned to both the project/selection revisions (inside the
/// chooser handle) and this facade's proposal generation. Render/audition code
/// may inspect `handle.plan()` but cannot apply it directly through this API.
#[derive(Clone, Debug)]
pub struct ModelRhythmPreview {
    pub task_id: ModelTaskId,
    pub proposal_id: BeatThisRhythmProposalId,
    pub workflow_generation: u64,
    pub handle: RhythmPromotionPreviewHandle,
}

impl ModelRhythmPreview {
    pub fn choice(&self) -> RhythmPromotionChoiceId {
        self.handle.choice
    }
}

/// Project publication paired with the exact model evidence that motivated
/// it. The ordinary constructive receipt remains authoritative for mutation;
/// this wrapper prevents controller/UI code from dropping the claim link while
/// a durable AIR/reading edge is still a separate hookup.
#[derive(Clone, Debug)]
pub struct ModelRhythmApplied {
    pub task_id: ModelTaskId,
    pub proposal_id: BeatThisRhythmProposalId,
    pub claim_id: String,
    pub witness: BeatThisCompletionWitness,
    pub evidence_spans: Vec<BeatThisEvidenceSpan>,
    pub promotion: RhythmPromotionApplied,
}

#[derive(Clone, Debug)]
struct WorkflowEntry {
    generation: u64,
    interpreted_result_sha256: Option<String>,
    proposal: Option<BeatThisRhythmProposal>,
    chooser: Option<RhythmPromotionChooser>,
    preview: Option<ModelRhythmPreview>,
    refusal: Option<ModelDeprojectionRefusal>,
    adapter_refusal: Option<String>,
}

impl Default for WorkflowEntry {
    fn default() -> Self {
        Self {
            generation: 1,
            interpreted_result_sha256: None,
            proposal: None,
            chooser: None,
            preview: None,
            refusal: None,
            adapter_refusal: None,
        }
    }
}

/// Controller-side authority for model deprojection workflow state. Model task
/// execution and project mutation remain owned by their existing services.
#[derive(Default)]
pub struct BeatThisDeprojectionController {
    entries: BTreeMap<ModelTaskId, WorkflowEntry>,
}

impl BeatThisDeprojectionController {
    pub fn new() -> Self {
        Self::default()
    }

    /// Submit already-normalized mono f32/22.05 kHz material through the
    /// pinned Beat This recipe. The preprocessing transform remains the
    /// caller's explicit, provenance-bearing responsibility.
    pub fn start_prepared_analysis(
        &mut self,
        service: &mut ModelTaskService,
        material: TaskMaterial,
    ) -> Result<ModelTaskId, ModelWorkflowError> {
        let recipe = beat_this::task_recipe(material).map_err(ModelWorkflowError::Recipe)?;
        let task_id = service.run(recipe).map_err(ModelWorkflowError::Task)?;
        self.entries.entry(task_id).or_default();
        Ok(task_id)
    }

    pub fn retry_task(
        &mut self,
        service: &mut ModelTaskService,
        prior: ModelTaskId,
    ) -> Result<ModelTaskId, ModelWorkflowError> {
        let task_id = service.retry(prior).map_err(ModelWorkflowError::Task)?;
        self.entries.entry(task_id).or_default();
        Ok(task_id)
    }

    pub fn cancel_task(
        &mut self,
        service: &mut ModelTaskService,
        task_id: ModelTaskId,
    ) -> Result<(), ModelWorkflowError> {
        service.cancel(task_id).map_err(ModelWorkflowError::Task)
    }

    /// Poll product-visible state and compile a newly published live completion
    /// exactly once per proposal identity. Interpretation failure is retained
    /// as inspectable evidence refusal rather than replacing the worker claim.
    pub fn synchronize(
        &mut self,
        service: &ModelTaskService,
        task_id: ModelTaskId,
        budget: ExplainBudget,
    ) -> Result<ModelDeprojectionView, ModelWorkflowError> {
        let task = service
            .task(task_id)
            .ok_or(ModelWorkflowError::UnknownTask(task_id))?;
        let entry = self.entries.entry(task_id).or_default();
        let published_live = matches!(
            &task.status,
            ModelTaskStatus::Published {
                cache_hit: false,
                ..
            }
        );
        if let Some(completion) = published_live
            .then(|| service.verified_completion(task_id))
            .flatten()
        {
            let result_sha256 = completion.receipt.result_sha256();
            if entry.interpreted_result_sha256.as_deref() != Some(result_sha256) {
                entry.interpreted_result_sha256 = Some(result_sha256.into());
                match proposal_from_task(service, task_id, budget) {
                    Ok(proposal) => {
                        let changed = entry
                            .proposal
                            .as_ref()
                            .is_none_or(|current| current.id.as_str() != proposal.id.as_str());
                        if changed {
                            entry.generation = entry.generation.wrapping_add(1).max(1);
                            entry.proposal = Some(proposal);
                            entry.chooser = None;
                            entry.preview = None;
                            entry.refusal = None;
                        }
                        entry.adapter_refusal = None;
                    }
                    Err(error) => {
                        entry.adapter_refusal = Some(error.to_string());
                        entry.chooser = None;
                        entry.preview = None;
                    }
                }
            }
        }
        self.view(service, task_id)
    }

    /// Synchronize every retained task after the runtime owner has polled the
    /// service off the UI thread. Ordering follows stable task IDs.
    pub fn synchronize_all(
        &mut self,
        service: &ModelTaskService,
        budget: ExplainBudget,
    ) -> Result<Vec<ModelDeprojectionView>, ModelWorkflowError> {
        let task_ids = service.tasks().map(|task| task.id).collect::<Vec<_>>();
        task_ids
            .into_iter()
            .map(|task_id| self.synchronize(service, task_id, budget))
            .collect()
    }

    /// Snapshot without triggering artifact IO or interpretation.
    pub fn view(
        &self,
        service: &ModelTaskService,
        task_id: ModelTaskId,
    ) -> Result<ModelDeprojectionView, ModelWorkflowError> {
        let task = service
            .task(task_id)
            .ok_or(ModelWorkflowError::UnknownTask(task_id))?;
        let entry = self.entries.get(&task_id);
        let proposal = entry.and_then(|entry| entry.proposal.as_ref());
        let chooser = entry.and_then(|entry| entry.chooser.as_ref());
        let choices = chooser
            .map(|chooser| chooser.choices().map(choice_view).collect())
            .unwrap_or_default();
        let selected_choice = chooser
            .and_then(RhythmPromotionChooser::selected)
            .map(|choice| choice.id);
        let applied_choice = chooser.and_then(RhythmPromotionChooser::applied_choice);
        let preview_generation = entry
            .and_then(|entry| entry.preview.as_ref())
            .map(|preview| preview.handle.generation);
        let refusal = entry.and_then(|entry| entry.refusal.clone());
        let adapter_refusal = entry.and_then(|entry| entry.adapter_refusal.clone());
        let phase = phase(
            &task.status,
            proposal.is_some(),
            chooser.is_some(),
            selected_choice,
            preview_generation,
            applied_choice,
            refusal.as_ref(),
            adapter_refusal.as_deref(),
        );
        Ok(ModelDeprojectionView {
            task_id,
            model_id: task.recipe.model_id.clone(),
            workflow_generation: entry.map_or(0, |entry| entry.generation),
            phase,
            task_diagnostics: task.diagnostics.clone(),
            adapter_refusal,
            proposal: proposal.map(BeatThisProposalView::from),
            choices,
            selected_choice,
            preview_generation,
            applied_choice,
            refusal,
        })
    }

    /// Build all constructive alternatives against the current exact project
    /// selection. No choice is selected and no preview or edit is produced.
    pub fn plan_promotion(
        &mut self,
        session: &ProjectSession,
        task_id: ModelTaskId,
        intent: RhythmPromotionIntent,
    ) -> Result<(), ModelWorkflowError> {
        let entry = self.entry_mut(task_id)?;
        ensure_editable(entry)?;
        let proposal = entry
            .proposal
            .as_ref()
            .ok_or(ModelWorkflowError::NoProposal(task_id))?;
        let chooser = proposal
            .promotion_chooser(session, intent)
            .map_err(ModelWorkflowError::Promotion)?;
        entry.generation = entry.generation.wrapping_add(1).max(1);
        entry.chooser = Some(chooser);
        entry.preview = None;
        Ok(())
    }

    /// Full immutable proposal for analysis-specific presenters. Most product
    /// surfaces should prefer `view`; this accessor exists for detailed event,
    /// pattern, and explanation visualization without duplicating those types.
    pub fn proposal(&self, task_id: ModelTaskId) -> Option<&BeatThisRhythmProposal> {
        self.entries
            .get(&task_id)
            .and_then(|entry| entry.proposal.as_ref())
    }

    pub fn current_preview(&self, task_id: ModelTaskId) -> Option<&ModelRhythmPreview> {
        self.entries
            .get(&task_id)
            .and_then(|entry| entry.preview.as_ref())
    }

    pub fn select(
        &mut self,
        task_id: ModelTaskId,
        choice: RhythmPromotionChoiceId,
    ) -> Result<(), ModelWorkflowError> {
        let entry = self.entry_mut(task_id)?;
        ensure_editable(entry)?;
        entry
            .chooser
            .as_mut()
            .ok_or(ModelWorkflowError::PromotionNotPlanned(task_id))?
            .select(choice)
            .map_err(ModelWorkflowError::Promotion)?;
        entry.generation = entry.generation.wrapping_add(1).max(1);
        entry.preview = None;
        Ok(())
    }

    pub fn clear_selection(&mut self, task_id: ModelTaskId) -> Result<(), ModelWorkflowError> {
        let entry = self.entry_mut(task_id)?;
        ensure_editable(entry)?;
        entry
            .chooser
            .as_mut()
            .ok_or(ModelWorkflowError::PromotionNotPlanned(task_id))?
            .clear_selection();
        entry.generation = entry.generation.wrapping_add(1).max(1);
        entry.preview = None;
        Ok(())
    }

    /// Pin the selected plan to current project, selection, and workflow
    /// generations. The returned handle is suitable input to an audition lane.
    pub fn preview_selected(
        &mut self,
        session: &ProjectSession,
        task_id: ModelTaskId,
    ) -> Result<ModelRhythmPreview, ModelWorkflowError> {
        let entry = self.entry_mut(task_id)?;
        ensure_editable(entry)?;
        let proposal_id = entry
            .proposal
            .as_ref()
            .map(|proposal| proposal.id.clone())
            .ok_or(ModelWorkflowError::NoProposal(task_id))?;
        let handle = entry
            .chooser
            .as_mut()
            .ok_or(ModelWorkflowError::PromotionNotPlanned(task_id))?
            .preview_selected(session)
            .map_err(ModelWorkflowError::Promotion)?;
        entry.generation = entry.generation.wrapping_add(1).max(1);
        let preview = ModelRhythmPreview {
            task_id,
            proposal_id,
            workflow_generation: entry.generation,
            handle,
        };
        entry.preview = Some(preview.clone());
        Ok(preview)
    }

    pub fn validate_preview(
        &self,
        session: &ProjectSession,
        preview: &ModelRhythmPreview,
    ) -> Result<(), ModelWorkflowError> {
        let entry = self.entry(preview.task_id)?;
        ensure_open(entry)?;
        if entry.generation != preview.workflow_generation
            || entry
                .proposal
                .as_ref()
                .is_none_or(|proposal| proposal.id.as_str() != preview.proposal_id.as_str())
            || entry
                .preview
                .as_ref()
                .is_none_or(|current| current.handle.generation != preview.handle.generation)
        {
            return Err(ModelWorkflowError::StaleWorkflowPreview {
                expected: entry.generation,
                actual: preview.workflow_generation,
            });
        }
        entry
            .chooser
            .as_ref()
            .ok_or(ModelWorkflowError::PromotionNotPlanned(preview.task_id))?
            .validate_preview_handle(session, &preview.handle)
            .map_err(ModelWorkflowError::Promotion)
    }

    /// Accept exactly the previewed alternative. This is the only facade
    /// method that mutates the project, and it delegates to ProjectSession's
    /// ordinary constructive command/history path.
    pub fn accept_preview(
        &mut self,
        session: &mut ProjectSession,
        preview: &ModelRhythmPreview,
    ) -> Result<ModelRhythmApplied, ModelWorkflowError> {
        self.validate_preview(session, preview)?;
        let entry = self.entry_mut(preview.task_id)?;
        let proposal = entry
            .proposal
            .as_ref()
            .ok_or(ModelWorkflowError::NoProposal(preview.task_id))?;
        let proposal_id = proposal.id.clone();
        let claim_id = proposal.claim_id.as_str().into();
        let witness = proposal.witness.clone();
        let evidence_spans = proposal.evidence_spans.clone();
        let promotion = entry
            .chooser
            .as_mut()
            .ok_or(ModelWorkflowError::PromotionNotPlanned(preview.task_id))?
            .apply_selected(session)
            .map_err(ModelWorkflowError::Promotion)?;
        entry.generation = entry.generation.wrapping_add(1).max(1);
        entry.preview = None;
        Ok(ModelRhythmApplied {
            task_id: preview.task_id,
            proposal_id,
            claim_id,
            witness,
            evidence_spans,
            promotion,
        })
    }

    /// Retain a user's rejection beside the proposal. The evidence remains
    /// inspectable and may be reopened; no project object or model claim is
    /// deleted or negatively relabeled.
    pub fn refuse(
        &mut self,
        task_id: ModelTaskId,
        reason: impl Into<String>,
    ) -> Result<ModelDeprojectionRefusal, ModelWorkflowError> {
        let reason = reason.into();
        if reason.trim().is_empty() {
            return Err(ModelWorkflowError::EmptyRefusalReason);
        }
        let entry = self.entry_mut(task_id)?;
        ensure_editable(entry)?;
        let proposal_id = entry
            .proposal
            .as_ref()
            .map(|proposal| proposal.id.clone())
            .ok_or(ModelWorkflowError::NoProposal(task_id))?;
        entry.generation = entry.generation.wrapping_add(1).max(1);
        if let Some(chooser) = entry.chooser.as_mut() {
            chooser.clear_selection();
        }
        entry.preview = None;
        let refusal = ModelDeprojectionRefusal {
            proposal_id,
            workflow_generation: entry.generation,
            reason,
        };
        entry.refusal = Some(refusal.clone());
        Ok(refusal)
    }

    pub fn reopen(&mut self, task_id: ModelTaskId) -> Result<(), ModelWorkflowError> {
        let entry = self.entry_mut(task_id)?;
        if entry.refusal.take().is_some() {
            entry.generation = entry.generation.wrapping_add(1).max(1);
        }
        Ok(())
    }

    /// Allow an operator to retry artifact interpretation after repairing an
    /// external cache/storage problem. The immutable claim remains unchanged.
    pub fn retry_interpretation(&mut self, task_id: ModelTaskId) -> Result<(), ModelWorkflowError> {
        let entry = self.entry_mut(task_id)?;
        entry.interpreted_result_sha256 = None;
        entry.adapter_refusal = None;
        entry.generation = entry.generation.wrapping_add(1).max(1);
        Ok(())
    }

    pub fn undo_applied(
        &mut self,
        session: &mut ProjectSession,
        task_id: ModelTaskId,
    ) -> Result<crate::daw_project::ProjectRevisions, ModelWorkflowError> {
        let entry = self.entry_mut(task_id)?;
        ensure_open(entry)?;
        let revisions = entry
            .chooser
            .as_mut()
            .ok_or(ModelWorkflowError::PromotionNotPlanned(task_id))?
            .undo_applied(session)
            .map_err(ModelWorkflowError::Promotion)?;
        entry.generation = entry.generation.wrapping_add(1).max(1);
        Ok(revisions)
    }

    /// Forget only transient controller state. Worker claims and project edits
    /// live in their respective authorities and are unaffected.
    pub fn forget(&mut self, task_id: ModelTaskId) -> bool {
        self.entries.remove(&task_id).is_some()
    }

    fn entry(&self, task_id: ModelTaskId) -> Result<&WorkflowEntry, ModelWorkflowError> {
        self.entries
            .get(&task_id)
            .ok_or(ModelWorkflowError::NoProposal(task_id))
    }

    fn entry_mut(
        &mut self,
        task_id: ModelTaskId,
    ) -> Result<&mut WorkflowEntry, ModelWorkflowError> {
        self.entries
            .get_mut(&task_id)
            .ok_or(ModelWorkflowError::NoProposal(task_id))
    }
}

#[allow(clippy::too_many_arguments)]
fn phase(
    task: &ModelTaskStatus,
    has_proposal: bool,
    has_chooser: bool,
    selected: Option<RhythmPromotionChoiceId>,
    preview_generation: Option<u64>,
    applied: Option<RhythmPromotionChoiceId>,
    refusal: Option<&ModelDeprojectionRefusal>,
    adapter_refusal: Option<&str>,
) -> ModelDeprojectionPhase {
    if refusal.is_some() {
        return ModelDeprojectionPhase::Refused;
    }
    if adapter_refusal.is_some() {
        return ModelDeprojectionPhase::EvidenceRefused;
    }
    if applied.is_some() {
        return ModelDeprojectionPhase::Applied;
    }
    if preview_generation.is_some() {
        return ModelDeprojectionPhase::PreviewReady;
    }
    if selected.is_some() {
        return ModelDeprojectionPhase::AlternativeSelected;
    }
    if has_chooser {
        return ModelDeprojectionPhase::AlternativesReady;
    }
    if has_proposal {
        return ModelDeprojectionPhase::ProposalReady;
    }
    match task {
        ModelTaskStatus::Queued => ModelDeprojectionPhase::Queued,
        ModelTaskStatus::Running {
            completed_chunks,
            total_chunks,
            phase,
        } => ModelDeprojectionPhase::Running {
            completed_chunks: *completed_chunks,
            total_chunks: *total_chunks,
            phase: *phase,
        },
        ModelTaskStatus::Cancelling => ModelDeprojectionPhase::Cancelling,
        ModelTaskStatus::Published { cache_hit, .. } => ModelDeprojectionPhase::ClaimOnly {
            cache_hit: *cache_hit,
        },
        ModelTaskStatus::Cancelled => ModelDeprojectionPhase::Cancelled,
        ModelTaskStatus::Failed => ModelDeprojectionPhase::Failed,
    }
}

fn choice_view(choice: &crate::project_controller::RhythmPromotionChoice) -> ModelRhythmChoiceView {
    ModelRhythmChoiceView {
        id: choice.id,
        evidence_rank: choice.evidence_rank,
        bpm: choice.grid.bpm,
        phase_source_frame: choice.grid.phase_source_frame,
        support: choice.grid.support,
        steps_per_quarter: choice.grid.steps_per_quarter,
        diagnostic_messages: choice
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.clone())
            .collect(),
        explanation_links: choice.explanation_links.clone(),
    }
}

fn ensure_open(entry: &WorkflowEntry) -> Result<(), ModelWorkflowError> {
    if let Some(refusal) = &entry.refusal {
        Err(ModelWorkflowError::ProposalRefused(refusal.clone()))
    } else {
        Ok(())
    }
}

fn ensure_editable(entry: &WorkflowEntry) -> Result<(), ModelWorkflowError> {
    ensure_open(entry)?;
    if let Some(choice) = entry
        .chooser
        .as_ref()
        .and_then(RhythmPromotionChooser::applied_choice)
    {
        Err(ModelWorkflowError::AlreadyApplied(choice))
    } else {
        Ok(())
    }
}

#[derive(Debug)]
pub enum ModelWorkflowError {
    UnknownTask(ModelTaskId),
    NoProposal(ModelTaskId),
    PromotionNotPlanned(ModelTaskId),
    EmptyRefusalReason,
    AlreadyApplied(RhythmPromotionChoiceId),
    ProposalRefused(ModelDeprojectionRefusal),
    StaleWorkflowPreview { expected: u64, actual: u64 },
    Recipe(beat_this::BeatThisError),
    Task(TaskServiceError),
    Adapter(BeatThisDeprojectionError),
    Promotion(RhythmPromotionChooserError),
}

impl fmt::Display for ModelWorkflowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTask(id) => write!(f, "unknown model task {}", id.get()),
            Self::NoProposal(id) => write!(f, "model task {} has no rhythm proposal", id.get()),
            Self::PromotionNotPlanned(id) => {
                write!(f, "model task {} has no planned promotion", id.get())
            }
            Self::EmptyRefusalReason => f.write_str("proposal refusal needs a reason"),
            Self::AlreadyApplied(choice) => write!(
                f,
                "rhythm promotion {choice:?} is already applied; undo it before changing this workflow"
            ),
            Self::ProposalRefused(refusal) => write!(
                f,
                "proposal {} is refused: {}",
                refusal.proposal_id.as_str(),
                refusal.reason
            ),
            Self::StaleWorkflowPreview { expected, actual } => write!(
                f,
                "model workflow preview is stale: expected generation {expected}, actual {actual}"
            ),
            Self::Recipe(error) => error.fmt(f),
            Self::Task(error) => error.fmt(f),
            Self::Adapter(error) => error.fmt(f),
            Self::Promotion(error) => error.fmt(f),
        }
    }
}

impl Error for ModelWorkflowError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Recipe(error) => Some(error),
            Self::Task(error) => Some(error),
            Self::Adapter(error) => Some(error),
            Self::Promotion(error) => Some(error),
            _ => None,
        }
    }
}

impl From<BeatThisDeprojectionError> for ModelWorkflowError {
    fn from(error: BeatThisDeprojectionError) -> Self {
        Self::Adapter(error)
    }
}

impl From<TaskServiceError> for ModelWorkflowError {
    fn from(error: TaskServiceError) -> Self {
        Self::Task(error)
    }
}

impl From<RhythmPromotionChooserError> for ModelWorkflowError {
    fn from(error: RhythmPromotionChooserError) -> Self {
        Self::Promotion(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_claim::ModelClaimId;

    #[test]
    fn published_claim_is_not_reported_as_an_interpreted_proposal() {
        let published = ModelTaskStatus::Published {
            claim_id: ModelClaimId::from_cache_key("ab".repeat(32)).unwrap(),
            cache_hit: true,
        };
        assert_eq!(
            phase(&published, false, false, None, None, None, None, None),
            ModelDeprojectionPhase::ClaimOnly { cache_hit: true }
        );
        assert_eq!(
            phase(&published, true, false, None, None, None, None, None),
            ModelDeprojectionPhase::ProposalReady
        );
    }

    #[test]
    fn adapter_refusal_stays_distinct_from_worker_failure() {
        assert_eq!(
            phase(
                &ModelTaskStatus::Queued,
                false,
                false,
                None,
                None,
                None,
                None,
                Some("artifact digest mismatch")
            ),
            ModelDeprojectionPhase::EvidenceRefused
        );
    }
}
