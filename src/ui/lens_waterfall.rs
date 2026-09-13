//! Spectral waterfall lens.
//!
//! Split from `ui.rs`; behaviour-preserving. Private items of the parent
//! module are reachable through `use super::*`.

use super::*;

impl Visualizer {
    pub(super) fn rebuild_spectrogram(&mut self, cx: &mut Context<Self>) {
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
            self.say(
                "No material is open, so there is no spectral field to restyle".into(),
                cx,
            );
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

    pub(super) fn adjust_db_ceiling(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.spectrum_settings.db_ceiling =
            (self.spectrum_settings.db_ceiling + delta).clamp(-120.0, 24.0);
        self.rebuild_spectrogram(cx);
    }

    pub(super) fn adjust_db_range(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.spectrum_settings.db_range =
            (self.spectrum_settings.db_range + delta).clamp(6.0, 180.0);
        self.remember_spectrum_choices();
        self.rebuild_spectrogram(cx);
    }

    /// Persist the person's spectrum choices; the material-derived ceiling
    /// and range are not remembered. Failure is a diagnostic, never a block.
    pub(super) fn remember_spectrum_choices(&self) {
        let settings = self.spectrum_settings;
        if let Err(error) = crate::preferences::update(|preferences| {
            preferences.spectrum = Some(settings);
        }) {
            eprintln!("preferences not saved: {error}");
        }
    }

    pub(super) fn rerun_spectrum(&mut self, cx: &mut Context<Self>) {
        let settings = self.spectrum_settings;
        let source = self.workbench.read(cx).analysis_arc().map(|analysis| {
            let frames = analysis.waveform_pyramid.frame_count();
            (
                analysis.path.clone(),
                analysis.sample_rate,
                frames,
                analysis,
            )
        });
        let Some((path, sample_rate, frames, analysis)) = source else {
            self.say(
                "No material is open, so there is no spectral transform to run".into(),
                cx,
            );
            return;
        };

        let requested = self.waterfall_freshness.bump();
        self.spectrum_transforming = true;
        cx.notify();
        let task = cx.background_spawn(async move {
            // The field spans the whole material but is read one analysis
            // window at a time: no lens buffer the size of the song.
            let mut read_mono = |range: SpectralFrameRange, out: &mut Vec<f32>| {
                out.extend_from_slice(
                    &analysis.mono_range(range.start as usize, range.end as usize),
                );
            };
            let (values, refused) = match display_field_streamed(
                SPECTROGRAM_WIDTH,
                SPECTROGRAM_HEIGHT,
                frames,
                sample_rate,
                settings,
                &mut read_mono,
            ) {
                Ok(values) => (values, None),
                Err(error) => (
                    fft_display_field_streamed(
                        SPECTROGRAM_WIDTH,
                        SPECTROGRAM_HEIGHT,
                        frames,
                        sample_rate,
                        settings,
                        &mut read_mono,
                    ),
                    Some(error.to_string()),
                ),
            };
            let image = encode_spectrogram(&values, settings.db_ceiling, settings.db_range)
                .map(|bytes| Arc::new(Image::from_bytes(ImageFormat::Png, bytes)))
                .map_err(|error| format!("{error:#}"));
            (values, image, refused)
        });
        cx.spawn(async move |this, cx| {
            let (values, image, refused) = task.await;
            let _ = this.update(cx, |this, cx| {
                if !this.waterfall_freshness.still_current(requested) {
                    return;
                }
                this.spectrum_transforming = false;
                match refused {
                    // The chosen transform cannot run on this material (for
                    // example constant-Q above Nyquist at a low sample rate).
                    // Say so in the musician's channel and show the transform
                    // that was actually computed — but the choice stands. A
                    // refusal is a fact about this material, not a decision,
                    // and overwriting the preference with it would silently
                    // change what the next material is analysed with.
                    Some(reason) => {
                        let message = format!(
                            "{} could not run on this material, so the waterfall is showing FFT · {reason} · your {} choice is kept",
                            settings.transform.label(),
                            settings.transform.label()
                        );
                        this.spectrum_refusal = Some(reason);
                        this.say(message, cx);
                    }
                    None => this.spectrum_refusal = None,
                }
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

    /// The resolution knob of whichever transform is chosen: an FFT length in
    /// samples, or — under constant-Q, where the FFT length is the kernel's
    /// business and not the picture's — pitch steps per octave.
    pub(super) fn change_fft_size(&mut self, direction: i32, cx: &mut Context<Self>) {
        if self.spectrum_settings.transform == SpectralTransform::ConstantQ {
            let before = self.spectrum_settings.cqt_bins_per_octave;
            let after = crate::settings::step_cqt_bins_per_octave(before, direction);
            if after == before {
                let steps = crate::settings::CQT_BINS_PER_OCTAVE_STEPS;
                self.say(
                    format!(
                        "Constant-Q stays at {before} bins per octave · the waterfall offers {}",
                        steps
                            .iter()
                            .map(|value| value.to_string())
                            .collect::<Vec<_>>()
                            .join(" / ")
                    ),
                    cx,
                );
                return;
            }
            self.spectrum_settings.cqt_bins_per_octave = after;
            self.remember_spectrum_choices();
            self.rerun_spectrum(cx);
            return;
        }
        self.spectrum_settings.fft_size = if direction < 0 {
            (self.spectrum_settings.fft_size / 2).max(256)
        } else {
            (self.spectrum_settings.fft_size * 2).min(65_536)
        };
        self.spectrum_settings.hop_size = (self.spectrum_settings.fft_size / 4).max(1);
        self.remember_spectrum_choices();
        self.rerun_spectrum(cx);
    }

    pub(super) fn cycle_transform(&mut self, cx: &mut Context<Self>) {
        self.spectrum_settings.transform = self.spectrum_settings.transform.next();
        // The old refusal was about the old transform; the new run answers
        // for itself.
        self.spectrum_refusal = None;
        self.remember_spectrum_choices();
        self.rerun_spectrum(cx);
    }

    pub(super) fn cycle_window_function(&mut self, cx: &mut Context<Self>) {
        self.spectrum_settings.window = self.spectrum_settings.window.next();
        self.remember_spectrum_choices();
        self.rerun_spectrum(cx);
    }

    /// What the waterfall's size readout says: the resolution of the chosen
    /// transform, the window, and — when the last run was refused — which
    /// transform the picture on screen actually is.
    pub(super) fn spectrum_readout(&self) -> String {
        let resolution = match self.spectrum_settings.transform {
            SpectralTransform::Fft => self.spectrum_settings.fft_size.to_string(),
            SpectralTransform::ConstantQ => {
                format!("{}/oct", self.spectrum_settings.cqt_bins_per_octave)
            }
        };
        format!(
            "{resolution} {}{}{}",
            self.spectrum_settings.window.label(),
            if self.spectrum_transforming {
                " …"
            } else {
                ""
            },
            if self.spectrum_refusal.is_some() {
                format!(
                    " · showing FFT, {} refused",
                    self.spectrum_settings.transform.label()
                )
            } else {
                String::new()
            }
        )
    }

    pub(super) fn render_waterfall(
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
}
