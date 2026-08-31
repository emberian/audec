use std::fs::File;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use gpui::{
    actions, canvas, div, img, point, prelude::*, px, quad, relative, rgb, rgba, App, Bounds,
    Context, Entity, FocusHandle, Focusable, Image, ImageFormat, IntoElement, KeyBinding,
    MouseButton, MouseDownEvent, ObjectFit, PathBuilder, PathPromptOptions, Pixels, Render,
    ScrollWheelEvent, SharedString, Task, Window, WindowOptions,
};
use rodio::{buffer::SamplesBuffer, Decoder, DeviceSinkBuilder, MixerDeviceSink, Player};

use crate::analysis::{
    analyze_file, encode_spectrogram, spectral_projection, Analysis, FeatureFrame, OnsetEvent,
    RhythmAnalysis, WaveformBin, MAX_FREQUENCY, MIN_FREQUENCY,
};
use crate::decomposition::ComponentDecomposition;
use crate::hpss::{separate_harmonic_percussive, HpssResult, HpssSettings};
use crate::loom::{EventObservation, FitMetrics, SequenceSketch, TemplateBuildConfig};
use crate::settings::SpectrumSettings;

actions!(
    audec,
    [
        OpenAudio,
        TogglePlayback,
        SeekBackward,
        SeekForward,
        OpenWaterfall,
        OpenRhythm,
        OpenComponents,
        OpenSeparation,
        OpenLoom,
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
        KeyBinding::new("cmd-3", OpenComponents, Some("Audec")),
        KeyBinding::new("cmd-4", OpenSeparation, Some("Audec")),
        KeyBinding::new("cmd-5", OpenLoom, Some("Audec")),
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VizKind {
    Waterfall,
    Rhythm,
    Components,
    Separation,
    Loom,
}

impl VizKind {
    fn title(self) -> &'static str {
        match self {
            Self::Waterfall => "Spectral waterfall",
            Self::Rhythm => "Event recurrence / pulse hypotheses",
            Self::Components => "Recurring component hypotheses",
            Self::Separation => "Harmonic / transient decomposition",
            Self::Loom => "Loom — editable event reconstruction",
        }
    }
}

struct AudioEngine {
    _device: MixerDeviceSink,
    player: Player,
    preview: Player,
}

impl AudioEngine {
    fn open(path: &Path) -> Result<Self> {
        let mut device =
            DeviceSinkBuilder::open_default_sink().context("opening the default audio output")?;
        device.log_on_drop(false);
        let player = Player::connect_new(device.mixer());
        let preview = Player::connect_new(device.mixer());
        let source = Decoder::try_from(
            File::open(path).with_context(|| format!("opening {} for playback", path.display()))?,
        )
        .context("decoding audio for playback")?;
        player.append(source);
        player.pause();
        preview.pause();
        Ok(Self {
            _device: device,
            player,
            preview,
        })
    }

    fn toggle(&self) {
        self.preview.stop();
        if self.player.is_paused() {
            self.player.play();
        } else {
            self.player.pause();
        }
    }

    fn seek(&self, seconds: f64) -> Result<()> {
        self.preview.stop();
        self.player
            .try_seek(Duration::from_secs_f64(seconds.max(0.0)))
            .context("seeking audio")
    }

    fn position(&self) -> f64 {
        self.player.get_pos().as_secs_f64()
    }

    fn is_playing(&self) -> bool {
        (!self.player.is_paused() && !self.player.empty())
            || (!self.preview.is_paused() && !self.preview.empty())
    }

    fn audition(&self, samples: Vec<f32>, sample_rate: u32) {
        let Some(channels) = NonZero::new(1_u16) else {
            return;
        };
        let Some(sample_rate) = NonZero::new(sample_rate) else {
            return;
        };
        self.player.pause();
        self.preview.stop();
        self.preview
            .append(SamplesBuffer::new(channels, sample_rate, samples));
        self.preview.play();
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

    fn audition_pcm(&mut self, samples: Vec<f32>, sample_rate: u32, cx: &mut Context<Self>) {
        if let Some(audio) = &self.audio {
            audio.audition(samples, sample_rate);
            cx.notify();
        }
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
                if kind == VizKind::Separation {
                    visualizer.update(cx, |visualizer, cx| visualizer.refresh_hpss(cx));
                } else if kind == VizKind::Loom {
                    visualizer.update(cx, |visualizer, cx| visualizer.refresh_loom(cx));
                }
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

    fn on_open_components(&mut self, _: &OpenComponents, _: &mut Window, cx: &mut Context<Self>) {
        self.open_visualizer(VizKind::Components, cx);
    }

    fn on_open_separation(&mut self, _: &OpenSeparation, _: &mut Window, cx: &mut Context<Self>) {
        self.open_visualizer(VizKind::Separation, cx);
    }

    fn on_open_loom(&mut self, _: &OpenLoom, _: &mut Window, cx: &mut Context<Self>) {
        self.open_visualizer(VizKind::Loom, cx);
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
                    .id("open-components")
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BORDER)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_visualizer(VizKind::Components, cx)
                    }))
                    .child("Components"),
            )
            .child(
                div()
                    .id("open-separation")
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BORDER)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_visualizer(VizKind::Separation, cx)
                    }))
                    .child("Decompose selected span"),
            )
            .child(
                div()
                    .id("open-loom")
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BORDER)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_visualizer(VizKind::Loom, cx)
                    }))
                    .child("Loom · reconstruct events"),
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
                        "Space  play/pause\n← →  seek 5 seconds\n⌘O  open material\n⌘1…⌘5  open views",
                    ),
            )
    }

    fn render_inspector(&self) -> impl IntoElement {
        let feature = self.current_feature().unwrap_or_default();
        let (tempo, pulse_support, beat) = self.analysis().map_or_else(
            || ("—".to_owned(), "—".to_owned(), "—".to_owned()),
            |analysis| {
                let beat = analysis
                    .rhythm
                    .beat_times
                    .partition_point(|time| *time <= self.playhead_seconds);
                (
                    format!("{:.1} BPM", analysis.rhythm.tempo_bpm),
                    format!("{:.0}%", analysis.rhythm.pulse_contrast * 100.0),
                    format!("{}", beat + 1),
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
            .child(metric("PULSE CONTRAST", pulse_support, CYAN))
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
            .on_action(cx.listener(Self::on_open_components))
            .on_action(cx.listener(Self::on_open_separation))
            .on_action(cx.listener(Self::on_open_loom))
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

enum HpssViewState {
    Idle,
    Analyzing {
        start_seconds: f64,
        end_seconds: f64,
    },
    Ready(Arc<HpssViewResult>),
    Failed(String),
}

struct HpssViewResult {
    start_seconds: f64,
    end_seconds: f64,
    sample_rate: u32,
    original: Vec<f32>,
    separation: HpssResult,
}

#[derive(Clone, Copy)]
enum HpssAudition {
    Original,
    Harmonic,
    Percussive,
    Residual,
}

enum LoomViewState {
    Idle,
    Inferring {
        start_seconds: f64,
        end_seconds: f64,
        event_count: usize,
    },
    Ready(LoomViewResult),
    Failed(String),
}

struct LoomViewResult {
    sketch: SequenceSketch,
    selected_cluster: usize,
    start_sample: usize,
    start_seconds: f64,
    end_seconds: f64,
    sample_rate: u32,
    original: Vec<f32>,
    reconstruction: Vec<f32>,
    residual: Vec<f32>,
    fit: FitMetrics,
}

#[derive(Clone, Copy)]
enum LoomAudition {
    Original,
    Reconstruction,
    Residual,
    Template,
}

struct Visualizer {
    kind: VizKind,
    workbench: Entity<Workbench>,
    timeline_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    focus_handle: FocusHandle,
    time_start: f64,
    time_end: f64,
    follow_playhead: bool,
    frequency_start: f32,
    frequency_end: f32,
    spectrum_settings: SpectrumSettings,
    local_spectrogram: Option<Arc<Image>>,
    local_spectral_db: Option<Arc<Vec<f32>>>,
    spectrogram_source: Option<PathBuf>,
    spectrum_generation: u64,
    spectrum_transforming: bool,
    hpss_state: HpssViewState,
    hpss_generation: u64,
    loom_state: LoomViewState,
    loom_generation: u64,
}

impl Visualizer {
    fn new(kind: VizKind, workbench: Entity<Workbench>, cx: &mut Context<Self>) -> Self {
        cx.observe(&workbench, |_, _, cx| cx.notify()).detach();
        let (spectrum_settings, spectrogram_source, playhead, duration) = {
            let workbench = workbench.read(cx);
            if let Some(analysis) = workbench.analysis() {
                (
                    SpectrumSettings {
                        fft_size: 8_192,
                        hop_size: 2_048,
                        min_frequency_hz: MIN_FREQUENCY,
                        max_frequency_hz: MAX_FREQUENCY,
                        db_ceiling: analysis.spectral_peak_db,
                        db_range: 84.0,
                        ..SpectrumSettings::default()
                    },
                    Some(analysis.path.clone()),
                    workbench.playhead_fraction() as f64,
                    analysis.duration_seconds,
                )
            } else {
                (SpectrumSettings::default(), None, 0.0, 0.0)
            }
        };
        let (time_start, time_end) =
            if matches!(kind, VizKind::Separation | VizKind::Loom) && duration > 0.0 {
                let span = (18.0 / duration).clamp(0.0025, 1.0);
                let start = (playhead - span * 0.5).clamp(0.0, 1.0 - span);
                (start, start + span)
            } else {
                (0.0, 1.0)
            };
        Self {
            kind,
            workbench,
            timeline_bounds: Arc::new(Mutex::new(None)),
            focus_handle: cx.focus_handle(),
            time_start,
            time_end,
            follow_playhead: true,
            frequency_start: 0.0,
            frequency_end: 1.0,
            spectrum_settings,
            local_spectrogram: None,
            local_spectral_db: None,
            spectrogram_source,
            spectrum_generation: 0,
            spectrum_transforming: false,
            hpss_state: HpssViewState::Idle,
            hpss_generation: 0,
            loom_state: LoomViewState::Idle,
            loom_generation: 0,
        }
    }

    fn rebuild_spectrogram(&mut self, cx: &mut Context<Self>) {
        let analysis = self.workbench.read(cx).analysis().map(|value| {
            (
                value.path.clone(),
                self.local_spectral_db
                    .as_ref()
                    .map(|values| values.as_ref().clone())
                    .unwrap_or_else(|| value.spectral_db.clone()),
                value.spectral_peak_db,
            )
        });
        let Some((path, spectral_db, _)) = analysis else {
            return;
        };
        match encode_spectrogram(
            &spectral_db,
            self.spectrum_settings.db_ceiling,
            self.spectrum_settings.db_range,
        ) {
            Ok(bytes) => {
                self.local_spectrogram = Some(Arc::new(Image::from_bytes(ImageFormat::Png, bytes)));
                self.spectrogram_source = Some(path);
            }
            Err(error) => eprintln!("rendering lens spectrogram: {error:#}"),
        }
        cx.notify();
    }

    fn adjust_db_ceiling(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.spectrum_settings.db_ceiling =
            (self.spectrum_settings.db_ceiling + delta).clamp(-120.0, 24.0);
        self.rebuild_spectrogram(cx);
    }

    fn adjust_db_range(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.spectrum_settings.db_range =
            (self.spectrum_settings.db_range + delta).clamp(6.0, 180.0);
        self.rebuild_spectrogram(cx);
    }

    fn rerun_spectrum(&mut self, cx: &mut Context<Self>) {
        let settings = self.spectrum_settings;
        let source = self.workbench.read(cx).analysis().map(|analysis| {
            let frames = analysis.waveform_pyramid.frame_count();
            (
                analysis.path.clone(),
                analysis.sample_rate,
                analysis.mono_range(0, frames),
            )
        });
        let Some((path, sample_rate, mono)) = source else {
            return;
        };

        self.spectrum_generation = self.spectrum_generation.wrapping_add(1);
        let generation = self.spectrum_generation;
        self.spectrum_transforming = true;
        cx.notify();
        let task = cx.background_spawn(async move {
            let values = spectral_projection(&mono, sample_rate, settings);
            let image = encode_spectrogram(&values, settings.db_ceiling, settings.db_range)
                .map(|bytes| Arc::new(Image::from_bytes(ImageFormat::Png, bytes)))
                .map_err(|error| format!("{error:#}"));
            (values, image)
        });
        cx.spawn(async move |this, cx| {
            let (values, image) = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.spectrum_generation != generation {
                    return;
                }
                this.spectrum_transforming = false;
                match image {
                    Ok(image) => {
                        this.local_spectral_db = Some(Arc::new(values));
                        this.local_spectrogram = Some(image);
                        this.spectrogram_source = Some(path);
                    }
                    Err(error) => eprintln!("rerunning spectrum transform: {error}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn change_fft_size(&mut self, direction: i32, cx: &mut Context<Self>) {
        self.spectrum_settings.fft_size = if direction < 0 {
            (self.spectrum_settings.fft_size / 2).max(256)
        } else {
            (self.spectrum_settings.fft_size * 2).min(65_536)
        };
        self.spectrum_settings.hop_size = (self.spectrum_settings.fft_size / 4).max(1);
        self.rerun_spectrum(cx);
    }

    fn cycle_window_function(&mut self, cx: &mut Context<Self>) {
        self.spectrum_settings.window = self.spectrum_settings.window.next();
        self.rerun_spectrum(cx);
    }

    fn refresh_hpss(&mut self, cx: &mut Context<Self>) {
        let (duration, sample_rate, frame_count, playhead) = {
            let workbench = self.workbench.read(cx);
            let Some(analysis) = workbench.analysis() else {
                self.hpss_state = HpssViewState::Idle;
                return;
            };
            (
                analysis.duration_seconds,
                analysis.sample_rate,
                analysis.waveform_pyramid.frame_count(),
                workbench.playhead_fraction() as f64,
            )
        };
        if frame_count == 0 || duration <= 0.0 {
            self.hpss_state = HpssViewState::Idle;
            return;
        }

        // A reconstructible whole-song complex STFT can consume hundreds of
        // megabytes. HPSS is therefore an Aspect-local transform. Keep an
        // explicit upper bound until the field becomes a tiled disk cache.
        let maximum_span = (30.0 / duration).min(1.0);
        if self.time_span() > maximum_span {
            let anchor = if (self.time_start..=self.time_end).contains(&playhead) {
                playhead
            } else {
                (self.time_start + self.time_end) * 0.5
            };
            self.time_start = (anchor - maximum_span * 0.5).clamp(0.0, 1.0 - maximum_span);
            self.time_end = self.time_start + maximum_span;
        }

        let start_frame = (self.time_start * frame_count as f64).floor() as usize;
        let end_frame = (self.time_end * frame_count as f64).ceil() as usize;
        let original = self
            .workbench
            .read(cx)
            .analysis()
            .map(|analysis| analysis.mono_range(start_frame, end_frame))
            .unwrap_or_default();
        let start_seconds = start_frame as f64 / f64::from(sample_rate);
        let end_seconds = end_frame as f64 / f64::from(sample_rate);

        self.hpss_generation = self.hpss_generation.wrapping_add(1);
        let generation = self.hpss_generation;
        self.hpss_state = HpssViewState::Analyzing {
            start_seconds,
            end_seconds,
        };
        cx.notify();

        let task = cx.background_spawn(async move {
            separate_harmonic_percussive(&original, HpssSettings::default())
                .map(|separation| {
                    Arc::new(HpssViewResult {
                        start_seconds,
                        end_seconds,
                        sample_rate,
                        original,
                        separation,
                    })
                })
                .map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.hpss_generation != generation {
                    return;
                }
                this.hpss_state = match result {
                    Ok(result) => HpssViewState::Ready(result),
                    Err(error) => HpssViewState::Failed(error),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn audition_hpss(&mut self, kind: HpssAudition, cx: &mut Context<Self>) {
        let HpssViewState::Ready(result) = &self.hpss_state else {
            return;
        };
        let samples = match kind {
            HpssAudition::Original => result.original.clone(),
            HpssAudition::Harmonic => result.separation.harmonic.clone(),
            HpssAudition::Percussive => result.separation.percussive.clone(),
            HpssAudition::Residual => result.separation.residual.clone(),
        };
        let sample_rate = result.sample_rate;
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| {
            workbench.audition_pcm(samples, sample_rate, cx)
        });
    }

    fn refresh_loom(&mut self, cx: &mut Context<Self>) {
        let source = {
            let workbench = self.workbench.read(cx);
            workbench.analysis().map(|analysis| {
                let frame_count = analysis.waveform_pyramid.frame_count();
                let observations = analysis
                    .rhythm
                    .onsets
                    .iter()
                    .map(|onset| EventObservation {
                        sample_index: (onset.time_seconds * f64::from(analysis.sample_rate)).round()
                            as usize,
                        cluster_id: onset.cluster,
                        salience: onset.strength,
                        template_similarity: onset.template_similarity,
                    })
                    .collect::<Vec<_>>();
                (
                    analysis.sample_rate,
                    frame_count,
                    analysis.mono_range(0, frame_count),
                    observations,
                )
            })
        };
        let Some((sample_rate, frame_count, mono, observations)) = source else {
            self.loom_state = LoomViewState::Idle;
            return;
        };
        if frame_count == 0 || observations.is_empty() {
            self.loom_state = LoomViewState::Failed(
                "No recurring onset observations are available to sequence.".to_owned(),
            );
            cx.notify();
            return;
        }

        let start_sample = (self.time_start * frame_count as f64).floor() as usize;
        let end_sample = (self.time_end * frame_count as f64).ceil() as usize;
        let start_seconds = start_sample as f64 / f64::from(sample_rate);
        let end_seconds = end_sample as f64 / f64::from(sample_rate);
        let event_count = observations.len();
        self.loom_generation = self.loom_generation.wrapping_add(1);
        let generation = self.loom_generation;
        self.loom_state = LoomViewState::Inferring {
            start_seconds,
            end_seconds,
            event_count,
        };
        cx.notify();

        let task = cx.background_spawn(async move {
            let started = Instant::now();
            let sketch = SequenceSketch::infer(
                &mono,
                sample_rate,
                &observations,
                TemplateBuildConfig::for_sample_rate(sample_rate),
            )
            .map_err(|error| error.to_string())?;
            let result = build_loom_result(sketch, &mono, start_sample, end_sample, sample_rate, 0);
            eprintln!(
                "inferred and rendered {} Loom events across {} templates in {:.3}s",
                result.sketch.events.len(),
                result.sketch.clusters.len(),
                started.elapsed().as_secs_f64(),
            );
            Ok::<_, String>(result)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.loom_generation != generation {
                    return;
                }
                this.loom_state = match result {
                    Ok(result) => LoomViewState::Ready(result),
                    Err(error) => LoomViewState::Failed(error),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn rerender_loom_span(&mut self, cx: &mut Context<Self>) {
        let source = {
            let workbench = self.workbench.read(cx);
            workbench.analysis().map(|analysis| {
                let frame_count = analysis.waveform_pyramid.frame_count();
                let start_sample = (self.time_start * frame_count as f64).floor() as usize;
                let end_sample = (self.time_end * frame_count as f64).ceil() as usize;
                (
                    start_sample,
                    end_sample,
                    analysis.sample_rate,
                    analysis.mono_range(start_sample, end_sample),
                )
            })
        };
        let Some((start_sample, end_sample, sample_rate, original)) = source else {
            return;
        };
        let LoomViewState::Ready(result) = &mut self.loom_state else {
            return;
        };
        update_loom_render(result, original, start_sample, end_sample, sample_rate);
        cx.notify();
    }

    fn cycle_loom_cluster(&mut self, direction: i32, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &mut self.loom_state else {
            return;
        };
        let count = result.sketch.clusters.len();
        if count == 0 {
            return;
        }
        result.selected_cluster = if direction < 0 {
            (result.selected_cluster + count - 1) % count
        } else {
            (result.selected_cluster + 1) % count
        };
        cx.notify();
    }

    fn toggle_loom_cluster(&mut self, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &mut self.loom_state else {
            return;
        };
        let Some(cluster_id) = selected_loom_cluster_id(result) else {
            return;
        };
        let enabled = result
            .sketch
            .cluster(cluster_id)
            .is_some_and(|cluster| !cluster.enabled);
        result.sketch.set_cluster_enabled(cluster_id, enabled);
        rebuild_loom_audio(result);
        cx.notify();
    }

    fn adjust_loom_cluster_gain(&mut self, delta: f32, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &mut self.loom_state else {
            return;
        };
        let Some(cluster_id) = selected_loom_cluster_id(result) else {
            return;
        };
        let gain = result
            .sketch
            .cluster(cluster_id)
            .map_or(1.0, |cluster| cluster.gain);
        result
            .sketch
            .set_cluster_gain(cluster_id, (gain + delta).clamp(0.0, 4.0));
        rebuild_loom_audio(result);
        cx.notify();
    }

    fn edit_nearest_loom_event(
        &mut self,
        timing_delta_seconds: f64,
        gain_delta: f32,
        toggle: bool,
        cx: &mut Context<Self>,
    ) {
        let playhead_sample = {
            let workbench = self.workbench.read(cx);
            workbench
                .analysis()
                .map(|analysis| {
                    (workbench.playhead_seconds * f64::from(analysis.sample_rate)).round() as i64
                })
                .unwrap_or(0)
        };
        let LoomViewState::Ready(result) = &mut self.loom_state else {
            return;
        };
        let Some(cluster_id) = selected_loom_cluster_id(result) else {
            return;
        };
        let Some(event_id) = nearest_loom_event(&result.sketch, cluster_id, playhead_sample) else {
            return;
        };
        if let Some(event) = result.sketch.event(event_id).cloned() {
            if timing_delta_seconds != 0.0 {
                let delta = (timing_delta_seconds * f64::from(result.sample_rate)).round() as i64;
                result
                    .sketch
                    .move_event(event_id, event.sample_index + delta);
            }
            if gain_delta != 0.0 {
                result
                    .sketch
                    .set_event_gain(event_id, (event.gain + gain_delta).clamp(0.0, 4.0));
            }
            if toggle {
                result.sketch.set_event_enabled(event_id, !event.enabled);
            }
        }
        rebuild_loom_audio(result);
        cx.notify();
    }

    fn audition_loom(&mut self, kind: LoomAudition, cx: &mut Context<Self>) {
        let LoomViewState::Ready(result) = &self.loom_state else {
            return;
        };
        let samples = match kind {
            LoomAudition::Original => result.original.clone(),
            LoomAudition::Reconstruction => result.reconstruction.clone(),
            LoomAudition::Residual => result.residual.clone(),
            LoomAudition::Template => selected_loom_cluster_id(result)
                .and_then(|cluster_id| result.sketch.cluster(cluster_id))
                .map(|cluster| cluster.template.samples.clone())
                .unwrap_or_default(),
        };
        let sample_rate = result.sample_rate;
        let workbench = self.workbench.clone();
        workbench.update(cx, |workbench, cx| {
            workbench.audition_pcm(samples, sample_rate, cx)
        });
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

    fn follow_playhead_if_needed(&mut self, analysis: &Analysis, playhead_seconds: f64) {
        if !self.follow_playhead || self.time_span() >= 0.999_999 {
            return;
        }
        let current =
            (playhead_seconds / analysis.duration_seconds.max(f64::EPSILON)).clamp(0.0, 1.0);
        if (self.time_start..=self.time_end).contains(&current) {
            return;
        }
        let span = self.time_span();
        self.time_start = (current - span * 0.5).clamp(0.0, 1.0 - span);
        self.time_end = self.time_start + span;

        if self.kind == VizKind::Loom {
            let frame_count = analysis.waveform_pyramid.frame_count();
            let start_sample = (self.time_start * frame_count as f64).floor() as usize;
            let end_sample = (self.time_end * frame_count as f64).ceil() as usize;
            let original = analysis.mono_range(start_sample, end_sample);
            if let LoomViewState::Ready(result) = &mut self.loom_state {
                update_loom_render(
                    result,
                    original,
                    start_sample,
                    end_sample,
                    analysis.sample_rate,
                );
            }
        }
    }

    fn center_time_on_playhead(&mut self, cx: &mut Context<Self>) {
        self.follow_playhead = true;
        let center = self.workbench.read(cx).playhead_fraction() as f64;
        let span = self.time_span();
        self.time_start = (center - span * 0.5).clamp(0.0, 1.0 - span);
        self.time_end = self.time_start + span;
        if self.kind == VizKind::Loom {
            self.rerender_loom_span(cx);
        } else {
            cx.notify();
        }
    }

    fn zoom_time(&mut self, scale: f64, cx: &mut Context<Self>) {
        self.follow_playhead = false;
        let current = self.workbench.read(cx).playhead_fraction() as f64;
        let current_is_visible = (self.time_start..=self.time_end).contains(&current);
        let anchor = if current_is_visible {
            current
        } else {
            current.clamp(0.0, 1.0)
        };
        let new_span = (self.time_span() * scale).clamp(0.0025, 1.0);
        let anchor_position = if current_is_visible {
            (anchor - self.time_start) / self.time_span()
        } else {
            0.5
        };
        let mut start = anchor - anchor_position * new_span;
        start = start.clamp(0.0, 1.0 - new_span);
        self.time_start = start;
        self.time_end = start + new_span;
        if self.kind == VizKind::Loom {
            self.rerender_loom_span(cx);
        } else {
            cx.notify();
        }
    }

    fn pan_time(&mut self, amount: f64, cx: &mut Context<Self>) {
        self.follow_playhead = false;
        let span = self.time_span();
        let start = (self.time_start + amount * span).clamp(0.0, 1.0 - span);
        self.time_start = start;
        self.time_end = start + span;
        if self.kind == VizKind::Loom {
            self.rerender_loom_span(cx);
        } else {
            cx.notify();
        }
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
        self.follow_playhead = false;
        self.time_start = 0.0;
        self.time_end = 1.0;
        self.frequency_start = 0.0;
        self.frequency_end = 1.0;
        if self.kind == VizKind::Loom {
            self.rerender_loom_span(cx);
        } else {
            cx.notify();
        }
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
        let is_waterfall = self.kind == VizKind::Waterfall;
        let top_row =
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
                .child(
                    div()
                        .max_w(px(310.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_sm()
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(self.kind.title()),
                )
                .child(
                    div()
                        .max_w(px(180.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
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
                        .child(viz_control("view-current", "Follow").px_2().on_click(
                            cx.listener(|this, _, _, cx| this.center_time_on_playhead(cx)),
                        ))
                        .child(
                            viz_control("view-zoom-in", "+")
                                .on_click(cx.listener(|this, _, _, cx| this.zoom_time(0.5, cx))),
                        )
                        .child(
                            viz_control("view-pan-right", "→")
                                .on_click(cx.listener(|this, _, _, cx| this.pan_time(0.7, cx))),
                        ),
                )
                .when(self.kind == VizKind::Separation, |header| {
                    header.child(
                        viz_control("reanalyze-hpss", "Analyze view")
                            .px_2()
                            .on_click(cx.listener(|this, _, _, cx| this.refresh_hpss(cx))),
                    )
                })
                .when(self.kind == VizKind::Loom, |header| {
                    header.child(
                        viz_control("reinfer-loom", "Reinfer")
                            .px_2()
                            .on_click(cx.listener(|this, _, _, cx| this.refresh_loom(cx))),
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
                );

        div()
            .h(px(if is_waterfall { 86.0 } else { 50.0 }))
            .flex_none()
            .flex()
            .flex_col()
            .bg(rgb(PANEL))
            .child(top_row)
            .when(is_waterfall, |header| {
                header.child(
                    div()
                        .h(px(36.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .pl(px(82.0))
                        .pr_4()
                        .gap_1()
                        .bg(rgb(PANEL_ALT))
                        .border_b_1()
                        .border_color(rgb(BORDER))
                        .child(
                            viz_control("fft-size-down", "FFT−").on_click(
                                cx.listener(|this, _, _, cx| this.change_fft_size(-1, cx)),
                            ),
                        )
                        .child(
                            div()
                                .min_w(px(82.0))
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child(format!(
                                    "{} {}{}",
                                    self.spectrum_settings.fft_size,
                                    self.spectrum_settings.window.label(),
                                    if self.spectrum_transforming {
                                        " …"
                                    } else {
                                        ""
                                    }
                                )),
                        )
                        .child(
                            viz_control("fft-size-up", "FFT+").on_click(
                                cx.listener(|this, _, _, cx| this.change_fft_size(1, cx)),
                            ),
                        )
                        .child(
                            viz_control("fft-window", "Win").on_click(
                                cx.listener(|this, _, _, cx| this.cycle_window_function(cx)),
                            ),
                        )
                        .child(div().w(px(12.0)))
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
                        )
                        .child(div().w(px(12.0)))
                        .child(viz_control("db-ceiling-down", "D−").on_click(
                            cx.listener(|this, _, _, cx| this.adjust_db_ceiling(-3.0, cx)),
                        ))
                        .child(
                            div()
                                .min_w(px(88.0))
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child(format!(
                                    "{:.0}/{:.0} dB",
                                    self.spectrum_settings.db_ceiling,
                                    self.spectrum_settings.db_range
                                )),
                        )
                        .child(viz_control("db-ceiling-up", "D+").on_click(
                            cx.listener(|this, _, _, cx| this.adjust_db_ceiling(3.0, cx)),
                        ))
                        .child(
                            viz_control("db-range-down", "R−").on_click(
                                cx.listener(|this, _, _, cx| this.adjust_db_range(-6.0, cx)),
                            ),
                        )
                        .child(
                            viz_control("db-range-up", "R+").on_click(
                                cx.listener(|this, _, _, cx| this.adjust_db_range(6.0, cx)),
                            ),
                        )
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(DIM))
                                .child("FFT/window rebuild evidence · F/D/R are view transfer"),
                        ),
                )
            })
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
        let waveform = analysis.waveform_range(self.time_start, self.time_end, 4_096);
        let features = slice_visible(&analysis.features, self.time_start, self.time_end);
        let clusters = analysis.rhythm.event_clusters.clone();
        let cluster_count = clusters.len().max(1);
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
                        "pulse contrast {:.0}%  ·  {} spectral recurrence clusters  ·  {} events",
                        analysis.rhythm.pulse_contrast * 100.0,
                        analysis.rhythm.event_clusters.len(),
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
                            .children(clusters.into_iter().enumerate().map(
                                move |(index, cluster)| {
                                    div()
                                        .h(relative(1.0 / cluster_count as f32))
                                        .px_2()
                                        .flex()
                                        .flex_col()
                                        .justify_center()
                                        .border_b_1()
                                        .border_color(rgb(BORDER))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(cluster_color(index))
                                                .child(cluster.label),
                                        )
                                        .child(div().text_xs().text_color(rgb(DIM)).child(format!(
                                            "{} · {} events · {:.0}% template similarity",
                                            format_frequency(cluster.centroid_hz),
                                            cluster.event_count,
                                            cluster.consistency * 100.0
                                        )))
                                        .child(cluster_spectrum_plot(
                                            cluster.spectrum,
                                            cluster_rgba(index),
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
                            .child(event_cluster_plot(
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
                    .child("Clusters group similar mixed-audio event spectra; they are not isolated instruments or samples."),
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

    fn render_components(
        &self,
        analysis: Arc<Analysis>,
        playhead: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let timeline_bounds = self.timeline_bounds.clone();
        let start_seconds = analysis.duration_seconds * self.time_start;
        let end_seconds = analysis.duration_seconds * self.time_end;
        let decomposition = analysis.components.clone();
        let components = decomposition.components.clone();
        let component_count = components.len().max(1);

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
                            .child(format!("{} components", components.len())),
                    )
                    .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                        "{:.0}% explained energy  ·  {:.1}% relative magnitude error  ·  {} iterations",
                        decomposition.explained_energy * 100.0,
                        decomposition.relative_error * 100.0,
                        decomposition.iterations_run
                    ))),
            )
            .child(time_ruler_range(start_seconds, end_seconds))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(360.0))
                    .flex()
                    .child(
                        div()
                            .w(px(210.0))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .bg(rgb(PANEL_ALT))
                            .border_r_1()
                            .border_color(rgb(BORDER))
                            .children(components.into_iter().enumerate().map(
                                move |(index, component)| {
                                    div()
                                        .h(relative(1.0 / component_count as f32))
                                        .px_2()
                                        .flex()
                                        .flex_col()
                                        .justify_center()
                                        .border_b_1()
                                        .border_color(rgb(BORDER))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(cluster_color(index))
                                                .child(format!("Component C{}", index + 1)),
                                        )
                                        .child(div().text_xs().text_color(rgb(DIM)).child(format!(
                                            "{:.1}% energy · {:.0}% distinct",
                                            component.energy_share * 100.0,
                                            component.spectral_distinctness * 100.0,
                                        )))
                                        .child(cluster_spectrum_plot(
                                            component.spectral_template,
                                            cluster_rgba(index),
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
                            .child(component_activation_plot(
                                decomposition,
                                self.time_start,
                                self.time_end,
                                playhead,
                            ))
                            .child(timeline_overlay(timeline_bounds, playhead)),
                    ),
            )
            .child(
                div()
                    .h(px(34.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px_4()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child("NMF factors recurring mixed-audio magnitude shapes; components are hypotheses, not isolated sources or instrument labels."),
            )
    }

    fn render_separation(
        &self,
        analysis: Arc<Analysis>,
        playhead_seconds: f64,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match &self.hpss_state {
            HpssViewState::Idle => empty_state(
                "No selected-span decomposition yet",
                "Frame a span of at most 30 seconds, then choose Analyze view.",
            ),
            HpssViewState::Analyzing {
                start_seconds,
                end_seconds,
            } => empty_state(
                "Separating sustained and transient evidence…",
                &format!(
                    "Analyzing {}–{} with a reconstructible complex STFT and complementary soft masks.",
                    format_time(*start_seconds),
                    format_time(*end_seconds)
                ),
            ),
            HpssViewState::Failed(error) => {
                empty_state("The selected-span transform failed", error)
            }
            HpssViewState::Ready(result) => {
                let diagnostics = result.separation.diagnostics;
                let null_db = if diagnostics.relative_reconstruction_error <= 1.0e-9 {
                    -180.0
                } else {
                    20.0 * diagnostics.relative_reconstruction_error.log10()
                };
                let result_playhead = ((playhead_seconds - result.start_seconds)
                    / (result.end_seconds - result.start_seconds).max(f64::EPSILON))
                    as f32;
                let original = mono_waveform_bins(&result.original, 3_000);
                let harmonic = mono_waveform_bins(&result.separation.harmonic, 3_000);
                let percussive = mono_waveform_bins(&result.separation.percussive, 3_000);
                let residual = mono_waveform_bins(&result.separation.residual, 3_000);
                let result_span = (result.end_seconds - result.start_seconds).max(f64::EPSILON);
                let requested_start = analysis.duration_seconds * self.time_start;
                let requested_end = analysis.duration_seconds * self.time_end;
                let stale = (requested_start - result.start_seconds).abs() > result_span * 0.002
                    || (requested_end - result.end_seconds).abs() > result_span * 0.002;

                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(42.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px_4()
                            .gap_4()
                            .bg(rgb(PANEL_ALT))
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(
                                div().text_color(rgb(CYAN)).child(format!(
                                    "mask separation {:.0}%",
                                    diagnostics.mask_confidence * 100.0
                                )),
                            )
                            .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                                "mixture null {:.1} dB  ·  FFT {} / hop {}  ·  {}",
                                null_db,
                                result.separation.settings.fft_size,
                                result.separation.settings.hop_size,
                                if stale { "view changed — reanalyze to update" } else { "selected span is current" }
                            )))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(
                                        viz_control("hear-hpss-original", "Hear mix").px_2().on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.audition_hpss(HpssAudition::Original, cx)
                                            }),
                                        ),
                                    )
                                    .child(
                                        viz_control("hear-hpss-harmonic", "Hear sustained")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.audition_hpss(HpssAudition::Harmonic, cx)
                                            })),
                                    )
                                    .child(
                                        viz_control("hear-hpss-percussive", "Hear transient")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.audition_hpss(HpssAudition::Percussive, cx)
                                            })),
                                    )
                                    .child(
                                        viz_control("hear-hpss-residual", "Hear null")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.audition_hpss(HpssAudition::Residual, cx)
                                            })),
                                    ),
                            ),
                    )
                    .child(time_ruler_range(result.start_seconds, result.end_seconds))
                    .child(lane(
                        "ORIGINAL MIX / SELECTED ASPECT",
                        px(120.0),
                        waveform_plot(original, result_playhead),
                    ))
                    .child(lane(
                        "TONALLY SUSTAINED ESTIMATE",
                        px(120.0),
                        waveform_plot(harmonic, result_playhead),
                    ))
                    .child(lane(
                        "TRANSIENT ESTIMATE",
                        px(120.0),
                        waveform_plot(percussive, result_playhead),
                    ))
                    .child(lane(
                        "MIXTURE NULL (ORIGINAL − ESTIMATES)",
                        px(92.0),
                        waveform_plot(residual, result_playhead),
                    ))
                    .child(
                        div()
                            .h(px(38.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px_4()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("HPSS separates time-persistent from frequency-broad evidence. It is auditionable and additive, but it is not an instrument or vocal classifier."),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_loom(
        &self,
        _analysis: Arc<Analysis>,
        playhead_seconds: f64,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match &self.loom_state {
            LoomViewState::Idle => empty_state(
                "No editable reconstruction yet",
                "Infer recurring excerpts and their event sequence for this material.",
            ),
            LoomViewState::Inferring {
                start_seconds,
                end_seconds,
                event_count,
            } => empty_state(
                "Inferring reusable event templates…",
                &format!(
                    "Aligning {event_count} mixed-signal occurrences, then rendering {}–{}.",
                    format_time(*start_seconds),
                    format_time(*end_seconds)
                ),
            ),
            LoomViewState::Failed(error) => empty_state("The sequence hypothesis failed", error),
            LoomViewState::Ready(result) => {
                let cluster_count = result.sketch.clusters.len();
                let selected = result
                    .sketch
                    .clusters
                    .get(result.selected_cluster.min(cluster_count.saturating_sub(1)));
                let selected_cluster_id = selected
                    .map(|cluster| cluster.template.cluster_id)
                    .unwrap_or(0);
                let selected_events = result
                    .sketch
                    .events
                    .iter()
                    .filter(|event| event.cluster_id == selected_cluster_id)
                    .count();
                let selected_gain = selected.map_or(0.0, |cluster| cluster.gain);
                let selected_enabled = selected.is_some_and(|cluster| cluster.enabled);
                let agreement = selected.map_or(0.0, |cluster| cluster.template.exemplar_agreement);
                let template = selected
                    .map(|cluster| mono_waveform_bins(&cluster.template.samples, 1_200))
                    .unwrap_or_default();
                let local_playhead = ((playhead_seconds - result.start_seconds)
                    / (result.end_seconds - result.start_seconds).max(f64::EPSILON))
                    as f32;
                let original = mono_waveform_bins(&result.original, 2_400);
                let reconstruction = mono_waveform_bins(&result.reconstruction, 2_400);
                let residual = mono_waveform_bins(&result.residual, 2_400);
                let explained = result.fit.explained_energy * 100.0;

                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(42.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px_4()
                            .gap_3()
                            .bg(rgb(PANEL_ALT))
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(div().text_color(rgb(CYAN)).child(format!(
                                "{explained:.1}% source energy explained"
                            )))
                            .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                                "correlation {:+.3}  ·  {} templates / {} events  ·  editable overlap-add render",
                                result.fit.correlation,
                                cluster_count,
                                result.sketch.events.len(),
                            )))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(viz_control("hear-loom-mix", "Mix").px_2().on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.audition_loom(LoomAudition::Original, cx)
                                        }),
                                    ))
                                    .child(
                                        viz_control("hear-loom-render", "Render")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.audition_loom(
                                                    LoomAudition::Reconstruction,
                                                    cx,
                                                )
                                            })),
                                    )
                                    .child(
                                        viz_control("hear-loom-residual", "Residual")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.audition_loom(LoomAudition::Residual, cx)
                                            })),
                                    )
                                    .child(
                                        viz_control("hear-loom-template", "Template")
                                            .px_2()
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.audition_loom(LoomAudition::Template, cx)
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .h(px(42.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px_4()
                            .gap_1()
                            .bg(rgb(PANEL))
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(viz_control("loom-cluster-prev", "Cluster ‹").px_2().on_click(
                                cx.listener(|this, _, _, cx| this.cycle_loom_cluster(-1, cx)),
                            ))
                            .child(
                                div()
                                    .min_w(px(178.0))
                                    .px_2()
                                    .text_xs()
                                    .text_color(cluster_color(selected_cluster_id))
                                    .child(format!(
                                        "Cluster {} · {} events · {:.0}% agreement",
                                        selected_cluster_id + 1,
                                        selected_events,
                                        agreement * 100.0
                                    )),
                            )
                            .child(viz_control("loom-cluster-next", "Cluster ›").px_2().on_click(
                                cx.listener(|this, _, _, cx| this.cycle_loom_cluster(1, cx)),
                            ))
                            .child(viz_control("loom-cluster-toggle", "Mute/on").px_2().on_click(
                                cx.listener(|this, _, _, cx| this.toggle_loom_cluster(cx)),
                            ))
                            .child(viz_control("loom-cluster-gain-down", "Gain −").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.adjust_loom_cluster_gain(-0.1, cx)
                                }),
                            ))
                            .child(
                                div().min_w(px(48.0)).text_xs().text_color(if selected_enabled {
                                    rgb(TEXT)
                                } else {
                                    rgb(DIM)
                                }).child(format!("{selected_gain:.2}×")),
                            )
                            .child(viz_control("loom-cluster-gain-up", "Gain +").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.adjust_loom_cluster_gain(0.1, cx)
                                }),
                            ))
                            .child(div().w(px(10.0)))
                            .child(viz_control("loom-event-left", "Event −10ms").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.edit_nearest_loom_event(-0.010, 0.0, false, cx)
                                }),
                            ))
                            .child(viz_control("loom-event-right", "Event +10ms").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.edit_nearest_loom_event(0.010, 0.0, false, cx)
                                }),
                            ))
                            .child(viz_control("loom-event-gain-down", "Ev −").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.edit_nearest_loom_event(0.0, -0.1, false, cx)
                                }),
                            ))
                            .child(viz_control("loom-event-gain-up", "Ev +").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.edit_nearest_loom_event(0.0, 0.1, false, cx)
                                }),
                            ))
                            .child(viz_control("loom-event-toggle", "Ev on/off").px_2().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.edit_nearest_loom_event(0.0, 0.0, true, cx)
                                }),
                            )),
                    )
                    .child(time_ruler_range(result.start_seconds, result.end_seconds))
                    .child(lane(
                        "SELECTED REUSABLE MIXED-SIGNAL TEMPLATE",
                        px(78.0),
                        waveform_plot(template, -1.0),
                    ))
                    .child(lane(
                        "EDITABLE EVENT SEQUENCE · HEIGHT = GAIN · DIM = DISABLED",
                        px(150.0),
                        loom_event_plot(
                            result.sketch.clone(),
                            result.start_seconds,
                            result.end_seconds,
                            local_playhead,
                            selected_cluster_id,
                        ),
                    ))
                    .child(lane(
                        "ORIGINAL MIX",
                        px(78.0),
                        waveform_plot(original, local_playhead),
                    ))
                    .child(lane(
                        "EVENT-TEMPLATE RECONSTRUCTION",
                        px(78.0),
                        waveform_plot(reconstruction, local_playhead),
                    ))
                    .child(lane(
                        "UNEXPLAINED RESIDUAL · ORIGINAL − RECONSTRUCTION",
                        px(78.0),
                        waveform_plot(residual, local_playhead),
                    ))
                    .child(
                        div()
                            .h(px(34.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px_4()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child("Edits target the selected cluster and its event nearest the shared playhead. Templates are real aligned excerpts from the mix, so overlapping voices and effects leak into them."),
                    )
                    .into_any_element()
            }
        }
    }
}

