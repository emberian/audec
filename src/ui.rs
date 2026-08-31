use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result};
use gpui::{
    actions, canvas, div, img, point, prelude::*, px, quad, relative, rgb, rgba, App, Bounds,
    Context, Entity, FocusHandle, Focusable, Image, ImageFormat, IntoElement, KeyBinding,
    MouseButton, MouseDownEvent, ObjectFit, PathBuilder, PathPromptOptions, Pixels, Render,
    SharedString, Task, Window, WindowOptions,
};
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player};

use crate::analysis::{
    analyze_file, Analysis, FeatureFrame, OnsetEvent, RhythmAnalysis, WaveformBin, MAX_FREQUENCY,
    MIN_FREQUENCY,
};

actions!(
    audec,
    [
        OpenAudio,
        TogglePlayback,
        SeekBackward,
        SeekForward,
        OpenWaterfall,
        OpenRhythm,
        ViewZoomIn,
        ViewZoomOut,
        ViewPanLeft,
        ViewPanRight,
        ViewFit,
    ]
);

const BACKGROUND: u32 = 0x090b10;
const PANEL: u32 = 0x10141d;
const PANEL_ALT: u32 = 0x0d1118;
const BORDER: u32 = 0x252c38;
const TEXT: u32 = 0xe8edf5;
const MUTED: u32 = 0x8c98a9;
const DIM: u32 = 0x596579;
const CYAN: u32 = 0x50d8d7;
const MAGENTA: u32 = 0xf172b6;
const AMBER: u32 = 0xf6b760;
const LIME: u32 = 0xa7d877;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-o", OpenAudio, Some("Audec")),
        KeyBinding::new("space", TogglePlayback, Some("Audec")),
        KeyBinding::new("left", SeekBackward, Some("Audec")),
        KeyBinding::new("right", SeekForward, Some("Audec")),
        KeyBinding::new("cmd-1", OpenWaterfall, Some("Audec")),
        KeyBinding::new("cmd-2", OpenRhythm, Some("Audec")),
        KeyBinding::new("space", TogglePlayback, Some("AudecLens")),
        KeyBinding::new("left", SeekBackward, Some("AudecLens")),
        KeyBinding::new("right", SeekForward, Some("AudecLens")),
        KeyBinding::new("=", ViewZoomIn, Some("AudecLens")),
        KeyBinding::new("-", ViewZoomOut, Some("AudecLens")),
        KeyBinding::new("shift-left", ViewPanLeft, Some("AudecLens")),
        KeyBinding::new("shift-right", ViewPanRight, Some("AudecLens")),
        KeyBinding::new("0", ViewFit, Some("AudecLens")),
    ]);
}

#[derive(Clone, Copy, Debug)]
enum VizKind {
    Waterfall,
    Rhythm,
}

impl VizKind {
    fn title(self) -> &'static str {
        match self {
            Self::Waterfall => "Spectral waterfall",
            Self::Rhythm => "Hit families / pulse",
        }
    }
}

struct AudioEngine {
    _device: MixerDeviceSink,
    player: Player,
}

impl AudioEngine {
    fn open(path: &Path) -> Result<Self> {
        let mut device =
            DeviceSinkBuilder::open_default_sink().context("opening the default audio output")?;
        device.log_on_drop(false);
        let player = Player::connect_new(device.mixer());
        let source = Decoder::try_from(
            File::open(path).with_context(|| format!("opening {} for playback", path.display()))?,
        )
        .context("decoding audio for playback")?;
        player.append(source);
        player.pause();
        Ok(Self {
            _device: device,
            player,
        })
    }

    fn toggle(&self) {
        if self.player.is_paused() {
            self.player.play();
        } else {
            self.player.pause();
        }
    }

    fn seek(&self, seconds: f64) -> Result<()> {
        self.player
            .try_seek(Duration::from_secs_f64(seconds.max(0.0)))
            .context("seeking audio")
    }

    fn position(&self) -> f64 {
        self.player.get_pos().as_secs_f64()
    }

    fn is_playing(&self) -> bool {
        !self.player.is_paused() && !self.player.empty()
    }
}

enum ProjectState {
    Empty,
    Loading(PathBuf),
    Ready(Arc<Analysis>),
    Failed(String),
}

pub struct Workbench {
    state: ProjectState,
    spectrogram: Option<Arc<Image>>,
    audio: Option<AudioEngine>,
    audio_error: Option<String>,
    playhead_seconds: f64,
    timeline_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    focus_handle: FocusHandle,
    _ticker: Task<()>,
}

