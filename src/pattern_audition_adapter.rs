//! Cancellable work adapter from pattern recipes to the shared DAW engine.
//!
//! This module owns only control-side request freshness. Rendering delegates to
//! `compile_daw_engine`; publication and playback remain the responsibility of
//! `ProjectAudioController`/`RenderService`.

use crate::daw_engine::{compile_daw_engine, DawEngineRender};
use crate::daw_project::DawProject;
use crate::daw_render::{RenderCancellation, RenderWindow};

use super::pattern_runtime::{
    prepare_pattern_audition, PatternAuditionError, PatternAuditionPin, PatternAuditionRecipe,
    PatternAuditionRenderInputs, PatternAuditionRequest,
};

#[derive(Clone, Debug)]
pub struct PatternAuditionRenderJob {
    generation: u64,
    recipe: PatternAuditionRecipe,
    cancellation: RenderCancellation,
}

impl PatternAuditionRenderJob {
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn recipe(&self) -> &PatternAuditionRecipe {
        &self.recipe
    }

    pub fn cancellation(&self) -> RenderCancellation {
        self.cancellation.clone()
    }

    /// Exact inputs consumed by the normal project renderer. This narrow view
    /// lets an existing render worker schedule the job without knowing pattern
    /// editor state or reconstructing a symbolic event stream.
    pub fn shared_render_inputs(
        &self,
    ) -> (
        &DawProject,
        &crate::daw_engine::AssetPcmMap,
        RenderWindow,
        &crate::daw_engine::DawEngineConfig,
    ) {
        (
            &self.recipe.project,
            &self.recipe.pcm,
            render_window(&self.recipe.pin),
            &self.recipe.engine,
        )
    }

    pub fn execute(&self) -> Result<PatternAuditionRenderCompletion, PatternAuditionError> {
        if self.cancellation.is_cancelled() {
            return Err(PatternAuditionError::Cancelled);
        }
        let (project, pcm, window, engine) = self.shared_render_inputs();
        let schedule = compile_daw_engine(project, pcm, window, engine, &self.cancellation)
            .map_err(|error| {
                if self.cancellation.is_cancelled() {
                    PatternAuditionError::Cancelled
                } else {
                    PatternAuditionError::Render(error.to_string())
                }
            })?;
        let render = schedule
            .render_for_audition(&self.cancellation)
            .map_err(|error| {
                if self.cancellation.is_cancelled() {
                    PatternAuditionError::Cancelled
                } else {
                    PatternAuditionError::Render(error.to_string())
                }
            })?;
        Ok(PatternAuditionRenderCompletion {
            generation: self.generation,
            pin: self.recipe.pin.clone(),
            render,
        })
    }
}

#[derive(Clone, Debug)]
pub struct PatternAuditionRenderCompletion {
    pub generation: u64,
    pub pin: PatternAuditionPin,
    pub render: DawEngineRender,
}

impl PatternAuditionRenderCompletion {
    /// Mechanical adoption pin for
    /// `ProjectAudioController::adopt_frozen_engine_audition`.
    pub fn project_audio_pin(
        &self,
    ) -> Result<crate::project_audio_controller::ProjectAudioAuditionPin, PatternAuditionError>
    {
        let span = crate::render_plan::RenderSpan::new(
            self.pin.loop_range.start.0,
            self.pin.loop_range.end.0,
        )
        .map_err(|error| PatternAuditionError::Render(error.to_string()))?;
        Ok(crate::project_audio_controller::ProjectAudioAuditionPin {
            revision: self.pin.revisions.aggregate,
            span,
        })
    }
}

#[derive(Clone, Debug)]
struct ActiveAudition {
    generation: u64,
    revision: u64,
    cancellation: RenderCancellation,
}

/// Newest-request-wins control state. It never owns voices, a renderer, or an
/// audio device; its cancellation token is the same one consumed by the shared
/// DAW compiler and renderer.
#[derive(Clone, Debug, Default)]
pub struct PatternAuditionAdapter {
    next_generation: u64,
    active: Option<ActiveAudition>,
}

impl PatternAuditionAdapter {
    pub fn prepare(
        &mut self,
        project: &DawProject,
        request: &PatternAuditionRequest,
        inputs: PatternAuditionRenderInputs,
    ) -> Result<PatternAuditionRenderJob, PatternAuditionError> {
        let recipe = prepare_pattern_audition(project, request, inputs)?;
        if let Some(active) = self.active.take() {
            active.cancellation.cancel();
        }
        let generation = self.next_generation.max(1);
        self.next_generation = generation.saturating_add(1);
        let cancellation = RenderCancellation::new();
        self.active = Some(ActiveAudition {
            generation,
            revision: recipe.pin.revisions.aggregate,
            cancellation: cancellation.clone(),
        });
        Ok(PatternAuditionRenderJob {
            generation,
            recipe,
            cancellation,
        })
    }

    pub fn cancel(&mut self) -> bool {
        let Some(active) = self.active.take() else {
            return false;
        };
        active.cancellation.cancel();
        true
    }

    pub fn accepts(
        &self,
        completion: &PatternAuditionRenderCompletion,
        current_project_revision: u64,
    ) -> Result<(), PatternAuditionError> {
        let Some(active) = &self.active else {
            return Err(PatternAuditionError::Superseded);
        };
        if active.generation != completion.generation
            || active.cancellation.is_cancelled()
            || completion.pin.revisions.aggregate != active.revision
        {
            return Err(PatternAuditionError::Superseded);
        }
        if active.revision != current_project_revision {
            return Err(PatternAuditionError::StaleRevision {
                expected: active.revision,
                actual: current_project_revision,
            });
        }
        Ok(())
    }

    pub fn finish(
        &mut self,
        completion: PatternAuditionRenderCompletion,
        current_project_revision: u64,
    ) -> Result<PatternAuditionRenderCompletion, PatternAuditionError> {
        if let Err(error) = self.accepts(&completion, current_project_revision) {
            if self
                .active
                .as_ref()
                .is_some_and(|active| active.generation == completion.generation)
            {
                self.cancel();
            }
            return Err(error);
        }
        self.active = None;
        Ok(completion)
    }
}

fn render_window(pin: &PatternAuditionPin) -> RenderWindow {
    RenderWindow {
        start: pin.loop_range.start.0,
        end: pin.loop_range.end.0,
    }
}