fn build_loom_result(
    sketch: SequenceSketch,
    source: &[f32],
    start_sample: usize,
    end_sample: usize,
    sample_rate: u32,
    selected_cluster: usize,
) -> LoomViewResult {
    let start_sample = start_sample.min(source.len());
    let end_sample = end_sample.min(source.len()).max(start_sample);
    let original = source[start_sample..end_sample].to_vec();
    let reconstruction = sketch.render_span(start_sample, original.len());
    let residual = original
        .iter()
        .zip(&reconstruction)
        .map(|(source, rendered)| source - rendered)
        .collect::<Vec<_>>();
    let fit = sketch.fit_span(source, start_sample, original.len());
    LoomViewResult {
        sketch,
        selected_cluster,
        start_sample,
        start_seconds: start_sample as f64 / f64::from(sample_rate),
        end_seconds: end_sample as f64 / f64::from(sample_rate),
        sample_rate,
        original,
        reconstruction,
        residual,
        fit,
    }
}

fn update_loom_render(
    result: &mut LoomViewResult,
    original: Vec<f32>,
    start_sample: usize,
    end_sample: usize,
    sample_rate: u32,
) {
    result.start_sample = start_sample;
    result.start_seconds = start_sample as f64 / f64::from(sample_rate);
    result.end_seconds = end_sample as f64 / f64::from(sample_rate);
    result.sample_rate = sample_rate;
    result.original = original;
    rebuild_loom_audio(result);
}