impl Workbench {
    pub fn new(initial_path: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let ticker = cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(33))
                .await;
            if this
                .update(cx, |this, cx| {
                    if let Some(audio) = &this.audio {
                        let next = audio.position();
                        if audio.is_playing() || (next - this.playhead_seconds).abs() > 0.001 {
                            this.playhead_seconds = next;
                            cx.notify();
                        }
                    }
                })
                .is_err()
            {
                break;
            }
        });

        let mut workbench = Self {
            state: ProjectState::Empty,
            spectrogram: None,
            audio: None,
            audio_error: None,
            playhead_seconds: 0.0,
            timeline_bounds: Arc::new(Mutex::new(None)),
            focus_handle: cx.focus_handle(),
            _ticker: ticker,
        };
        if let Some(path) = initial_path {
            workbench.load_path(path, cx);
        }
        workbench
    }

    fn load_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(audio) = self.audio.take() {
            audio.player.stop();
        }
        self.spectrogram = None;
        self.audio_error = None;
        self.playhead_seconds = 0.0;
        self.state = ProjectState::Loading(path.clone());
        cx.notify();

        let analysis = cx.background_spawn(async move { analyze_file(&path) });
        cx.spawn(async move |this, cx| {
            let result = analysis.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(analysis) => this.install_analysis(analysis),
                    Err(error) => {
                        this.state = ProjectState::Failed(format!("{error:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn install_analysis(&mut self, analysis: Analysis) {
        let image = Image::from_bytes(ImageFormat::Png, analysis.spectrogram_png.clone());
        self.spectrogram = Some(Arc::new(image));
        match AudioEngine::open(&analysis.path) {
            Ok(audio) => self.audio = Some(audio),
            Err(error) => self.audio_error = Some(format!("{error:#}")),
        }
        self.state = ProjectState::Ready(Arc::new(analysis));
    }

    fn choose_audio(&mut self, cx: &mut Context<Self>) {
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(SharedString::from("Analyze")),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = selection.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| this.load_path(path, cx));
        })
        .detach();
    }

    fn toggle_playback(&mut self, cx: &mut Context<Self>) {
        if let Some(audio) = &self.audio {
            audio.toggle();
            cx.notify();
        }
    }

    fn seek_to(&mut self, seconds: f64, cx: &mut Context<Self>) {
        let duration = self
            .analysis()
            .map_or(0.0, |analysis| analysis.duration_seconds);
        let seconds = seconds.clamp(0.0, duration);
        self.playhead_seconds = seconds;
        if let Some(audio) = &self.audio {
            if let Err(error) = audio.seek(seconds) {
                self.audio_error = Some(format!("{error:#}"));
            }
        }
        cx.notify();
    }

    fn seek_relative(&mut self, delta: f64, cx: &mut Context<Self>) {
        self.seek_to(self.playhead_seconds + delta, cx);
    }

    fn seek_from_pointer(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let bounds = *self.timeline_bounds.lock().unwrap();
        let Some(bounds) = bounds else {
            return;
        };
        let fraction = ((event.position.x - bounds.origin.x) / bounds.size.width).clamp(0.0, 1.0);
        if let Some(analysis) = self.analysis() {
            self.seek_to(analysis.duration_seconds * fraction as f64, cx);
        }
    }

    fn analysis(&self) -> Option<&Analysis> {
        match &self.state {
            ProjectState::Ready(analysis) => Some(analysis),
            _ => None,
        }
    }

    fn playhead_fraction(&self) -> f32 {
        self.analysis()
            .map(|analysis| {
                (self.playhead_seconds / analysis.duration_seconds.max(f64::EPSILON)) as f32
            })
            .unwrap_or(0.0)
            .clamp(0.0, 1.0)
    }

    fn current_feature(&self) -> Option<FeatureFrame> {
        let analysis = self.analysis()?;
        let index = (self.playhead_fraction() * analysis.features.len() as f32) as usize;
        analysis
            .features
            .get(index.min(analysis.features.len().saturating_sub(1)))
            .copied()
    }

    fn on_open(&mut self, _: &OpenAudio, _: &mut Window, cx: &mut Context<Self>) {
        self.choose_audio(cx);
    }

    fn on_toggle(&mut self, _: &TogglePlayback, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_playback(cx);
    }

    fn on_seek_backward(&mut self, _: &SeekBackward, _: &mut Window, cx: &mut Context<Self>) {
        self.seek_relative(-5.0, cx);
    }

    fn on_seek_forward(&mut self, _: &SeekForward, _: &mut Window, cx: &mut Context<Self>) {
        self.seek_relative(5.0, cx);
    }

    fn open_visualizer(&mut self, kind: VizKind, cx: &mut Context<Self>) {
        let workbench = cx.entity();
        let options = visualizer_window_options(kind, cx);
        // `open_window` renders its root synchronously. Defer until this action's
        // Workbench update lease has ended so the new view can safely observe it.
        cx.defer(move |cx| {
            if let Err(error) = cx.open_window(options, move |window, cx| {
                let visualizer = cx.new(|cx| Visualizer::new(kind, workbench, cx));
                window.focus(&visualizer.focus_handle(cx));
                visualizer
            }) {
                eprintln!("opening {}: {error:#}", kind.title());
            }
        });
    }

    fn on_open_waterfall(&mut self, _: &OpenWaterfall, _: &mut Window, cx: &mut Context<Self>) {
        self.open_visualizer(VizKind::Waterfall, cx);
    }

    fn on_open_rhythm(&mut self, _: &OpenRhythm, _: &mut Window, cx: &mut Context<Self>) {
        self.open_visualizer(VizKind::Rhythm, cx);
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_playing = self.audio.as_ref().is_some_and(AudioEngine::is_playing);
        let transport_enabled = self.audio.is_some();
        let title = self
            .analysis()
            .map(|analysis| analysis.title.clone())
            .unwrap_or_else(|| "No material loaded".to_owned());
        let duration = self
            .analysis()
            .map_or(0.0, |analysis| analysis.duration_seconds);

        div()
            .h(px(54.0))
            .flex_none()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                div()
                    .w(px(220.0))
                    .pl(px(82.0))
                    .pr_4()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(div().font_weight(gpui::FontWeight::BOLD).child("audec"))
                    .child(div().text_xs().text_color(rgb(MUTED)).child("reverse DAW")),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        div()
                            .id("seek-back")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .text_color(if transport_enabled {
                                rgb(TEXT)
                            } else {
                                rgb(DIM)
                            })
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| this.seek_relative(-5.0, cx)))
                            .child("−5s"),
                    )
                    .child(
                        div()
                            .id("play-pause")
                            .size(px(34.0))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(if transport_enabled {
                                rgb(TEXT)
                            } else {
                                rgb(BORDER)
                            })
                            .text_color(rgb(BACKGROUND))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_playback(cx)))
                            .child(if is_playing { "❚❚" } else { "▶" }),
                    )
                    .child(
                        div()
                            .id("seek-forward")
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .text_color(if transport_enabled {
                                rgb(TEXT)
                            } else {
                                rgb(DIM)
                            })
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| this.seek_relative(5.0, cx)))
                            .child("+5s"),
                    )
                    .child(
                        div()
                            .ml_3()
                            .min_w(px(92.0))
                            .text_sm()
                            .text_color(rgb(CYAN))
                            .child(format!(
                                "{} / {}",
                                format_time(self.playhead_seconds),
                                format_time(duration)
                            )),
                    )
                    .child(div().ml_2().text_sm().text_color(rgb(MUTED)).child(title)),
            )
            .child(
                div().w(px(220.0)).px_4().flex().justify_end().child(
                    div()
                        .id("open-audio")
                        .px_3()
                        .py_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(BORDER)))
                        .on_click(cx.listener(|this, _, _, cx| this.choose_audio(cx)))
                        .child("Open audio…"),
                ),
            )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (album, title, path) = self.analysis().map_or_else(
            || {
                (
                    "SESSION".to_owned(),
                    "Empty".to_owned(),
                    "Choose a FLAC to begin".to_owned(),
                )
            },
            |analysis| {
                (
                    analysis.album.clone(),
                    analysis.title.clone(),
                    analysis.path.display().to_string(),
                )
            },
        );

        div()
            .w(px(220.0))
            .flex_none()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL_ALT))
            .p_3()
            .gap_2()
            .child(section_label("MATERIAL"))
            .child(
                div()
                    .mt_1()
                    .p_3()
                    .rounded_md()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_xs().text_color(rgb(MAGENTA)).child(album))
                    .child(div().text_sm().child(title))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(DIM))
                            .line_clamp(2)
                            .child(path),
                    ),
            )
            .child(section_label("LAYERS"))
            .child(layer_row("Stereo waveform", CYAN, true))
            .child(layer_row("Log-frequency energy", MAGENTA, true))
            .child(layer_row("Transient flux", AMBER, true))
            .child(layer_row("Pulse / onset evidence", CYAN, true))
            .child(layer_row("Stereo field", LIME, true))
            .child(section_label("OPEN VIEWS"))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .id("open-waterfall")
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_visualizer(VizKind::Waterfall, cx)
                            }))
                            .child("Waterfall"),
                    )
                    .child(
                        div()
                            .id("open-rhythm")
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BORDER)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_visualizer(VizKind::Rhythm, cx)
                            }))
                            .child("Rhythm"),
                    ),
            )
            .child(
                div()
                    .mt_auto()
                    .pt_3()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .text_xs()
                    .text_color(rgb(DIM))
                    .child(
                        "Space  play/pause\n← →  seek 5 seconds\n⌘O  open material\n⌘1 / ⌘2  open views",
                    ),
            )
    }

    fn render_inspector(&self) -> impl IntoElement {
        let feature = self.current_feature().unwrap_or_default();
        let (tempo, confidence, beat) = self.analysis().map_or_else(
            || ("—".to_owned(), "—".to_owned(), "—".to_owned()),
            |analysis| {
                let beat = analysis
                    .rhythm
                    .beat_times
                    .partition_point(|time| *time <= self.playhead_seconds);
                (
                    format!("{:.1} BPM", analysis.rhythm.tempo_bpm),
                    format!("{:.0}%", analysis.rhythm.confidence * 100.0),
                    format!("{}.{}", beat / 4 + 1, beat % 4 + 1),
                )
            },
        );
        let metadata = self.analysis().map(|analysis| {
            format!(
                "{} Hz  ·  {}-bit  ·  {} ch",
                analysis.sample_rate, analysis.bits_per_sample, analysis.channels
            )
        });
        div()
            .w(px(220.0))
            .flex_none()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL_ALT))
            .p_3()
            .gap_2()
            .child(section_label("AT PLAYHEAD"))
            .child(metric("PULSE", tempo, CYAN))
            .child(metric("PULSE CONF.", confidence, CYAN))
            .child(metric("BEAT", beat, CYAN))
            .child(metric(
                "DOMINANT",
                format_frequency(feature.dominant_hz),
                MAGENTA,
            ))
            .child(metric(
                "LOUDNESS",
                format!("{:.0}%", feature.loudness * 100.0),
                CYAN,
            ))
            .child(metric(
                "BRIGHTNESS",
                format!("{:.0}%", feature.brightness * 100.0),
                AMBER,
            ))
            .child(metric(
                "TRANSIENT",
                format!("{:.0}%", feature.flux * 100.0),
                AMBER,
            ))
            .child(metric(
                "STEREO WIDTH",
                format!("{:.0}%", feature.stereo_width * 100.0),
                LIME,
            ))
            .child(metric(
                "CORRELATION",
                format!("{:+.2}", feature.correlation),
                LIME,
            ))
            .when_some(metadata, |this, metadata| {
                this.child(
                    div()
                        .mt_auto()
                        .pt_3()
                        .border_t_1()
                        .border_color(rgb(BORDER))
                        .text_xs()
                        .text_color(rgb(DIM))
                        .child(metadata),
                )
            })
            .when_some(self.audio_error.clone(), |this, error| {
                this.child(div().text_xs().text_color(rgb(MAGENTA)).child(error))
            })
    }

    fn render_timeline(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        match &self.state {
            ProjectState::Empty => empty_state(
                "Sound becomes material here.",
                "Open a FLAC to build a persistent waveform, log-frequency map, and perceptual feature lanes.",
            ),
            ProjectState::Loading(path) => empty_state(
                "Decompiling audio…",
                &format!("Reading {}, then projecting it into inspectable layers.", path.display()),
            ),
            ProjectState::Failed(error) => empty_state("The material would not open.", error),
            ProjectState::Ready(analysis) => {
                let fraction = self.playhead_fraction();
                let spectrogram = self.spectrogram.clone().unwrap();
                let waveform = analysis.waveform.clone();
                let features = analysis.features.clone();
                let rhythm = analysis.rhythm.clone();
                let duration = analysis.duration_seconds;
                let timeline_bounds = self.timeline_bounds.clone();

                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .bg(rgb(BACKGROUND))
                    .child(time_ruler(duration))
                    .child(lane("STEREO AMPLITUDE", px(100.0), waveform_plot(waveform, fraction)))
                    .child(
                        div()
                            .relative()
                            .h(px(250.0))
                            .flex_none()
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .cursor_crosshair()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                    this.seek_from_pointer(event, cx)
                                }),
                            )
                            .child(img(spectrogram).size_full().object_fit(ObjectFit::Fill))
                            .child(
                                div()
                                    .absolute()
                                    .top_2()
                                    .left_2()
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .bg(rgba(0x090b10cc))
                                    .text_xs()
                                    .text_color(rgb(TEXT))
                                    .child("LOG-FREQUENCY ENERGY  ·  32.7 Hz — 16 kHz"),
                            )
                            .child(timeline_overlay(timeline_bounds, fraction)),
                    )
                    .child(lane(
                        "PULSE HYPOTHESIS / BANDWISE ONSETS",
                        px(92.0),
                        rhythm_plot(rhythm, 0.0, duration, fraction),
                    ))
                    .child(lane(
                        "LOUDNESS / BRIGHTNESS",
                        px(72.0),
                        dual_feature_plot(
                            features.clone(),
                            fraction,
                            |feature| feature.loudness,
                            |feature| feature.brightness,
                            rgba(0x50d8d7cc),
                            rgba(0xf6b76099),
                        ),
                    ))
                    .child(lane(
                        "TRANSIENT FLUX",
                        px(64.0),
                        feature_plot(
                            features.clone(),
                            fraction,
                            |feature| feature.flux,
                            rgba(0xf6b760cc),
                        ),
                    ))
                    .child(lane(
                        "STEREO WIDTH",
                        px(64.0),
                        feature_plot(
                            features,
                            fraction,
                            |feature| feature.stereo_width,
                            rgba(0xa7d877cc),
                        ),
                    ))
                    .into_any_element()
            }
        }
    }
}

