//! Audible diffs: new-minus-old between render cohorts.
//!
//! Cycle 3 (C3-Null) fills this in. Until then the action refuses by name
//! rather than pretending.

use gpui::Context;

use super::Workbench;

impl Workbench {
    pub(super) fn audition_diff(&mut self, cx: &mut Context<Self>) {
        self.constructive_status = Some("Audition diff · not connected in this build".into());
        cx.notify();
    }
}
