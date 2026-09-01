//! Authoritative adoption of musical-time evidence into the constructive project.
//!
//! Rhythm analysis may propose several pulse interpretations, but none becomes
//! project tempo without an explicit authored choice. This workflow turns that
//! choice into one reversible aggregate command. It does not claim that the
//! selected pulse was the producer's tempo or rewrite later tempo changes.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use crate::command::{CommandEnvelope, DomainCommand};
use crate::live_project::{ProjectController, ProjectControllerError, ProjectControllerUpdate};
use crate::render_plan::{ExactDigest, RenderSpan};
use crate::sequencer::{BeatTime, SequencerCommand, SequencerError, Tempo};

/// Evidence retained with the user action. It is a receipt for presentation
/// and later provenance work, not a confidence claim or an instrument label.
#[derive(Clone, Debug, PartialEq)]
pub struct RhythmTempoEvidence {
    pub source_content: ExactDigest,
    pub source_span: RenderSpan,
    pub candidate_rank: usize,
    pub periodicity: f32,
    pub evidence: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AdoptTempoIntent {
    pub expected_project_revision: u64,
    pub bpm: f64,
    pub source: Option<RhythmTempoEvidence>,
}

/// Exact result shown by the shell after adoption. `adopted_bpm` reflects the
/// sequencer's integer microseconds-per-quarter representation and may differ
/// microscopically from the requested floating-point value.
#[derive(Clone, Debug, PartialEq)]
pub struct TempoAdoptionPublication {
    pub revision: u64,
    pub requested_bpm: f64,
    pub adopted_bpm: f64,
    pub previous_bpm: f64,
    pub source: Option<RhythmTempoEvidence>,
}

#[derive(Clone, Debug)]
pub enum TempoAdoptionOutcome {
    Published {
        update: ProjectControllerUpdate,
        publication: TempoAdoptionPublication,
    },
    Unchanged(TempoAdoptionPublication),
}

impl ProjectController {
    pub fn adopt_project_tempo(
        &mut self,
        intent: AdoptTempoIntent,
    ) -> Result<TempoAdoptionOutcome, TempoAdoptionError> {
        let actual_revision = self.revisions().aggregate;
        if intent.expected_project_revision != actual_revision {
            return Err(TempoAdoptionError::ProjectRevisionConflict {
                expected: intent.expected_project_revision,
                actual: actual_revision,
            });
        }
        let adopted = Tempo::from_bpm(intent.bpm)?;
        let before = self
            .snapshot()
            .project
            .state()
            .domains
            .sequencer
            .tempo_map()
            .clone();
        let previous_bpm = before.tempo_at(BeatTime::ZERO).bpm();
        let mut after = before.clone();
        after.set_tempo(BeatTime::ZERO, adopted)?;
        let publication = TempoAdoptionPublication {
            revision: actual_revision,
            requested_bpm: intent.bpm,
            adopted_bpm: adopted.bpm(),
            previous_bpm,
            source: intent.source.clone(),
        };
        if before == after {
            return Ok(TempoAdoptionOutcome::Unchanged(publication));
        }

        let source_label = intent.source.as_ref().map_or_else(String::new, |source| {
            format!(" from rhythm candidate #{}", source.candidate_rank + 1)
        });
        let update = self.execute(CommandEnvelope {
            label: format!("Adopt {:.3} BPM{source_label}", adopted.bpm()),
            base_revision: actual_revision,
            coalesce: None,
            commands: vec![DomainCommand::Sequencer(SequencerCommand::SetTempoMap {
                before,
                after,
            })],
            id_claims: BTreeSet::new(),
        })?;
        Ok(TempoAdoptionOutcome::Published {
            publication: TempoAdoptionPublication {
                revision: update.revisions().aggregate,
                ..publication
            },
            update,
        })
    }
}

#[derive(Debug)]
pub enum TempoAdoptionError {
    ProjectRevisionConflict { expected: u64, actual: u64 },
    Sequencer(SequencerError),
    Controller(ProjectControllerError),
}

impl fmt::Display for TempoAdoptionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectRevisionConflict { expected, actual } => write!(
                formatter,
                "tempo adoption expected project revision {expected}, current revision is {actual}"
            ),
            Self::Sequencer(error) => error.fmt(formatter),
            Self::Controller(error) => error.fmt(formatter),
        }
    }
}

impl Error for TempoAdoptionError {}

impl From<SequencerError> for TempoAdoptionError {
    fn from(value: SequencerError) -> Self {
        Self::Sequencer(value)
    }
}