impl Focusable for Workbench {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workbench {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Audec")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_open))
            .on_action(cx.listener(Self::on_toggle))
            .on_action(cx.listener(Self::on_seek_backward))
            .on_action(cx.listener(Self::on_seek_forward))
            .on_action(cx.listener(Self::on_open_waterfall))
            .on_action(cx.listener(Self::on_open_rhythm))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .text_sm()
            .child(self.render_header(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.render_sidebar(cx))
                    .child(self.render_timeline(cx))
                    .child(self.render_inspector()),
            )
    }
}

struct Visualizer {
    kind: VizKind,
    workbench: Entity<Workbench>,
    timeline_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    focus_handle: FocusHandle,
    time_start: f64,
    time_end: f64,
    frequency_start: f32,
    frequency_end: f32,
}

impl Visualizer {
    fn new(kind: VizKind, workbench: Entity<Workbench>, cx: &mut Context<Self>) -> Self {
        cx.observe(&workbench, |_, _, cx| cx.notify()).detach();
        Self {
            kind,
            workbench,
            timeline_bounds: Arc::new(Mutex::new(None)),
            focus_handle: cx.focus_handle(),
            time_start: 0.0,
            time_end: 1.0,
            frequency_start: 0.0,
            frequency_end: 1.0,
        }
    }

