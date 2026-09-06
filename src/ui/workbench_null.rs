//! Audible diffs: new-minus-old between render cohorts.
//!
//! One press subtracts the retired cohort's master from the active one over the
//! loop and auditions the result; a second press stops it. Everything the
//! subtraction knows lives in `ProjectAudioController`; this is the shell's
//! half — find the host, say what happened, publish the status.

use gpui::Context;

use super::Workbench;
use crate::project_audio_controller::ProjectAudioDiffOutcome;

impl Workbench {
    pub(super) fn audition_diff(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.audio.as_ref() else {
            self.constructive_status =
                Some("Audition diff · the project audio host is not open yet".into());
            cx.notify();
            return;
        };
        self.constructive_status = Some(match self.audio_controller.audition_diff(host) {
            Ok(ProjectAudioDiffOutcome::Started {
                span,
                rms_in_loop,
                rms_outside_loop,
            }) => format!(
                "Diff · new minus old over {}..{} · RMS {rms_in_loop:.4} inside, {rms_outside_loop:.4} outside",
                span.start, span.end
            ),
            Ok(ProjectAudioDiffOutcome::Stopped) => {
                "Diff stopped · the master is audible again".into()
            }
            Err(error) => format!("Audition diff · {error}"),
        });
        self.publish_audio_status(cx);
        cx.notify();
    }
}
