//! Host-facing orchestration for shared pattern audition.
//!
//! The adapter prepares immutable work on the UI thread, hands a cloneable job
//! to a worker, and adopts only the newest completion into the existing project
//! audio controller. It owns neither a renderer nor an audio device.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::audio_host::AudioHost;
use crate::daw_engine::DawEngineConfig;
use crate::mixer::ProcessorId;
use crate::project_audio_controller::{
    AuditionAlignment, ProjectAudioController, ProjectAudioControllerError,
};
use crate::project_session::{ProjectSession, ProjectSessionError};
use crate::render_runtime::{
    AuditionMix, AuditionOwner, AuditionSubject, TimelineAudition, TimelineAuditionId,
};

use super::{
    PatternAuditionAdapter, PatternAuditionError, PatternAuditionRenderCompletion,
    PatternAuditionRenderInputs, PatternAuditionRenderJob, PatternAuditionRequest,
    PatternAuditionScope,
};

const DIAGNOSTIC_PREFIX: &str = "Pattern audition: ";

/// Exact semantic request emitted by piano/step/cycle surfaces. The host adds
/// owner and mix policy; the view never lowers this to MIDI or guessed PCM.
pub type SharedPatternAuditionCallback =
    Arc<dyn Fn(PatternAuditionRequest) + Send + Sync + 'static>;

#[derive(Clone, Debug)]
pub struct PatternAuditionSessionInputs {
    pub engine: Arc<DawEngineConfig>,
    pub plugin_instruments: BTreeMap<u64, ProcessorId>,
}