fn rebuild_loom_audio(result: &mut LoomViewResult) {
    result.reconstruction = result
        .sketch
        .render_span(result.start_sample, result.original.len());
    result.residual = result
        .original
        .iter()
        .zip(&result.reconstruction)
        .map(|(source, rendered)| source - rendered)
        .collect();
    result.fit = fit_rendered_span(
        &result.original,
        &result.reconstruction,
        result.start_sample,
    );
}

fn fit_rendered_span(source: &[f32], rendered: &[f32], start_sample: usize) -> FitMetrics {
    let mut source_energy = 0.0_f64;
    let mut rendered_energy = 0.0_f64;
    let mut residual_energy = 0.0_f64;
    let mut dot = 0.0_f64;
    for (&source, &rendered) in source.iter().zip(rendered) {
        let source = f64::from(source);
        let rendered = f64::from(rendered);
        let residual = source - rendered;
        source_energy += source * source;
        rendered_energy += rendered * rendered;
        residual_energy += residual * residual;
        dot += source * rendered;
    }
    let normalized_error = if source_energy <= f64::EPSILON {
        if residual_energy <= f64::EPSILON {
            0.0
        } else {
            f64::INFINITY
        }
    } else {
        residual_energy / source_energy
    };
    let correlation_denominator = (source_energy * rendered_energy).sqrt();
    let correlation = if correlation_denominator <= f64::EPSILON {
        if source_energy <= f64::EPSILON && rendered_energy <= f64::EPSILON {
            1.0
        } else {
            0.0
        }
    } else {
        (dot / correlation_denominator).clamp(-1.0, 1.0) as f32
    };
    FitMetrics {
        start_sample,
        sample_count: source.len().min(rendered.len()),
        source_energy,
        rendered_energy,
        residual_energy,
        normalized_error,
        explained_energy: 1.0 - normalized_error,
        correlation,
    }
}