    fn seek_from_pointer(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some(bounds) = *self.timeline_bounds.lock().unwrap() else {
            return;
        };
        let duration = self
            .workbench
            .read(cx)
            .analysis()
            .map_or(0.0, |analysis| analysis.duration_seconds);
        let fraction = ((event.position.x - bounds.origin.x) / bounds.size.width).clamp(0.0, 1.0);
        let global_fraction = self.time_start + f64::from(fraction) * self.time_span();
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| {
            workbench.seek_to(duration * global_fraction, cx)
        });
    }

    fn time_span(&self) -> f64 {
        (self.time_end - self.time_start).max(1.0e-6)
    }

    fn zoom_time(&mut self, scale: f64, cx: &mut Context<Self>) {
        let current = self.workbench.read(cx).playhead_fraction() as f64;
        let anchor = if (self.time_start..=self.time_end).contains(&current) {
            current
        } else {
            (self.time_start + self.time_end) * 0.5
        };
        let new_span = (self.time_span() * scale).clamp(0.0025, 1.0);
        let anchor_position = (anchor - self.time_start) / self.time_span();
        let mut start = anchor - anchor_position * new_span;
        start = start.clamp(0.0, 1.0 - new_span);
        self.time_start = start;
        self.time_end = start + new_span;
        cx.notify();
    }

    fn pan_time(&mut self, amount: f64, cx: &mut Context<Self>) {
        let span = self.time_span();
        let start = (self.time_start + amount * span).clamp(0.0, 1.0 - span);
        self.time_start = start;
        self.time_end = start + span;
        cx.notify();
    }

    fn zoom_frequency(&mut self, scale: f32, cx: &mut Context<Self>) {
        let center = (self.frequency_start + self.frequency_end) * 0.5;
        let span = ((self.frequency_end - self.frequency_start) * scale).clamp(0.05, 1.0);
        let start = (center - span * 0.5).clamp(0.0, 1.0 - span);
        self.frequency_start = start;
        self.frequency_end = start + span;
        cx.notify();
    }

    fn reset_view(&mut self, cx: &mut Context<Self>) {
        self.time_start = 0.0;
        self.time_end = 1.0;
        self.frequency_start = 0.0;
        self.frequency_end = 1.0;
        cx.notify();
    }

    fn on_toggle(&mut self, _: &TogglePlayback, _: &mut Window, cx: &mut Context<Self>) {
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| workbench.toggle_playback(cx));
    }

    fn on_seek_backward(&mut self, _: &SeekBackward, _: &mut Window, cx: &mut Context<Self>) {
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| workbench.seek_relative(-5.0, cx));
    }

    fn on_seek_forward(&mut self, _: &SeekForward, _: &mut Window, cx: &mut Context<Self>) {
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| workbench.seek_relative(5.0, cx));
    }

    fn on_view_zoom_in(&mut self, _: &ViewZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_time(0.5, cx);
    }

    fn on_view_zoom_out(&mut self, _: &ViewZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_time(2.0, cx);
    }

    fn on_view_pan_left(&mut self, _: &ViewPanLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.pan_time(-0.7, cx);
    }

    fn on_view_pan_right(&mut self, _: &ViewPanRight, _: &mut Window, cx: &mut Context<Self>) {
        self.pan_time(0.7, cx);
    }

    fn on_view_fit(&mut self, _: &ViewFit, _: &mut Window, cx: &mut Context<Self>) {
        self.reset_view(cx);
    }

    fn render_header(
        &self,
        analysis: &Analysis,
        playhead_seconds: f64,
        is_playing: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let frequency_ratio = MAX_FREQUENCY / MIN_FREQUENCY;
        let frequency_low = MIN_FREQUENCY * frequency_ratio.powf(self.frequency_start);
        let frequency_high = MIN_FREQUENCY * frequency_ratio.powf(self.frequency_end);
        div()
            .h(px(50.0))
            .flex_none()
            .flex()
            .items_center()
            .pl(px(82.0))
            .pr_4()
            .gap_3()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::BOLD)
                    .child(self.kind.title()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MAGENTA))
                    .child(analysis.title.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        viz_control("view-pan-left", "←")
                            .on_click(cx.listener(|this, _, _, cx| this.pan_time(-0.7, cx))),
                    )
                    .child(
                        viz_control("view-zoom-out", "−")
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_time(2.0, cx))),
                    )
                    .child(
                        viz_control("view-fit", "Fit")
                            .on_click(cx.listener(|this, _, _, cx| this.reset_view(cx))),
                    )
                    .child(
                        viz_control("view-zoom-in", "+")
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_time(0.5, cx))),
                    )
                    .child(
                        viz_control("view-pan-right", "→")
                            .on_click(cx.listener(|this, _, _, cx| this.pan_time(0.7, cx))),
                    ),
            )
            .when(matches!(self.kind, VizKind::Waterfall), |header| {
                header.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            viz_control("frequency-out", "F−").on_click(
                                cx.listener(|this, _, _, cx| this.zoom_frequency(2.0, cx)),
                            ),
                        )
                        .child(
                            div()
                                .min_w(px(105.0))
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child(format!(
                                    "{}–{}",
                                    format_frequency(frequency_low),
                                    format_frequency(frequency_high)
                                )),
                        )
                        .child(
                            viz_control("frequency-in", "F+").on_click(
                                cx.listener(|this, _, _, cx| this.zoom_frequency(0.5, cx)),
                            ),
                        ),
                )
            })
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(CYAN))
                    .child(format_time(playhead_seconds)),
            )
            .child(
                div()
                    .id("viz-play-pause")
                    .size(px(30.0))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(TEXT))
                    .text_color(rgb(BACKGROUND))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let workbench = this.workbench.clone();
                        workbench.update(cx, |workbench, cx| workbench.toggle_playback(cx));
                    }))
                    .child(if is_playing { "❚❚" } else { "▶" }),
            )
    }

    fn render_waterfall(
        &self,
        analysis: Arc<Analysis>,
        spectrogram: Arc<Image>,
        playhead: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let timeline_bounds = self.timeline_bounds.clone();
        let start_seconds = analysis.duration_seconds * self.time_start;
        let end_seconds = analysis.duration_seconds * self.time_end;
        let features = slice_visible(&analysis.features, self.time_start, self.time_end);
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(time_ruler_range(start_seconds, end_seconds))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(300.0))
                    .overflow_hidden()
                    .cursor_crosshair()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.seek_from_pointer(event, cx)
                        }),
                    )
                    .child(cropped_spectrogram(
                        spectrogram,
                        self.time_start,
                        self.time_end,
                        self.frequency_start,
                        self.frequency_end,
                    ))
                    .child(timeline_overlay(timeline_bounds, playhead)),
            )
            .child(lane(
                "LOUDNESS / BRIGHTNESS",
                px(92.0),
                dual_feature_plot(
                    features.clone(),
                    playhead,
                    |feature| feature.loudness,
                    |feature| feature.brightness,
                    rgba(0x50d8d7cc),
                    rgba(0xf6b76099),
                ),
            ))
            .child(lane(
                "TRANSIENT FLUX",
                px(82.0),
                feature_plot(features, playhead, |feature| feature.flux, rgba(0xf6b760cc)),
            ))
    }

    fn render_rhythm(
        &self,
        analysis: Arc<Analysis>,
        playhead: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let timeline_bounds = self.timeline_bounds.clone();
        let start_seconds = analysis.duration_seconds * self.time_start;
        let end_seconds = analysis.duration_seconds * self.time_end;
        let waveform = slice_visible(&analysis.waveform, self.time_start, self.time_end);
        let features = slice_visible(&analysis.features, self.time_start, self.time_end);
        let families = analysis.rhythm.hit_families.clone();
        let family_count = families.len().max(1);
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(38.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_4()
                    .gap_4()
                    .bg(rgb(PANEL_ALT))
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_color(rgb(CYAN))
                            .child(format!("{:.1} BPM", analysis.rhythm.tempo_bpm)),
                    )
                    .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                        "pulse confidence {:.0}%  ·  {} candidate hit families  ·  {} events",
                        analysis.rhythm.confidence * 100.0,
                        analysis.rhythm.hit_families.len(),
                        analysis.rhythm.onsets.len()
                    ))),
            )
            .child(time_ruler_range(start_seconds, end_seconds))
            .child(
                div()
                    .h(px(300.0))
                    .flex_none()
                    .flex()
                    .child(
                        div()
                            .w(px(150.0))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .bg(rgb(PANEL_ALT))
                            .border_r_1()
                            .border_color(rgb(BORDER))
                            .children(families.into_iter().enumerate().map(
                                move |(index, family)| {
                                    div()
                                        .h(relative(1.0 / family_count as f32))
                                        .px_2()
                                        .flex()
                                        .flex_col()
                                        .justify_center()
                                        .border_b_1()
                                        .border_color(rgb(BORDER))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(family_color(index))
                                                .child(family.label),
                                        )
                                        .child(div().text_xs().text_color(rgb(DIM)).child(format!(
                                            "{} · {} hits · {:.0}% alike",
                                            format_frequency(family.centroid_hz),
                                            family.event_count,
                                            family.consistency * 100.0
                                        )))
                                        .child(family_spectrum_plot(
                                            family.spectrum,
                                            family_rgba(index),
                                        ))
                                },
                            )),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .h_full()
                            .cursor_crosshair()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                    this.seek_from_pointer(event, cx)
                                }),
                            )
                            .child(hit_family_plot(
                                analysis.rhythm.clone(),
                                start_seconds,
                                end_seconds,
                                playhead,
                            ))
                            .child(timeline_overlay(timeline_bounds, playhead)),
                    ),
            )
            .child(
                div()
                    .h(px(30.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_4()
                    .px_4()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child("Families are spectral-shape clusters, not instrument labels."),
            )
            .child(lane(
                "STEREO AMPLITUDE",
                px(124.0),
                waveform_plot(waveform, playhead),
            ))
            .child(lane(
                "TRANSIENT FLUX",
                px(92.0),
                feature_plot(features, playhead, |feature| feature.flux, rgba(0xf6b760cc)),
            ))
    }
}

