mod analysis;
mod decomposition;
mod hpss;
mod loom;
mod pyramid;
mod settings;
mod ui;

use std::path::PathBuf;

use gpui::{App, AppContext as _, Application, Focusable as _};

fn main() {
    let initial_path = std::env::args_os().nth(1).map(PathBuf::from);

    Application::new().run(move |cx: &mut App| {
        ui::bind_keys(cx);
        let options = ui::window_options(cx);
        cx.open_window(options, |window, cx| {
            let workbench = cx.new(|cx| ui::Workbench::new(initial_path, cx));
            window.focus(&workbench.focus_handle(cx));
            workbench
        })
        .expect("opening the audec workbench");
        cx.activate(true);
    });
}