impl PatternAuditionSessionInputs {
    pub fn new(engine: Arc<DawEngineConfig>) -> Self {
        Self {
            engine,
            plugin_instruments: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PatternAuditionAdoption {
    pub owner: AuditionOwner,
    pub subject: AuditionSubject,
    pub mix: AuditionMix,
    pub alignment: AuditionAlignment,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternAuditionStartRequest {
    pub audition: PatternAuditionRequest,
    pub adoption: PatternAuditionAdoption,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternAuditionSessionStatus {
    Idle,
    Preparing {
        generation: u64,
        revision: u64,
        scope: PatternAuditionScope,
    },
    Active {
        generation: u64,
        audition: TimelineAuditionId,
    },
    Refused {
        message: String,
    },
}

impl Default for PatternAuditionSessionStatus {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Clone, Debug)]
struct ActiveSessionRequest {
    generation: u64,
    document_generation: u64,
    revision: u64,
}

#[derive(Clone, Debug)]
pub struct PatternAuditionSessionJob {
    document_generation: u64,
    adoption: PatternAuditionAdoption,
    render: PatternAuditionRenderJob,
}

impl PatternAuditionSessionJob {
    pub const fn generation(&self) -> u64 {
        self.render.generation()
    }

    pub fn cancellation(&self) -> crate::daw_render::RenderCancellation {
        self.render.cancellation()
    }

    /// Worker-sized entry point. It always returns a tagged result so a late
    /// failure from a superseded job cannot cancel a newer request.
    pub fn execute(self) -> PatternAuditionSessionWorkResult {
        let generation = self.render.generation();
        let result = self.render.execute();
        PatternAuditionSessionWorkResult {
            generation,
            document_generation: self.document_generation,
            adoption: self.adoption,
            result,
        }
    }
}

#[derive(Debug)]
pub struct PatternAuditionSessionWorkResult {
    pub generation: u64,
    pub document_generation: u64,
    pub adoption: PatternAuditionAdoption,
    pub result: Result<PatternAuditionRenderCompletion, PatternAuditionError>,
}

#[derive(Clone, Debug, Default)]
pub struct PatternAuditionSessionAdapter {
    audition: PatternAuditionAdapter,
    active: Option<ActiveSessionRequest>,
    status: PatternAuditionSessionStatus,
    diagnostic: Option<String>,
}

impl PatternAuditionSessionAdapter {
    pub fn status(&self) -> &PatternAuditionSessionStatus {
        &self.status
    }

    pub fn diagnostic(&self) -> Option<&str> {
        self.diagnostic.as_deref()
    }

    /// UI-thread phase: freeze the current session publication and exact PCM.
    pub fn prepare(
        &mut self,
        session: &mut ProjectSession,
        request: PatternAuditionStartRequest,
        inputs: PatternAuditionSessionInputs,
    ) -> Result<PatternAuditionSessionJob, PatternAuditionSessionError> {
        self.cancel_for_project_replacement(session);
        if self.active.take().is_some() {
            self.audition.cancel();
        }
        let document_generation = session.document_generation();
        let snapshot = session.project_snapshot()?.clone();
        let revision = snapshot.revisions().aggregate;
        let render_inputs = PatternAuditionRenderInputs {
            pcm: Arc::clone(&snapshot.pcm),
            engine: inputs.engine,
            plugin_instruments: inputs.plugin_instruments,
        };
        let render =
            match self
                .audition
                .prepare(&snapshot.project, &request.audition, render_inputs)
            {
                Ok(render) => render,
                Err(error) => {
                    self.refuse(session, error.to_string());
                    return Err(error.into());
                }
            };
        let generation = render.generation();
        self.active = Some(ActiveSessionRequest {
            generation,
            document_generation,
            revision,
        });
        self.status = PatternAuditionSessionStatus::Preparing {
            generation,
            revision,
            scope: request.audition.scope.clone(),
        };
        self.diagnostic = None;
        publish_pattern_diagnostic(session, None);
        Ok(PatternAuditionSessionJob {
            document_generation,
            adoption: request.adoption,
            render,
        })
    }

    /// UI-thread completion phase: freshness check, shared-controller adoption,
    /// and status publication are one indivisible callback-sized operation.
    pub fn complete(
        &mut self,
        session: &mut ProjectSession,
        audio: &mut ProjectAudioController,
        host: &AudioHost,
        work: PatternAuditionSessionWorkResult,
    ) -> Result<Arc<TimelineAudition>, PatternAuditionSessionError> {
        let Some(active) = self.active.clone() else {
            return Err(PatternAuditionSessionError::Superseded);
        };
        if active.generation != work.generation
            || active.document_generation != work.document_generation
        {
            return Err(PatternAuditionSessionError::Superseded);
        }
        if session.document_generation() != active.document_generation {
            self.cancel_for_project_replacement(session);
            return Err(PatternAuditionSessionError::ProjectReplaced);
        }
        let actual_revision = session.project_snapshot()?.revisions().aggregate;
        if actual_revision != active.revision {
            self.audition.cancel();
            self.active = None;
            let error = PatternAuditionSessionError::StaleRevision {
                expected: active.revision,
                actual: actual_revision,
            };
            self.refuse(session, error.to_string());
            return Err(error);
        }
        let completion = match work.result {
            Ok(completion) => completion,
            Err(error) => {
                self.audition.cancel();
                self.active = None;
                self.refuse(session, error.to_string());
                return Err(error.into());
            }
        };
        self.audition
            .accepts(&completion, actual_revision)
            .map_err(PatternAuditionSessionError::Pattern)?;
        let pin = completion.project_audio_pin()?;
        let audition = match audio.adopt_frozen_engine_audition(
            host,
            pin,
            &completion.render,
            work.adoption.owner,
            work.adoption.subject,
            work.adoption.mix,
            work.adoption.alignment,
        ) {
            Ok(audition) => audition,
            Err(error) => {
                self.audition.cancel();
                self.active = None;
                self.refuse_with_audio(session, audio, error.to_string());
                return Err(error.into());
            }
        };
        let completion = self
            .audition
            .finish(completion, actual_revision)
            .map_err(PatternAuditionSessionError::Pattern)?;
        self.active = None;
        self.status = PatternAuditionSessionStatus::Active {
            generation: completion.generation,
            audition: audition.id,
        };
        self.diagnostic = None;
        audio.publish_session_state(session);
        Ok(audition)
    }

    pub fn stop(
        &mut self,
        session: &mut ProjectSession,
        audio: &mut ProjectAudioController,
        owner: AuditionOwner,
    ) -> Result<(), PatternAuditionSessionError> {
        self.audition.cancel();
        self.active = None;
        if let Err(error) = audio.stop_scoped_audition(owner) {
            self.refuse_with_audio(session, audio, error.to_string());
            return Err(error.into());
        }
        self.status = PatternAuditionSessionStatus::Idle;
        self.diagnostic = None;
        audio.publish_session_state(session);
        Ok(())
    }

    /// Call when a document lifecycle event is observed. Returns true only
    /// when an in-flight job was cancelled because its publication vanished.
    pub fn cancel_for_project_replacement(&mut self, session: &mut ProjectSession) -> bool {
        let replaced = self.active.as_ref().is_some_and(|active| {
            session.document_generation() != active.document_generation
                || session
                    .snapshot()
                    .revisions()
                    .is_none_or(|revisions| revisions.aggregate != active.revision)
        });
        if !replaced {
            return false;
        }
        self.audition.cancel();
        self.active = None;
        self.status = PatternAuditionSessionStatus::Refused {
            message: "Project was replaced while pattern audition was rendering".into(),
        };
        self.diagnostic = Some("Project was replaced while pattern audition was rendering".into());
        publish_pattern_diagnostic(session, self.diagnostic.as_deref());
        true
    }

    fn refuse(&mut self, session: &mut ProjectSession, message: String) {
        self.status = PatternAuditionSessionStatus::Refused {
            message: message.clone(),
        };
        self.diagnostic = Some(message);
        publish_pattern_diagnostic(session, self.diagnostic.as_deref());
    }

    fn refuse_with_audio(
        &mut self,
        session: &mut ProjectSession,
        audio: &ProjectAudioController,
        message: String,
    ) {
        self.status = PatternAuditionSessionStatus::Refused {
            message: message.clone(),
        };
        self.diagnostic = Some(message);
        session.set_audio_status(audio.status());
        let mut diagnostics = audio.diagnostics().to_vec();
        diagnostics.push(format!(
            "{DIAGNOSTIC_PREFIX}{}",
            self.diagnostic.as_deref().unwrap_or("refused")
        ));
        session.replace_diagnostics(diagnostics);
    }
}

fn publish_pattern_diagnostic(session: &mut ProjectSession, diagnostic: Option<&str>) {
    let mut diagnostics = session
        .diagnostics()
        .iter()
        .filter(|message| !message.starts_with(DIAGNOSTIC_PREFIX))
        .cloned()
        .collect::<Vec<_>>();
    if let Some(diagnostic) = diagnostic {
        diagnostics.push(format!("{DIAGNOSTIC_PREFIX}{diagnostic}"));
    }
    session.replace_diagnostics(diagnostics);
}

#[derive(Debug)]
pub enum PatternAuditionSessionError {
    Session(ProjectSessionError),
    Pattern(PatternAuditionError),
    Audio(ProjectAudioControllerError),
    Superseded,
    ProjectReplaced,
    StaleRevision { expected: u64, actual: u64 },
}

impl fmt::Display for PatternAuditionSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(error) => write!(formatter, "pattern audition session failed: {error}"),
            Self::Pattern(error) => write!(formatter, "pattern audition failed: {error}"),
            Self::Audio(error) => write!(formatter, "pattern audition adoption failed: {error}"),
            Self::Superseded => formatter.write_str("pattern audition completion was superseded"),
            Self::ProjectReplaced => {
                formatter.write_str("project was replaced during pattern audition")
            }
            Self::StaleRevision { expected, actual } => write!(
                formatter,
                "pattern audition revision {expected} is stale; current revision is {actual}"
            ),
        }
    }
}

impl Error for PatternAuditionSessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Session(error) => Some(error),
            Self::Pattern(error) => Some(error),
            Self::Audio(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ProjectSessionError> for PatternAuditionSessionError {
    fn from(error: ProjectSessionError) -> Self {
        Self::Session(error)
    }
}

impl From<PatternAuditionError> for PatternAuditionSessionError {
    fn from(error: PatternAuditionError) -> Self {
        Self::Pattern(error)
    }
}

impl From<ProjectAudioControllerError> for PatternAuditionSessionError {
    fn from(error: ProjectAudioControllerError) -> Self {
        Self::Audio(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use crate::arrangement::Frame;
    use crate::arrangement_interaction::{ArrangementEdit, ArrangementEditIntent, GestureCommit};
    use crate::arrangement_view::{
        ArrangementAction, ArrangementActionIntent, ArrangementViewEvent,
    };
    use crate::daw_engine::{BuiltInInstrumentDefinition, BuiltInInstrumentRoute};
    use crate::daw_project::DawProject;
    use crate::instruments::{SynthParams, Waveform};
    use crate::live_project::LiveProject;
    use crate::pattern_actions::{
        CreatePatternIntent, PatternAction, PatternActionIntent, PatternEdit, PatternEditIntent,
        PatternEditorMode,
    };
    use crate::pattern_authoring::{DivergedOverwrite, ExpressionRealizationContext};
    use crate::pattern_use_graph::{PatternOccurrenceTarget, PatternUseGraph, PatternUseSnapshot};
    use crate::project_controller::{
        lower_arrangement_event, lower_gesture, ArrangementDispatch, PatternAuditionPad,
        PatternAuditionSelection, PatternWorkflowIntent, PatternWorkflowOutcome,
    };
    use crate::project_session::ProjectSessionId;
    use crate::sequencer::{BeatDuration, TriggerTarget, PPQ};
    use crate::ui_drag::DropIntent;

    fn session_with_alternation() -> (ProjectSession, PatternOccurrenceTarget) {
        let project = DawProject::new("Pattern session audition", 48_000, 120.0).unwrap();
        let live = LiveProject::from_project(project, BTreeMap::new()).unwrap();
        let mut session = ProjectSession::new(ProjectSessionId(91)).unwrap();
        session.install(live, None).unwrap();
        let revision = session.project_snapshot().unwrap().revisions().aggregate;
        let create = session
            .execute_pattern_workflow(PatternWorkflowIntent::Action(PatternActionIntent {
                expected_project_revision: revision,
                action: PatternAction::Create(CreatePatternIntent {
                    mode: PatternEditorMode::Steps,
                    name: "Alternating audition".into(),
                    length: BeatDuration((PPQ * 4) as u64),
                    step_resolution: BeatDuration((PPQ / 4) as u64),
                    initial_target: None,
                }),
            }))
            .unwrap();
        let PatternWorkflowOutcome::Published { publication, .. } = create else {
            panic!("create must publish")
        };
        let pattern = publication.pattern;
        let pattern_revision = publication.definition.unwrap().revision;
        let revision = session.project_snapshot().unwrap().revisions().aggregate;
        session
            .execute_pattern_workflow(PatternWorkflowIntent::Action(PatternActionIntent {
                expected_project_revision: revision,
                action: PatternAction::Edit(PatternEditIntent {
                    pattern,
                    expected_pattern_revision: pattern_revision,
                    edit: PatternEdit::ApplyExpression {
                        source: "<a b>".into(),
                        bindings: BTreeMap::from([
                            (
                                "a".into(),
                                TriggerTarget::InstrumentNote {
                                    instrument: 11,
                                    key: 48,
                                },
                            ),
                            (
                                "b".into(),
                                TriggerTarget::InstrumentNote {
                                    instrument: 22,
                                    key: 72,
                                },
                            ),
                        ]),
                        overwrite: DivergedOverwrite::Refuse,
                        realization: ExpressionRealizationContext::default(),
                    },
                }),
            }))
            .unwrap();

        let snapshot = session.project_snapshot().unwrap().clone();
        let drop = ArrangementViewEvent::Action(ArrangementActionIntent {
            expected_revision: snapshot.revisions().aggregate,
            action: ArrangementAction::Drop(DropIntent::InsertPattern {
                pattern,
                track: None,
                at: Frame::ZERO,
                make_unique: false,
            }),
        });
        let ArrangementDispatch::Apply(drop) = lower_arrangement_event(&snapshot, drop).unwrap()
        else {
            panic!("drop must apply")
        };
        session.execute(drop.envelope).unwrap();
        let occurrence = PatternUseGraph::build(PatternUseSnapshot::from_project(
            &session.project_snapshot().unwrap().project,
        ))
        .unwrap()
        .pattern(pattern)
        .unwrap()
        .occurrences[0]
            .clone();
        let boundary = Frame(
            occurrence
                .placement
                .start
                .0
                .checked_add((occurrence.placement.len() as i64) * 2)
                .unwrap(),
        );
        let repeat = GestureCommit {
            selection: None,
            edit: Some(ArrangementEditIntent {
                expected_revision: session.project_snapshot().unwrap().revisions().aggregate,
                edit: ArrangementEdit::SetRepeatBoundary {
                    clip_id: occurrence.target.arrangement_clip,
                    boundary,
                },
            }),
        };
        let snapshot = session.project_snapshot().unwrap().clone();
        let ArrangementDispatch::Apply(repeat) = lower_gesture(&snapshot, repeat).unwrap() else {
            panic!("repeat must apply")
        };
        session.execute(repeat.envelope).unwrap();
        let target = PatternUseGraph::build(PatternUseSnapshot::from_project(
            &session.project_snapshot().unwrap().project,
        ))
        .unwrap()
        .occurrence_for_clip(occurrence.target.arrangement_clip)
        .unwrap()
        .target;
        (session, target)
    }

    fn inputs(session: &ProjectSession) -> PatternAuditionSessionInputs {
        let master = session
            .project_snapshot()
            .unwrap()
            .project
            .state()
            .domains
            .mixer
            .master();
        let mut saw = SynthParams::default();
        saw.waveform = Waveform::Saw;
        let mut sine = SynthParams::default();
        sine.waveform = Waveform::Sine;
        let mut engine = DawEngineConfig::default();
        engine.instruments.insert(
            11,
            BuiltInInstrumentRoute {
                definition: BuiltInInstrumentDefinition::Subtractive(saw),
                bus: master,
            },
        );
        engine.instruments.insert(
            22,
            BuiltInInstrumentRoute {
                definition: BuiltInInstrumentDefinition::Subtractive(sine),
                bus: master,
            },
        );
        PatternAuditionSessionInputs::new(Arc::new(engine))
    }

    fn start(
        session: &ProjectSession,
        occurrence: PatternOccurrenceTarget,
        cycle_index: u64,
    ) -> PatternAuditionStartRequest {
        PatternAuditionStartRequest {
            audition: PatternAuditionRequest {
                expected_project_revision: session
                    .project_snapshot()
                    .unwrap()
                    .revisions()
                    .aggregate,
                occurrence,
                cycle_index,
                performance_seed: 19,
                scope: PatternAuditionScope::Pattern,
            },
            adoption: PatternAuditionAdoption {
                owner: AuditionOwner {
                    namespace: 77,
                    local: 1,
                },
                subject: AuditionSubject::Construction,
                mix: AuditionMix::Replace,
                alignment: AuditionAlignment::LoopSpan { play: true },
            },
        }
    }

    #[test]
    fn session_jobs_render_non_silent_distinctive_cycle_pcm() {
        let (mut session, occurrence) = session_with_alternation();
        let mut first_adapter = PatternAuditionSessionAdapter::default();
        let first_inputs = inputs(&session);
        let first_request = start(&session, occurrence, 0);
        let first = first_adapter
            .prepare(&mut session, first_request, first_inputs)
            .unwrap()
            .execute()
            .result
            .unwrap();

        let mut second_adapter = PatternAuditionSessionAdapter::default();
        let second_inputs = inputs(&session);
        let second_request = start(&session, occurrence, 1);
        let second = second_adapter
            .prepare(&mut session, second_request, second_inputs)
            .unwrap()
            .execute()
            .result
            .unwrap();
        let first_pcm = first.render.audio.interleaved();
        let second_pcm = second.render.audio.interleaved();
        assert!(first_pcm.iter().any(|sample| sample.abs() > 1.0e-4));
        assert!(second_pcm.iter().any(|sample| sample.abs() > 1.0e-4));
        assert_ne!(first_pcm, second_pcm);
    }

    #[test]
    fn supersession_and_project_replacement_cancel_session_jobs() {
        let (mut session, occurrence) = session_with_alternation();
        let mut adapter = PatternAuditionSessionAdapter::default();
        let old_inputs = inputs(&session);
        let old_request = start(&session, occurrence, 0);
        let old = adapter
            .prepare(&mut session, old_request, old_inputs)
            .unwrap();
        let current_inputs = inputs(&session);
        let current_request = start(&session, occurrence, 1);
        let current = adapter
            .prepare(&mut session, current_request, current_inputs)
            .unwrap();
        assert!(matches!(
            old.execute().result,
            Err(PatternAuditionError::Cancelled)
        ));

        let replacement = DawProject::new("Replacement", 48_000, 120.0).unwrap();
        session
            .install(
                LiveProject::from_project(replacement, BTreeMap::new()).unwrap(),
                None,
            )
            .unwrap();
        assert!(adapter.cancel_for_project_replacement(&mut session));
        assert!(matches!(
            current.execute().result,
            Err(PatternAuditionError::Cancelled)
        ));
        assert!(session
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.contains("Project was replaced")));
    }

    #[test]
    fn exact_callback_type_accepts_note_and_pad_scopes_without_lowering() {
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&received);
        let callback: SharedPatternAuditionCallback = Arc::new(move |request| {
            captured.lock().unwrap().push(request.scope);
        });
        let occurrence = PatternOccurrenceTarget {
            arrangement_clip: crate::arrangement::ClipId::from_raw(1),
            sequencer_clip: crate::sequencer::PatternClipId::from_raw(2),
            pattern: crate::sequencer::PatternId::from_raw(3),
        };
        callback(PatternAuditionRequest {
            expected_project_revision: 4,
            occurrence,
            cycle_index: 0,
            performance_seed: 0,
            scope: PatternAuditionScope::Selection(PatternAuditionSelection::Notes(
                BTreeSet::from([crate::sequencer::NoteId::from_raw(5)]),
            )),
        });
        callback(PatternAuditionRequest {
            expected_project_revision: 4,
            occurrence,
            cycle_index: 0,
            performance_seed: 0,
            scope: PatternAuditionScope::Pad(PatternAuditionPad {
                lane: crate::sequencer::StepLaneId::from_raw(6),
                target: TriggerTarget::Sample(crate::sequencer::SampleAssetId::from_raw(7)),
            }),
        });
        assert_eq!(received.lock().unwrap().len(), 2);
    }
}