impl From<ProjectControllerError> for TempoAdoptionError {
    fn from(value: ProjectControllerError) -> Self {
        Self::Controller(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::daw_project::DawProject;
    use crate::live_project::LiveProject;
    use crate::ontology::AuditoryIr;
    use crate::project_codecs::{decode_constructive, encode_constructive};
    use crate::project_io::ProjectFile;
    use crate::sequencer::{TempoMap, PPQ};

    fn controller() -> ProjectController {
        let project = DawProject::new("Musical time", 48_000, 120.0).unwrap();
        let live = LiveProject::from_project(project, BTreeMap::new()).unwrap();
        ProjectController::new(live).unwrap()
    }

    fn initial_bpm(controller: &ProjectController) -> f64 {
        controller
            .snapshot()
            .project
            .state()
            .domains
            .sequencer
            .tempo_map()
            .tempo_at(BeatTime::ZERO)
            .bpm()
    }

    #[test]
    fn adoption_is_one_undoable_persistent_command_and_preserves_later_time() {
        let mut controller = controller();
        let before = controller
            .snapshot()
            .project
            .state()
            .domains
            .sequencer
            .tempo_map()
            .clone();
        let mut with_later_change = before.clone();
        with_later_change
            .set_tempo(BeatTime(4 * PPQ), Tempo::from_bpm(90.0).unwrap())
            .unwrap();
        controller
            .execute(CommandEnvelope {
                label: "Add later tempo change".into(),
                base_revision: controller.revisions().aggregate,
                coalesce: None,
                commands: vec![DomainCommand::Sequencer(SequencerCommand::SetTempoMap {
                    before,
                    after: with_later_change,
                })],
                id_claims: BTreeSet::new(),
            })
            .unwrap();

        let source = RhythmTempoEvidence {
            source_content: ExactDigest::new([7; 32]),
            source_span: RenderSpan::new(0, 96_000).unwrap(),
            candidate_rank: 1,
            periodicity: 0.83,
            evidence: 0.71,
        };
        let outcome = controller
            .adopt_project_tempo(AdoptTempoIntent {
                expected_project_revision: controller.revisions().aggregate,
                bpm: 150.0,
                source: Some(source.clone()),
            })
            .unwrap();
        let TempoAdoptionOutcome::Published { publication, .. } = outcome else {
            panic!("a changed tempo must publish")
        };
        assert_eq!(publication.source, Some(source));
        assert_eq!(publication.previous_bpm, 120.0);
        assert_eq!(publication.adopted_bpm, 150.0);
        let adopted_map = controller
            .snapshot()
            .project
            .state()
            .domains
            .sequencer
            .tempo_map();
        assert_eq!(adopted_map.tempo_at(BeatTime::ZERO).bpm(), 150.0);
        assert!((adopted_map.tempo_at(BeatTime(4 * PPQ)).bpm() - 90.0).abs() < 0.001);
        assert_eq!(
            adopted_map.beat_to_frame(BeatTime(PPQ)),
            crate::sequencer::ProjectFrame(19_200)
        );

        let project = &controller.snapshot().project;
        let file = ProjectFile::from_project(project, None);
        let payloads = encode_constructive(project).unwrap();
        let reopened = decode_constructive(&file, &payloads, AuditoryIr::new(48_000)).unwrap();
        assert_eq!(
            reopened
                .state
                .domains
                .sequencer
                .tempo_map()
                .tempo_at(BeatTime::ZERO)
                .bpm(),
            150.0
        );

        controller
            .undo()
            .unwrap()
            .expect("adoption is one undo unit");
        assert_eq!(initial_bpm(&controller), 120.0);
        assert!(
            (controller
                .snapshot()
                .project
                .state()
                .domains
                .sequencer
                .tempo_map()
                .tempo_at(BeatTime(4 * PPQ))
                .bpm()
                - 90.0)
                .abs()
                < 0.001
        );
        controller.redo().unwrap().expect("adoption redoes");
        assert_eq!(initial_bpm(&controller), 150.0);
    }

    #[test]
    fn identical_and_stale_adoptions_do_not_publish() {
        let mut controller = controller();
        let revision = controller.revisions().aggregate;
        let unchanged = controller
            .adopt_project_tempo(AdoptTempoIntent {
                expected_project_revision: revision,
                bpm: 120.0,
                source: None,
            })
            .unwrap();
        assert!(matches!(unchanged, TempoAdoptionOutcome::Unchanged(_)));
        assert_eq!(controller.revisions().aggregate, revision);
        assert!(!controller.can_undo());

        let error = controller
            .adopt_project_tempo(AdoptTempoIntent {
                expected_project_revision: revision.saturating_add(1),
                bpm: 128.0,
                source: None,
            })
            .unwrap_err();
        assert!(matches!(
            error,
            TempoAdoptionError::ProjectRevisionConflict { .. }
        ));
    }

    #[test]
    fn tempo_map_type_stays_constructible_at_the_workflow_boundary() {
        let map = TempoMap::common_time(44_100, 123.0).unwrap();
        assert!((map.tempo_at(BeatTime::ZERO).bpm() - 123.0).abs() < 0.001);
    }
}
