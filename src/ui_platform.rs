//! Native UI platform adapter.
//!
//! This is the narrow seam between platform/UI infrastructure (GPUI, Guise,
//! native windows and menus) and Audec's project/application services. The
//! existing Workbench remains behind this seam during the gradual strangler
//! migration; new executables launch through `audec_app` instead of assembling
//! UI services independently.
//!
//! This module refuses to own project state, command history, transport state,
//! or workspace persistence. Those authorities remain in their existing Audec
//! services and are only installed into a native application here.

use std::path::PathBuf;

use gpui::{App, Application};

/// Configuration accepted at the process/platform boundary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchOptions {
    /// Optional material or project path supplied by the operating system or
    /// command line.
    pub initial_path: Option<PathBuf>,
}

impl LaunchOptions {
    pub fn from_process_arguments() -> Self {
        Self {
            initial_path: std::env::args_os().nth(1).map(PathBuf::from),
        }
    }
}

/// Run the native desktop application until GPUI terminates its event loop.
///
/// Keeping this as the executable's sole Audec call makes a future GPUI/toolkit
/// migration local to the application boundary rather than every binary.
pub fn run(options: LaunchOptions) {
    let initial_path = options.initial_path;
    Application::new().run(move |cx: &mut App| {
        install_application_services(cx);
        open_initial_project_window(initial_path, cx);
        cx.activate(true);
    });
}

fn install_application_services(cx: &mut App) {
    crate::ui::bind_keys(cx);
    crate::arrangement_view::bind_arrangement_keys(cx);
    crate::sequencer_view::bind_keys(cx);
    crate::control_views::bind_control_view_keys(cx);
    crate::reading_query_view::bind_reading_query_view_keys(cx);
    crate::ui::init_theme(cx);
    cx.set_menus(crate::ui::app_menus());
}

fn open_initial_project_window(initial_path: Option<PathBuf>, cx: &mut App) {
    let options = crate::ui::window_options(cx);
    cx.open_window(options, |window, cx| {
        crate::ui::create_workspace(initial_path, window, cx)
    })
    .expect("opening the audec workbench");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_options_default_to_an_empty_project_intent() {
        assert_eq!(LaunchOptions::default().initial_path, None);
    }
}