fn selected_loom_cluster_id(result: &LoomViewResult) -> Option<usize> {
    result
        .sketch
        .clusters
        .get(result.selected_cluster)
        .map(|cluster| cluster.template.cluster_id)
}

fn nearest_loom_event(
    sketch: &SequenceSketch,
    cluster_id: usize,
    playhead_sample: i64,
) -> Option<u64> {
    sketch
        .events
        .iter()
        .filter(|event| event.cluster_id == cluster_id)
        .min_by_key(|event| event.sample_index.abs_diff(playhead_sample))
        .map(|event| event.id)
}

impl Focusable for Visualizer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Visualizer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (analysis, shared_spectrogram, playhead_seconds, is_playing) = {
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

        if let Some(analysis) = &analysis {
            self.follow_playhead_if_needed(analysis, playhead_seconds);
        }

        if let Some(analysis) = &analysis {
            if self.spectrogram_source.as_ref() != Some(&analysis.path) {
                self.spectrum_settings.db_ceiling = analysis.spectral_peak_db;
                self.spectrum_settings.db_range = 84.0;
                self.local_spectrogram = None;
                self.local_spectral_db = None;
                self.spectrum_transforming = false;
                self.hpss_state = HpssViewState::Idle;
                self.loom_state = LoomViewState::Idle;
                if matches!(self.kind, VizKind::Separation | VizKind::Loom) {
                    let span =
                        (18.0 / analysis.duration_seconds.max(f64::EPSILON)).clamp(0.0025, 1.0);
                    let center = (playhead_seconds / analysis.duration_seconds.max(f64::EPSILON))
                        .clamp(0.0, 1.0);
                    self.time_start = (center - span * 0.5).clamp(0.0, 1.0 - span);
                    self.time_end = self.time_start + span;
                }
                self.spectrogram_source = Some(analysis.path.clone());
            }
        }
        let spectrogram = self.local_spectrogram.clone().or(shared_spectrogram);

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
                (VizKind::Components, _) => self
                    .render_components(analysis.clone(), playhead, cx)
                    .into_any_element(),
                (VizKind::Separation, _) => {
                    self.render_separation(analysis.clone(), playhead_seconds, cx)
                }
                (VizKind::Loom, _) => self.render_loom(analysis.clone(), playhead_seconds, cx),
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
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                let delta = event.delta.pixel_delta(window.line_height());
                let horizontal = delta.x / px(100.0);
                let vertical = delta.y / px(100.0);
                let dominant = if horizontal.abs() > vertical.abs() {
                    horizontal
                } else {
                    vertical
                };
                if dominant.abs() > 0.001 {
                    this.pan_time(-f64::from(dominant) * 0.18, cx);
                    cx.stop_propagation();
                }
            }))
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

