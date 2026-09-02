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
            let (values, refused) = match spectral_field(&mono, sample_rate, settings) {
                Ok(values) => (values, None),
                Err(error) => (
                    spectral_projection(&mono, sample_rate, settings),
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
                if this.spectrum_generation != generation {
                    return;
                }
                this.spectrum_transforming = false;
                if let Some(reason) = refused {
                    // The chosen transform cannot run on this material (for
                    // example constant-Q above Nyquist at a low sample rate).
                    // Say so and show the transform that was actually computed.
                    eprintln!(
                        "{} transform refused, showing FFT: {reason}",
                        settings.transform.label()
                    );
                    this.spectrum_settings.transform = SpectralTransform::Fft;
                    this.remember_spectrum_choices();
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

    pub(super) fn change_fft_size(&mut self, direction: i32, cx: &mut Context<Self>) {
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
        self.remember_spectrum_choices();
        self.rerun_spectrum(cx);
    }

    pub(super) fn cycle_window_function(&mut self, cx: &mut Context<Self>) {
        self.spectrum_settings.window = self.spectrum_settings.window.next();
        self.remember_spectrum_choices();
        self.rerun_spectrum(cx);
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