impl Focusable for Visualizer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Visualizer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (analysis, spectrogram, playhead_seconds, is_playing) = {
            let workbench = self.workbench.read(cx);
            (
                match &workbench.state {
                    ProjectState::Ready(analysis) => Some(analysis.clone()),
                    _ => None,
                },
                workbench.spectrogram.clone(),
                workbench.playhead_seconds,
                workbench
                    .audio
                    .as_ref()
                    .is_some_and(AudioEngine::is_playing),
            )
        };

        let content = if let Some(analysis) = analysis {
            let global_playhead = playhead_seconds / analysis.duration_seconds.max(f64::EPSILON);
            let playhead = ((global_playhead - self.time_start) / self.time_span()) as f32;
            let body = match (self.kind, spectrogram) {
                (VizKind::Waterfall, Some(spectrogram)) => self
                    .render_waterfall(analysis.clone(), spectrogram, playhead, cx)
                    .into_any_element(),
                (VizKind::Rhythm, _) => self
                    .render_rhythm(analysis.clone(), playhead, cx)
                    .into_any_element(),
                _ => empty_state("The spectral image is unavailable.", "Reopen the material."),
            };
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(self.render_header(&analysis, playhead_seconds, is_playing, cx))
                .child(body)
                .into_any_element()
        } else {
            empty_state(
                self.kind.title(),
                "Load material in the workbench; this view will attach automatically.",
            )
        };

        div()
            .key_context("AudecLens")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_toggle))
            .on_action(cx.listener(Self::on_seek_backward))
            .on_action(cx.listener(Self::on_seek_forward))
            .on_action(cx.listener(Self::on_view_zoom_in))
            .on_action(cx.listener(Self::on_view_zoom_out))
            .on_action(cx.listener(Self::on_view_pan_left))
            .on_action(cx.listener(Self::on_view_pan_right))
            .on_action(cx.listener(Self::on_view_fit))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .text_sm()
            .child(content)
    }
}