fn mono_waveform_bins(samples: &[f32], target_bins: usize) -> Vec<WaveformBin> {
    if samples.is_empty() || target_bins == 0 {
        return Vec::new();
    }
    let bin_count = target_bins.min(samples.len());
    (0..bin_count)
        .map(|bin| {
            let start = samples.len() * bin / bin_count;
            let end = samples.len() * (bin + 1) / bin_count;
            let mut minimum = f32::INFINITY;
            let mut maximum = f32::NEG_INFINITY;
            for sample in samples[start..end].iter().copied() {
                minimum = minimum.min(sample);
                maximum = maximum.max(sample);
            }
            WaveformBin {
                left_min: minimum,
                left_max: maximum,
                right_min: minimum,
                right_max: maximum,
            }
        })
        .collect()
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
            for time in rhythm.beat_times.iter().copied() {
                if time < start_seconds || time > end_seconds {
                    continue;
                }
                let fraction = ((time - start_seconds) / duration) as f32;
                let x = bounds.origin.x + bounds.size.width * fraction;
                window.paint_quad(quad(
                    Bounds::new(
                        point(x, bounds.origin.y),
                        gpui::size(px(1.0), bounds.size.height),
                    ),
                    px(0.0),
                    rgba(0xffffff18),
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

fn event_cluster_plot(
    rhythm: RhythmAnalysis,
    start_seconds: f64,
    end_seconds: f64,
    playhead: f32,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            let duration = (end_seconds - start_seconds).max(f64::EPSILON);
            let rows = rhythm.event_clusters.len().max(1);
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
            for time in rhythm.beat_times.iter().copied() {
                if time < start_seconds || time > end_seconds {
                    continue;
                }
                let fraction = ((time - start_seconds) / duration) as f32;
                let x = bounds.origin.x + bounds.size.width * fraction;
                window.paint_quad(quad(
                    Bounds::new(
                        point(x, bounds.origin.y),
                        gpui::size(px(1.0), bounds.size.height),
                    ),
                    px(0.0),
                    rgba(0xffffff10),
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
                let row = onset.cluster.min(rows - 1);
                let height = row_height
                    * (0.18 + 0.72 * onset.strength * onset.template_similarity.max(0.35));
                let bottom = bounds.origin.y + row_height * (row as f32 + 0.92);
                window.paint_quad(quad(
                    Bounds::new(
                        point(x - px(1.25), bottom - height),
                        gpui::size(px(2.5), height),
                    ),
                    px(1.0),
                    cluster_rgba(row),
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

fn loom_event_plot(
    sketch: SequenceSketch,
    start_seconds: f64,
    end_seconds: f64,
    playhead: f32,
    selected_cluster_id: usize,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            let duration = (end_seconds - start_seconds).max(f64::EPSILON);
            let rows = sketch.clusters.len().max(1);
            let row_height = bounds.size.height / rows as f32;
            if let Some(selected_row) = sketch
                .clusters
                .iter()
                .position(|cluster| cluster.template.cluster_id == selected_cluster_id)
            {
                window.paint_quad(quad(
                    Bounds::new(
                        point(
                            bounds.origin.x,
                            bounds.origin.y + row_height * selected_row as f32,
                        ),
                        gpui::size(bounds.size.width, row_height),
                    ),
                    px(0.0),
                    rgba(0x50d8d70d),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            for row in 1..rows {
                let y = bounds.origin.y + row_height * row as f32;
                window.paint_quad(quad(
                    Bounds::new(
                        point(bounds.origin.x, y),
                        gpui::size(bounds.size.width, px(1.0)),
                    ),
                    px(0.0),
                    rgba(0xffffff12),
                    px(0.0),
                    rgba(0x00000000),
                    Default::default(),
                ));
            }
            for event in &sketch.events {
                let seconds = event.sample_index as f64 / f64::from(sketch.sample_rate);
                if seconds < start_seconds || seconds > end_seconds {
                    continue;
                }
                let Some(row) = sketch
                    .clusters
                    .iter()
                    .position(|cluster| cluster.template.cluster_id == event.cluster_id)
                else {
                    continue;
                };
                let cluster_enabled = sketch.clusters[row].enabled;
                let fraction = ((seconds - start_seconds) / duration) as f32;
                let x = bounds.origin.x + bounds.size.width * fraction;
                let height = row_height * (0.18 + 0.68 * event.gain.abs().clamp(0.0, 1.6) / 1.6);
                let bottom = bounds.origin.y + row_height * (row as f32 + 0.90);
                let color = if event.enabled && cluster_enabled {
                    cluster_rgba(event.cluster_id)
                } else {
                    rgba(0x59657966)
                };
                window.paint_quad(quad(
                    Bounds::new(
                        point(x - px(1.5), bottom - height),
                        gpui::size(px(3.0), height),
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

fn component_activation_plot(
    decomposition: ComponentDecomposition,
    start: f64,
    end: f64,
    playhead: f32,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            let rows = decomposition.components.len().max(1);
            let row_height = bounds.size.height / rows as f32;
            let first = (start.clamp(0.0, 1.0) * decomposition.frames as f64).floor() as usize;
            let last = (end.clamp(0.0, 1.0) * decomposition.frames as f64).ceil() as usize;
            let first = first.min(decomposition.frames.saturating_sub(1));
            let last = last.clamp(first + 1, decomposition.frames);

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

            for (row, component) in decomposition.components.iter().enumerate() {
                let values = &component.activation[first..last];
                if values.len() < 2 {
                    continue;
                }
                let peak = values.iter().copied().fold(0.0_f32, f32::max).max(1.0e-9);
                let top = bounds.origin.y + row_height * row as f32;
                let bottom = top + row_height;
                let mut builder = PathBuilder::fill();
                builder.move_to(point(bounds.origin.x, bottom));
                for (index, value) in values.iter().copied().enumerate() {
                    let fraction = index as f32 / (values.len() - 1) as f32;
                    builder.line_to(point(
                        bounds.origin.x + bounds.size.width * fraction,
                        bottom - row_height * 0.88 * (value / peak).sqrt().clamp(0.0, 1.0),
                    ));
                }
                builder.line_to(point(bounds.origin.x + bounds.size.width, bottom));
                builder.close();
                if let Ok(path) = builder.build() {
                    window.paint_path(path, cluster_rgba(row));
                }
            }
            paint_playhead(bounds, playhead, window);
        },
    )
    .size_full()
}

fn cluster_color(index: usize) -> gpui::Rgba {
    rgb([
        CYAN, MAGENTA, AMBER, LIME, 0x8e9cff, 0xe99172, 0x78d5a3, 0xd8a7ff,
    ][index % 8])
}

fn cluster_rgba(index: usize) -> gpui::Rgba {
    rgba(
        [
            0x50d8d7dd, 0xf172b6dd, 0xf6b760dd, 0xa7d877dd, 0x8e9cffdd, 0xe99172dd, 0x78d5a3dd,
            0xd8a7ffdd,
        ][index % 8],
    )
}

fn cluster_spectrum_plot(spectrum: Vec<f32>, color: gpui::Rgba) -> impl IntoElement {
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
