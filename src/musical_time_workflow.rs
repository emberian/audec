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
use crate::sequencer::{BeatTime, SequencerCommand, SequencerError, Tempo, TimeSignature};

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

/// One authored point in the project's tempo map, at a tick the map itself
/// reports as a bar start ([`crate::sequencer::TempoMap::bar_start`]) or as
/// the start of the segment being nudged
/// ([`crate::sequencer::TempoMap::tempo_segment_start`]). An editor never
/// invents a position; it asks the map where the playhead is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TempoPointIntent {
    pub expected_project_revision: u64,
    pub at: BeatTime,
    pub bpm: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeterPointIntent {
    pub expected_project_revision: u64,
    pub at: BeatTime,
    pub signature: TimeSignature,
}

/// What a planned tempo point means, in the words a status line uses. `bar`
/// is one-based, the way the ruler labels bars.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TempoPointPublication {
    pub at: BeatTime,
    pub bar: i64,
    pub previous_bpm: f64,
    pub adopted_bpm: f64,
    /// The map already carried a tempo point exactly here, so this is an edit
    /// of an existing change rather than a new one.
    pub existing_point: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeterPointPublication {
    pub at: BeatTime,
    pub bar: i64,
    pub previous: TimeSignature,
    pub adopted: TimeSignature,
    pub existing_point: bool,
}

/// A planned musical-time edit. The controller plans; the session that owns
/// receipt publication executes, through the same `execute_envelope` every
/// other durable edit uses. A plan that changes nothing carries no envelope,
/// so "already marked" can never be reported as an edit.
#[derive(Clone, Debug)]
pub enum MusicalPointPlan<P> {
    Unchanged(P),
    Change {
        envelope: CommandEnvelope,
        publication: P,
    },
}

impl<P> MusicalPointPlan<P> {
    pub fn publication(&self) -> &P {
        match self {
            Self::Unchanged(publication) => publication,
            Self::Change { publication, .. } => publication,
        }
    }
}

impl ProjectController {
    /// Place (or replace) the tempo point at `at`, carrying `bpm`. The whole
    /// map travels as one `SetTempoMap`, so undo, coalescing, the wire codec
    /// and the package codec need nothing new.
    pub fn plan_tempo_point(
        &self,
        intent: TempoPointIntent,
    ) -> Result<MusicalPointPlan<TempoPointPublication>, MusicalPointError> {
        let (before, base_revision) = self.musical_time_base(intent.expected_project_revision)?;
        let tempo = Tempo::from_bpm(intent.bpm)?;
        let publication = TempoPointPublication {
            at: intent.at,
            bar: before.musical_position(intent.at).bar + 1,
            previous_bpm: before.tempo_at(intent.at).bpm(),
            adopted_bpm: tempo.bpm(),
            existing_point: before
                .tempo_points()
                .iter()
                .any(|point| point.at == intent.at),
        };
        let mut after = before.clone();
        after.set_tempo(intent.at, tempo)?;
        if before == after {
            return Ok(MusicalPointPlan::Unchanged(publication));
        }
        Ok(MusicalPointPlan::Change {
            envelope: CommandEnvelope {
                label: format!("Tempo {:.3} BPM at bar {}", tempo.bpm(), publication.bar),
                base_revision,
                coalesce: None,
                commands: vec![DomainCommand::Sequencer(SequencerCommand::SetTempoMap {
                    before,
                    after,
                })],
                id_claims: BTreeSet::new(),
            },
            publication,
        })
    }

    /// Place (or replace) the meter point at `at`. `TempoMap::set_meter` is
    /// the authority on where a meter may change; its refusal is returned
    /// unchanged so the musician reads the map's reason, not a paraphrase.
    pub fn plan_meter_point(
        &self,
        intent: MeterPointIntent,
    ) -> Result<MusicalPointPlan<MeterPointPublication>, MusicalPointError> {
        let (before, base_revision) = self.musical_time_base(intent.expected_project_revision)?;
        let publication = MeterPointPublication {
            at: intent.at,
            bar: before.musical_position(intent.at).bar + 1,
            previous: before.meter_at(intent.at),
            adopted: intent.signature,
            existing_point: before
                .meter_points()
                .iter()
                .any(|point| point.at == intent.at),
        };
        let mut after = before.clone();
        after.set_meter(intent.at, intent.signature)?;
        if before == after {
            return Ok(MusicalPointPlan::Unchanged(publication));
        }
        Ok(MusicalPointPlan::Change {
            envelope: CommandEnvelope {
                label: format!(
                    "Time signature {}/{} at bar {}",
                    intent.signature.numerator, intent.signature.denominator, publication.bar
                ),
                base_revision,
                coalesce: None,
                commands: vec![DomainCommand::Sequencer(SequencerCommand::SetTempoMap {
                    before,
                    after,
                })],
                id_claims: BTreeSet::new(),
            },
            publication,
        })
    }