fn section_label(label: &'static str) -> impl IntoElement {
    div().mt_3().text_xs().text_color(rgb(DIM)).child(label)
}

fn viz_control(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .min_w(px(25.0))
        .h(px(25.0))
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(rgb(BORDER))
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .text_color(rgb(MUTED))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(BORDER)).text_color(rgb(TEXT)))
        .child(label)
}

fn layer_row(label: &'static str, color: u32, active: bool) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .py_1()
        .text_xs()
        .text_color(if active { rgb(TEXT) } else { rgb(DIM) })
        .child(div().size(px(7.0)).rounded_full().bg(rgb(color)))
        .child(label)
}

fn metric(label: &'static str, value: String, color: u32) -> impl IntoElement {
    div()
        .py_2()
        .border_b_1()
        .border_color(rgb(BORDER))
        .flex()
        .items_baseline()
        .justify_between()
        .child(div().text_xs().text_color(rgb(DIM)).child(label))
        .child(div().text_sm().text_color(rgb(color)).child(value))
}

fn empty_state(title: &str, detail: &str) -> gpui::AnyElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .px_8()
        .child(div().text_lg().child(title.to_owned()))
        .child(
            div()
                .max_w(px(520.0))
                .text_color(rgb(MUTED))
                .text_center()
                .child(detail.to_owned()),
        )
        .into_any_element()
}

fn time_ruler(duration: f64) -> impl IntoElement {
    time_ruler_range(0.0, duration)
}

fn time_ruler_range(start: f64, end: f64) -> impl IntoElement {
    let ticks = 8;
    div()
        .h(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_between()
        .px_2()
        .border_b_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL_ALT))
        .children((0..=ticks).map(|tick| {
            div().text_xs().text_color(rgb(DIM)).child(format_time(
                start + (end - start) * tick as f64 / ticks as f64,
            ))
        }))
}

fn cropped_spectrogram(
    image: Arc<Image>,
    time_start: f64,
    time_end: f64,
    frequency_start: f32,
    frequency_end: f32,
) -> impl IntoElement {
    let time_span = (time_end - time_start).max(1.0e-6) as f32;
    let frequency_span = (frequency_end - frequency_start).max(1.0e-6);
    let source_top = 1.0 - frequency_end;
    img(image)
        .absolute()
        .left(relative(-(time_start as f32) / time_span))
        .top(relative(-source_top / frequency_span))
        .w(relative(1.0 / time_span))
        .h(relative(1.0 / frequency_span))
        .object_fit(ObjectFit::Fill)
}

fn slice_visible<T: Clone>(values: &[T], start: f64, end: f64) -> Vec<T> {
    if values.is_empty() {
        return Vec::new();
    }
    let first = (start.clamp(0.0, 1.0) * values.len() as f64).floor() as usize;
    let last = (end.clamp(0.0, 1.0) * values.len() as f64).ceil() as usize;
    values[first.min(values.len() - 1)..last.clamp(first + 1, values.len())].to_vec()
}

fn lane(label: &'static str, height: Pixels, plot: impl IntoElement) -> impl IntoElement {
    div()
        .relative()
        .h(height)
        .flex_none()
        .border_b_1()
        .border_color(rgb(BORDER))
        .child(plot)
        .child(
            div()
                .absolute()
                .top_2()
                .left_2()
                .px_2()
                .py_1()
                .rounded_sm()
                .bg(rgba(0x090b10cc))
                .text_xs()
                .text_color(rgb(MUTED))
                .child(label),
        )
}

