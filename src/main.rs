mod air_query;
mod analysis;
mod arrangement;
mod arrangement_view;
mod aspect;
mod asset_view;
mod assets;
mod audio;
mod audio_host;
mod automation;
mod command;
mod control_views;
mod curve_lang;
mod cqt;
mod daw_engine;
mod daw_project;
mod daw_render;
mod decomposition;
#[cfg(test)]
mod engine_regression;
mod export;
mod hpss;
mod instruments;
mod lens;
mod live_project;
mod loom;
mod mixer;
mod model_registry;
mod model_worker;
mod nmfd;
mod ontology;
mod pattern_lang;
mod persistence;
mod pitch;
mod plugin;
mod project;
mod project_codecs;
mod project_io;
mod pyramid;
mod reconstruction;
mod reconstruction_apply;
mod render;
mod render_validation;
mod rhythm;
mod sequencer;
mod sequencer_view;
mod session;
mod settings;
mod spectral_tiles;
mod timeline;
mod ui;
mod workspace;
mod workspace_ui;

use std::path::PathBuf;

use gpui::{App, Application};

fn main() {
    let initial_path = std::env::args_os().nth(1).map(PathBuf::from);

    Application::new().run(move |cx: &mut App| {
        ui::bind_keys(cx);
        arrangement_view::bind_arrangement_keys(cx);
        sequencer_view::bind_keys(cx);
        control_views::bind_control_view_keys(cx);
        ui::init_theme(cx);
        let options = ui::window_options(cx);
        cx.open_window(options, |window, cx| {
            ui::create_workspace(initial_path, window, cx)
        })
        .expect("opening the audec workbench");
        cx.activate(true);
    });
}