    fn musical_time_base(
        &self,
        expected_project_revision: u64,
    ) -> Result<(crate::sequencer::TempoMap, u64), MusicalPointError> {
        let actual = self.revisions().aggregate;
        if expected_project_revision != actual {
            return Err(MusicalPointError::ProjectRevisionConflict {
                expected: expected_project_revision,
                actual,
            });
        }
        Ok((
            self.snapshot()
                .project
                .state()
                .domains
                .sequencer
                .tempo_map()
                .clone(),
            actual,
        ))
    }
}

#[derive(Debug)]
pub enum MusicalPointError {
    /// No project is installed in the session the editor asked.
    NoProject,
    ProjectRevisionConflict {
        expected: u64,
        actual: u64,
    },
    Sequencer(SequencerError),
}

impl fmt::Display for MusicalPointError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProject => formatter.write_str("no project is open"),
            Self::ProjectRevisionConflict { expected, actual } => write!(
                formatter,
                "musical-time edit expected project revision {expected}, current revision is {actual}"
            ),
            Self::Sequencer(error) => error.fmt(formatter),
        }
    }
}

impl Error for MusicalPointError {}

impl From<SequencerError> for MusicalPointError {
    fn from(value: SequencerError) -> Self {
        Self::Sequencer(value)
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

    fn tempo_map(controller: &ProjectController) -> crate::sequencer::TempoMap {
        controller
            .snapshot()
            .project
            .state()
            .domains
            .sequencer
            .tempo_map()
            .clone()
    }

    fn apply<P>(controller: &mut ProjectController, plan: MusicalPointPlan<P>) -> P {
        match plan {
            MusicalPointPlan::Change {
                envelope,
                publication,
            } => {
                controller.execute(envelope).unwrap();
                publication
            }
            MusicalPointPlan::Unchanged(_) => panic!("this plan was expected to change the map"),
        }
    }

    #[test]
    fn a_tempo_point_at_a_later_bar_is_one_undoable_command_and_spares_bar_one() {
        let mut controller = controller();
        let bar_five = tempo_map(&controller).bar_start(BeatTime(17 * PPQ + 40));
        assert_eq!(bar_five, BeatTime(16 * PPQ));
        let plan = controller
            .plan_tempo_point(TempoPointIntent {
                expected_project_revision: controller.revisions().aggregate,
                at: bar_five,
                bpm: 90.0,
            })
            .unwrap();
        let publication = apply(&mut controller, plan);
        assert_eq!(publication.bar, 5);
        assert!(!publication.existing_point);
        assert_eq!(publication.previous_bpm, 120.0);

        let map = tempo_map(&controller);
        assert_eq!(map.tempo_at(BeatTime::ZERO).bpm(), 120.0);
        assert!((map.tempo_at(bar_five).bpm() - 90.0).abs() < 0.001);
        // One command: the whole map travels in a single `SetTempoMap`.
        controller.undo().unwrap().expect("one undo unit");
        assert_eq!(tempo_map(&controller).tempo_points().len(), 1);
        controller.redo().unwrap().expect("one redo unit");
        assert_eq!(tempo_map(&controller).tempo_points().len(), 2);
    }

    #[test]
    fn marking_a_bar_that_already_carries_a_tempo_point_plans_nothing() {
        let mut controller = controller();
        let bar_five = BeatTime(16 * PPQ);
        let plan = controller
            .plan_tempo_point(TempoPointIntent {
                expected_project_revision: controller.revisions().aggregate,
                at: bar_five,
                bpm: 90.0,
            })
            .unwrap();
        apply(&mut controller, plan);
        let revision = controller.revisions().aggregate;
        let again = controller
            .plan_tempo_point(TempoPointIntent {
                expected_project_revision: revision,
                at: bar_five,
                bpm: 90.0,
            })
            .unwrap();
        assert!(matches!(again, MusicalPointPlan::Unchanged(_)));
        assert!(again.publication().existing_point);
        assert_eq!(controller.revisions().aggregate, revision);
    }

    #[test]
    fn nudging_the_segment_under_the_playhead_leaves_the_opening_tempo_alone() {
        let mut controller = controller();
        let plan = controller
            .plan_tempo_point(TempoPointIntent {
                expected_project_revision: controller.revisions().aggregate,
                at: BeatTime(16 * PPQ),
                bpm: 90.0,
            })
            .unwrap();
        apply(&mut controller, plan);
        // A playhead deep inside the second segment nudges that segment, the
        // way the transport bar does, not tick zero.
        let map = tempo_map(&controller);
        let playhead = BeatTime(23 * PPQ + 100);
        let segment = map.tempo_segment_start(playhead);
        assert_eq!(segment, BeatTime(16 * PPQ));
        let plan = controller
            .plan_tempo_point(TempoPointIntent {
                expected_project_revision: controller.revisions().aggregate,
                at: segment,
                bpm: map.tempo_at(segment).bpm() + 1.0,
            })
            .unwrap();
        let publication = apply(&mut controller, plan);
        assert!(publication.existing_point);
        let map = tempo_map(&controller);
        assert_eq!(map.tempo_at(BeatTime::ZERO).bpm(), 120.0);
        assert!((map.tempo_at(segment).bpm() - 91.0).abs() < 0.01);
        assert_eq!(map.tempo_points().len(), 2);
        // With one segment the same query is tick zero, which is exactly the
        // behaviour the transport buttons had before they learned the map.
        let plain = TempoMap::common_time(48_000, 120.0).unwrap();
        assert_eq!(
            plain.tempo_segment_start(BeatTime(99 * PPQ)),
            BeatTime::ZERO
        );
    }

    #[test]
    fn a_meter_point_moves_the_bar_grid_after_it_and_survives_a_reopen() {
        let mut controller = controller();
        let bar_five = tempo_map(&controller).bar_start(BeatTime(16 * PPQ + 7));
        let plan = controller
            .plan_meter_point(MeterPointIntent {
                expected_project_revision: controller.revisions().aggregate,
                at: bar_five,
                signature: TimeSignature::new(3, 4).unwrap(),
            })
            .unwrap();
        let publication = apply(&mut controller, plan);
        assert_eq!(publication.bar, 5);
        assert_eq!(publication.previous, TimeSignature::new(4, 4).unwrap());
        let map = tempo_map(&controller);
        assert_eq!(map.meter_at(bar_five), TimeSignature::new(3, 4).unwrap());
        // Bar six now begins three quarters later, not four.
        assert_eq!(map.next_bar_start(bar_five), BeatTime(19 * PPQ));
        assert_eq!(map.musical_position(BeatTime(19 * PPQ)).bar, 5);

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
                .meter_points()
                .len(),
            2
        );
    }

    #[test]
    fn a_meter_change_off_the_bar_line_is_refused_with_the_maps_own_reason() {
        let controller = controller();
        let error = controller
            .plan_meter_point(MeterPointIntent {
                expected_project_revision: controller.revisions().aggregate,
                at: BeatTime(PPQ),
                signature: TimeSignature::new(7, 8).unwrap(),
            })
            .unwrap_err();
        assert!(matches!(
            error,
            MusicalPointError::Sequencer(SequencerError::MeterChangeNotAtBar)
        ));
        assert_eq!(
            error.to_string(),
            SequencerError::MeterChangeNotAtBar.to_string()
        );
    }

    #[test]
    fn tempo_map_type_stays_constructible_at_the_workflow_boundary() {
        let map = TempoMap::common_time(44_100, 123.0).unwrap();
        assert!((map.tempo_at(BeatTime::ZERO).bpm() - 123.0).abs() < 0.001);
    }
}