fn waveform_plot(waveform: Vec<WaveformBin>, playhead: f32) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            if let Some(path) = waveform_envelope(&waveform, bounds, true) {
                window.paint_path(path, rgba(0x50d8d7a8));
            }
            if let Some(path) = waveform_envelope(&waveform, bounds, false) {
                window.paint_path(path, rgba(0xf172b69a));
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

fn waveform_envelope(
    waveform: &[WaveformBin],
    bounds: Bounds<Pixels>,
    left_channel: bool,
) -> Option<gpui::Path<Pixels>> {
    if waveform.len() < 2 {
        return None;
    }
    let center = bounds.origin.y + bounds.size.height * if left_channel { 0.28 } else { 0.72 };
    let amplitude = bounds.size.height * 0.20;
    let mut builder = PathBuilder::fill();
    for (index, bin) in waveform.iter().enumerate() {
        let fraction = index as f32 / (waveform.len() - 1) as f32;
        let value = if left_channel {
            bin.left_max
        } else {
            bin.right_max
        };
        let location = point(
            bounds.origin.x + bounds.size.width * fraction,
            center - amplitude * value.clamp(-1.0, 1.0),
        );
        if index == 0 {
            builder.move_to(location);
        } else {
            builder.line_to(location);
        }
    }
    for (index, bin) in waveform.iter().enumerate().rev() {
        let fraction = index as f32 / (waveform.len() - 1) as f32;
        let value = if left_channel {
            bin.left_min
        } else {
            bin.right_min
        };
        builder.line_to(point(
            bounds.origin.x + bounds.size.width * fraction,
            center - amplitude * value.clamp(-1.0, 1.0),
        ));
    }
    builder.close();
    builder.build().ok()
}

fn dual_feature_plot(
    features: Vec<FeatureFrame>,
    playhead: f32,
    first: fn(FeatureFrame) -> f32,
    second: fn(FeatureFrame) -> f32,
    first_color: gpui::Rgba,
    second_color: gpui::Rgba,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            if let Some(path) = feature_area(&features, bounds, first) {
                window.paint_path(path, first_color);
            }
            if let Some(path) = feature_line(&features, bounds, second) {
                window.paint_path(path, second_color);
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

fn feature_plot(
    features: Vec<FeatureFrame>,
    playhead: f32,
    value: fn(FeatureFrame) -> f32,
    color: gpui::Rgba,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            if let Some(path) = feature_area(&features, bounds, value) {
                window.paint_path(path, color);
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

fn rhythm_plot(
    rhythm: RhythmAnalysis,
    start_seconds: f64,
    end_seconds: f64,
    playhead: f32,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            let duration = (end_seconds - start_seconds).max(f64::EPSILON);
            for (index, time) in rhythm.beat_times.iter().copied().enumerate() {
                if time < start_seconds || time > end_seconds {
                    continue;
                }
                let fraction = ((time - start_seconds) / duration) as f32;
                let x = bounds.origin.x + bounds.size.width * fraction;
                let is_bar = index % 4 == 0;
                window.paint_quad(quad(
                    Bounds::new(
                        point(x, bounds.origin.y),
                        gpui::size(if is_bar { px(1.5) } else { px(1.0) }, bounds.size.height),
                    ),
                    px(0.0),
                    if is_bar {
                        rgba(0x50d8d766)
                    } else {
                        rgba(0xffffff18)
                    },
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }

            for onset in rhythm.onsets.iter().copied() {
                if onset.time_seconds < start_seconds || onset.time_seconds > end_seconds {
                    continue;
                }
                let fraction = ((onset.time_seconds - start_seconds) / duration) as f32;
                let x = bounds.origin.x + bounds.size.width * fraction;
                let (row, color) = onset_style(onset);
                let lane_height = bounds.size.height / 3.0;
                let max_height = lane_height * 0.84;
                let height = (max_height * onset.strength.max(0.12)).max(px(2.0));
                let bottom = bounds.origin.y + lane_height * (row as f32 + 0.92);
                window.paint_quad(quad(
                    Bounds::new(
                        point(x - px(1.0), bottom - height),
                        gpui::size(px(2.0), height),
                    ),
                    px(1.0),
                    color,
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

fn hit_family_plot(
    rhythm: RhythmAnalysis,
    start_seconds: f64,
    end_seconds: f64,
    playhead: f32,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            let duration = (end_seconds - start_seconds).max(f64::EPSILON);
            let rows = rhythm.hit_families.len().max(1);
            let row_height = bounds.size.height / rows as f32;
            for row in 1..rows {
                let y = bounds.origin.y + row_height * row as f32;
                window.paint_quad(quad(
                    Bounds::new(
                        point(bounds.origin.x, y),
                        gpui::size(bounds.size.width, px(1.0)),
                    ),
                    px(0.0),
                    rgba(0xffffff14),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            for (index, time) in rhythm.beat_times.iter().copied().enumerate() {
                if time < start_seconds || time > end_seconds {
                    continue;
                }
                let fraction = ((time - start_seconds) / duration) as f32;
                let x = bounds.origin.x + bounds.size.width * fraction;
                window.paint_quad(quad(
                    Bounds::new(
                        point(x, bounds.origin.y),
                        gpui::size(
                            if index % 4 == 0 { px(1.5) } else { px(1.0) },
                            bounds.size.height,
                        ),
                    ),
                    px(0.0),
                    if index % 4 == 0 {
                        rgba(0x50d8d744)
                    } else {
                        rgba(0xffffff10)
                    },
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            for onset in rhythm.onsets.iter().copied() {
                if onset.time_seconds < start_seconds || onset.time_seconds > end_seconds {
                    continue;
                }
                let fraction = ((onset.time_seconds - start_seconds) / duration) as f32;
                let x = bounds.origin.x + bounds.size.width * fraction;
                let row = onset.family.min(rows - 1);
                let height =
                    row_height * (0.18 + 0.72 * onset.strength * onset.family_similarity.max(0.35));
                let bottom = bounds.origin.y + row_height * (row as f32 + 0.92);
                window.paint_quad(quad(
                    Bounds::new(
                        point(x - px(1.25), bottom - height),
                        gpui::size(px(2.5), height),
                    ),
                    px(1.0),
                    family_rgba(row),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

fn family_color(index: usize) -> gpui::Rgba {
    rgb([
        CYAN, MAGENTA, AMBER, LIME, 0x8e9cff, 0xe99172, 0x78d5a3, 0xd8a7ff,
    ][index % 8])
}

fn family_rgba(index: usize) -> gpui::Rgba {
    rgba(
        [
            0x50d8d7dd, 0xf172b6dd, 0xf6b760dd, 0xa7d877dd, 0x8e9cffdd, 0xe99172dd, 0x78d5a3dd,
            0xd8a7ffdd,
        ][index % 8],
    )
}

fn family_spectrum_plot(spectrum: Vec<f32>, color: gpui::Rgba) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            if spectrum.len() < 2 {
                return;
            }
            let peak = spectrum.iter().copied().fold(0.0_f32, f32::max).max(1.0e-8);
            let mut builder = PathBuilder::stroke(px(1.0));
            for (index, value) in spectrum.iter().copied().enumerate() {
                let x = bounds.origin.x
                    + bounds.size.width * index as f32 / (spectrum.len() - 1) as f32;
                let y = bounds.origin.y + bounds.size.height * (1.0 - value / peak);
                if index == 0 {
                    builder.move_to(point(x, y));
                } else {
                    builder.line_to(point(x, y));
                }
            }
            if let Ok(path) = builder.build() {
                window.paint_path(path, color);
            }
        },
    )
    .h(px(8.0))
    .w_full()
}

fn onset_style(onset: OnsetEvent) -> (usize, gpui::Rgba) {
    if onset.high >= onset.mid && onset.high >= onset.low {
        (0, rgba(0xf6b760dd))
    } else if onset.mid >= onset.low {
        (1, rgba(0xf172b6dd))
    } else {
        (2, rgba(0x50d8d7dd))
    }
}

fn feature_area(
    features: &[FeatureFrame],
    bounds: Bounds<Pixels>,
    value: fn(FeatureFrame) -> f32,
) -> Option<gpui::Path<Pixels>> {
    if features.len() < 2 {
        return None;
    }
    let bottom = bounds.origin.y + bounds.size.height;
    let mut builder = PathBuilder::fill();
    builder.move_to(point(bounds.origin.x, bottom));
    for (index, feature) in features.iter().copied().enumerate() {
        let fraction = index as f32 / (features.len() - 1) as f32;
        builder.line_to(point(
            bounds.origin.x + bounds.size.width * fraction,
            bottom - bounds.size.height * value(feature).clamp(0.0, 1.0),
        ));
    }
    builder.line_to(point(bounds.origin.x + bounds.size.width, bottom));
    builder.close();
    builder.build().ok()
}

fn feature_line(
    features: &[FeatureFrame],
    bounds: Bounds<Pixels>,
    value: fn(FeatureFrame) -> f32,
) -> Option<gpui::Path<Pixels>> {
    if features.len() < 2 {
        return None;
    }
    let mut builder = PathBuilder::stroke(px(1.5));
    for (index, feature) in features.iter().copied().enumerate() {
        let fraction = index as f32 / (features.len() - 1) as f32;
        let location = point(
            bounds.origin.x + bounds.size.width * fraction,
            bounds.origin.y + bounds.size.height * (1.0 - value(feature).clamp(0.0, 1.0)),
        );
        if index == 0 {
            builder.move_to(location);
        } else {
            builder.line_to(location);
        }
    }
    builder.build().ok()
}

fn timeline_overlay(
    timeline_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    playhead: f32,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| {
            *timeline_bounds.lock().unwrap() = Some(bounds);
            bounds
        },
        move |bounds, _, window, _| {
            for fraction in [0.25_f32, 0.5, 0.75] {
                let x = bounds.origin.x + bounds.size.width * fraction;
                window.paint_quad(quad(
                    Bounds::new(
                        point(x, bounds.origin.y),
                        gpui::size(px(1.0), bounds.size.height),
                    ),
                    px(0.0),
                    rgba(0xffffff14),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .absolute()
    .inset_0()
}

fn paint_playhead(bounds: Bounds<Pixels>, fraction: f32, window: &mut Window) {
    if !(0.0..=1.0).contains(&fraction) {
        return;
    }
    let x = bounds.origin.x + bounds.size.width * fraction;
    window.paint_quad(quad(
        Bounds::new(
            point(x, bounds.origin.y),
            gpui::size(px(1.0), bounds.size.height),
        ),
        px(0.0),
        rgba(0xe8edf5dd),
        px(0.0),
        rgba(0x00000000),
        Default::default(),
    ));
}

fn format_time(seconds: f64) -> String {
    if !seconds.is_finite() {
        return "0:00.0".to_owned();
    }
    let minutes = (seconds / 60.0).floor() as u64;
    let remainder = seconds - minutes as f64 * 60.0;
    format!("{minutes}:{remainder:04.1}")
}

fn format_frequency(frequency: f32) -> String {
    if frequency >= 1_000.0 {
        format!("{:.2} kHz", frequency / 1_000.0)
    } else {
        format!("{frequency:.1} Hz")
    }
}

pub fn window_options(cx: &mut App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::centered(
            None,
            gpui::size(px(1_340.0), px(820.0)),
            cx,
        ))),
        window_min_size: Some(gpui::size(px(980.0), px(680.0))),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(SharedString::from("audec — reverse DAW")),
            appears_transparent: true,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn visualizer_window_options(kind: VizKind, cx: &mut App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::centered(
            None,
            gpui::size(px(1_080.0), px(720.0)),
            cx,
        ))),
        window_min_size: Some(gpui::size(px(720.0), px(480.0))),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(SharedString::from(format!("audec — {}", kind.title()))),
            appears_transparent: true,
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_transport_time() {
        assert_eq!(format_time(0.0), "0:00.0");
        assert_eq!(format_time(61.25), "1:01.2");
    }

    #[test]
    fn formats_frequency_scale() {
        assert_eq!(format_frequency(440.0), "440.0 Hz");
        assert_eq!(format_frequency(2_000.0), "2.00 kHz");
    }
}
