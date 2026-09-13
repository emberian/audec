//! Event-template resynthesis for audec's first honest reverse-DAW sketch.
//!
//! This module deliberately makes a narrow claim: when a sound recurs, we can
//! turn those recurrences into an editable sequence of reusable PCM templates.
//! It does not claim that a cluster is an instrument, nor that overlapping
//! sources have been separated.  The resulting [`SequenceSketch`] is small,
//! deterministic, and can render a selected span without retaining or copying
//! the source track.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A neutral observation supplied by an onset/recurrence analysis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EventObservation {
    /// Approximate onset in source PCM frames.
    pub sample_index: usize,
    /// An opaque recurrence-cluster identifier. It is not an instrument label.
    pub cluster_id: usize,
    /// Relative importance of the event, normally in `0..=1`.
    pub salience: f32,
    /// Similarity to the upstream cluster hypothesis, normally in `0..=1`.
    pub template_similarity: f32,
}

/// Controls template extraction and local onset refinement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TemplateBuildConfig {
    /// Context retained before each refined onset.
    pub pre_roll_samples: usize,
    /// Context retained from the onset onward.
    pub post_roll_samples: usize,
    /// Maximum positive or negative correction to an observed onset.
    pub alignment_radius_samples: usize,
    /// Maximum strong occurrences considered when selecting a medoid.
    pub max_exemplars_per_cluster: usize,
}

impl TemplateBuildConfig {
    /// A transient-oriented default: 8 ms before, 240 ms after, and an 8 ms
    /// local alignment search.
    pub fn for_sample_rate(sample_rate: u32) -> Self {
        let samples = |seconds: f64| (f64::from(sample_rate) * seconds).round() as usize;
        Self {
            pre_roll_samples: samples(0.008),
            post_roll_samples: samples(0.240).max(1),
            alignment_radius_samples: samples(0.008),
            max_exemplars_per_cluster: 16,
        }
    }

    pub fn template_len(self) -> usize {
        self.pre_roll_samples.saturating_add(self.post_roll_samples)
    }
}

/// How far before a selection loom looks for the recurrences that build its
/// templates. Long enough that anything worth templating recurs inside it,
/// short enough that what the lens reads is a function of the window rather
/// than the length of the recording. The middle entry is the 60 s this build
/// always used.
pub const LOOM_LOOKBEHIND_SECONDS: [usize; 3] = [30, 60, 120];

/// Template lengths the lens offers, in milliseconds of post-roll. 240 ms is
/// a transient; 1000 ms can hold a chord stab or a short phrase.
pub const LOOM_TEMPLATE_MILLISECONDS: [u32; 4] = [120, 240, 500, 1_000];

/// The two Loom knobs a musician turns. Everything else about template
/// extraction stays [`TemplateBuildConfig::for_sample_rate`]'s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoomLensSettings {
    /// Index into [`LOOM_LOOKBEHIND_SECONDS`].
    pub lookbehind: usize,
    /// Index into [`LOOM_TEMPLATE_MILLISECONDS`].
    pub template_length: usize,
}

impl Default for LoomLensSettings {
    fn default() -> Self {
        Self {
            lookbehind: 1,
            template_length: 1,
        }
    }
}

impl LoomLensSettings {
    /// Clamp indices that came from a preferences file this build did not
    /// write back to the defaults.
    pub fn normalized(mut self) -> Self {
        if self.lookbehind >= LOOM_LOOKBEHIND_SECONDS.len() {
            self.lookbehind = Self::default().lookbehind;
        }
        if self.template_length >= LOOM_TEMPLATE_MILLISECONDS.len() {
            self.template_length = Self::default().template_length;
        }
        self
    }

    pub fn lookbehind_seconds(self) -> usize {
        LOOM_LOOKBEHIND_SECONDS[self.normalized().lookbehind]
    }

    pub fn template_milliseconds(self) -> u32 {
        LOOM_TEMPLATE_MILLISECONDS[self.normalized().template_length]
    }

    /// Move an index by `steps` notches inside its table, saying whether it
    /// moved so a press against the end can be refused in words. The count is
    /// signed and honoured, as the rhythm lens's sensitivity step is.
    fn step(index: &mut usize, steps: i32, length: usize) -> bool {
        let next = (*index as i64 + i64::from(steps)).clamp(0, length as i64 - 1) as usize;
        let moved = next != *index;
        *index = next;
        moved
    }

    pub fn step_lookbehind(&mut self, direction: i32) -> bool {
        *self = self.normalized();
        Self::step(
            &mut self.lookbehind,
            direction,
            LOOM_LOOKBEHIND_SECONDS.len(),
        )
    }

    pub fn step_template_length(&mut self, direction: i32) -> bool {
        *self = self.normalized();
        Self::step(
            &mut self.template_length,
            direction,
            LOOM_TEMPLATE_MILLISECONDS.len(),
        )
    }

    /// The extraction config this lens asks for: the build's pre-roll and
    /// alignment radius, with the chosen post-roll.
    pub fn build_config(self, sample_rate: u32) -> TemplateBuildConfig {
        let mut config = TemplateBuildConfig::for_sample_rate(sample_rate);
        let seconds = f64::from(self.template_milliseconds()) / 1_000.0;
        config.post_roll_samples = ((f64::from(sample_rate) * seconds).round() as usize).max(1);
        config
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoomError {
    InvalidSampleRate,
    EmptyTemplateWindow,
    NoExemplars,
    Cancelled,
}

impl fmt::Display for LoomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSampleRate => f.write_str("sample rate must be non-zero"),
            Self::EmptyTemplateWindow => f.write_str("template window must contain samples"),
            Self::NoExemplars => f.write_str("max exemplars per cluster must be non-zero"),
            Self::Cancelled => f.write_str("Loom inference was cancelled"),
        }
    }
}

impl Error for LoomError {}

#[derive(Clone, Debug, Default)]
pub struct LoomCancellation(Arc<AtomicBool>);

impl LoomCancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn check(&self) -> Result<(), LoomError> {
        if self.is_cancelled() {
            Err(LoomError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// A reusable, phase-preserving PCM representative of a recurrence cluster.
#[derive(Clone, Debug, PartialEq)]
pub struct ClusterTemplate {
    pub cluster_id: usize,
    /// PCM begins `onset_offset` samples before the event time.
    pub samples: Vec<f32>,
    pub onset_offset: usize,
    /// Stable event id whose actual waveform was selected as the medoid.
    pub medoid_event_id: u64,
    pub exemplar_count: usize,
    /// Weighted mean shape agreement between the medoid and exemplars.
    pub exemplar_agreement: f32,
}

/// Editable controls for one reusable cluster template.
#[derive(Clone, Debug, PartialEq)]
pub struct SequenceCluster {
    pub template: ClusterTemplate,
    pub enabled: bool,
    pub gain: f32,
}

/// One editable occurrence in the inferred sequence.
#[derive(Clone, Debug, PartialEq)]
pub struct SequenceEvent {
    pub id: u64,
    pub cluster_id: usize,
    /// Refined onset. Signed time permits an editor to drag an event before 0.
    pub sample_index: i64,
    /// Least-squares amplitude relative to the cluster's medoid template.
    pub gain: f32,
    pub enabled: bool,
    pub salience: f32,
    pub upstream_similarity: f32,
    /// Refined minus observed onset, in samples.
    pub timing_adjustment: i32,
    /// Normalized correlation with the selected template after refinement.
    pub template_correlation: f32,
}

/// A compact, editable hypothesis that can be rendered back into sound.
#[derive(Clone, Debug, PartialEq)]
pub struct SequenceSketch {
    pub sample_rate: u32,
    /// Sorted by opaque cluster id for deterministic lookup and serialization.
    pub clusters: Vec<SequenceCluster>,
    /// Stable in the order of input observations.
    pub events: Vec<SequenceEvent>,
}

/// Energy-domain agreement between the source and a rendered span.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FitMetrics {
    pub start_sample: usize,
    pub sample_count: usize,
    pub source_energy: f64,
    pub rendered_energy: f64,
    pub residual_energy: f64,
    /// `residual_energy / source_energy`; zero for two silent spans.
    pub normalized_error: f64,
    /// `1 - normalized_error`. This can be negative for a harmful hypothesis.
    pub explained_energy: f64,
    pub correlation: f32,
}

/// A bounded view of canonical mono PCM in absolute source coordinates.
///
/// Template extraction reads the material only around the events it is
/// templating, so a lens can hand loom a lookbehind window instead of the
/// whole file. Indices stay absolute -- a sketch's event times are file times
/// -- and anything outside the window reads as silence, exactly as anything
/// outside the material does. A caller that means "all of it" says
/// [`MonoWindow::whole`].
#[derive(Clone, Copy, Debug)]
pub struct MonoWindow<'a> {
    start: usize,
    samples: &'a [f32],
    source_len: usize,
}

impl<'a> MonoWindow<'a> {
    pub fn whole(samples: &'a [f32]) -> Self {
        Self {
            start: 0,
            samples,
            source_len: samples.len(),
        }
    }

    /// `samples` are the source frames `[start, start + samples.len())` of a
    /// material `source_len` frames long.
    pub fn new(start: usize, samples: &'a [f32], source_len: usize) -> Self {
        Self {
            start,
            samples,
            source_len: source_len.max(start.saturating_add(samples.len())),
        }
    }

    /// Frames of material, not frames retained.
    pub fn source_len(&self) -> usize {
        self.source_len
    }

    /// The retained extent, in absolute frames.
    pub fn retained(&self) -> (usize, usize) {
        (self.start, self.start.saturating_add(self.samples.len()))
    }

    pub fn covers(&self, start: usize, end: usize) -> bool {
        let (low, high) = self.retained();
        start >= low && end <= high
    }

    fn sample(&self, index: i64) -> f32 {
        usize::try_from(index)
            .ok()
            .and_then(|index| index.checked_sub(self.start))
            .and_then(|offset| self.samples.get(offset))
            .copied()
            .map(|sample| finite_or(sample, 0.0))
            .unwrap_or(0.0)
    }

    /// The retained frames of `[start, end)`, clamped to what this window
    /// holds. Callers that need the whole span ask [`MonoWindow::covers`]
    /// first.
    fn slice(&self, start: usize, end: usize) -> &'a [f32] {
        let low = start.saturating_sub(self.start).min(self.samples.len());
        let high = end
            .saturating_sub(self.start)
            .clamp(low, self.samples.len());
        &self.samples[low..high]
    }
}

impl SequenceSketch {
    /// Infer a reusable template and editable event sequence from recurrence
    /// observations. Source PCM is borrowed only for construction.
    pub fn infer(
        mono: &[f32],
        sample_rate: u32,
        observations: &[EventObservation],
        config: TemplateBuildConfig,
    ) -> Result<Self, LoomError> {
        Self::infer_cancellable(
            mono,
            sample_rate,
            observations,
            config,
            &LoomCancellation::default(),
        )
    }

    pub fn infer_cancellable(
        mono: &[f32],
        sample_rate: u32,
        observations: &[EventObservation],
        config: TemplateBuildConfig,
        cancellation: &LoomCancellation,
    ) -> Result<Self, LoomError> {
        Self::infer_windowed(
            MonoWindow::whole(mono),
            sample_rate,
            observations,
            config,
            cancellation,
        )
    }

    /// Infer from a bounded window of the material. Every read this makes is
    /// a neighbourhood of one observation, so the window only has to cover
    /// the observations it is given, padded by the template and the alignment
    /// radius.
    pub fn infer_windowed(
        mono: MonoWindow<'_>,
        sample_rate: u32,
        observations: &[EventObservation],
        config: TemplateBuildConfig,
        cancellation: &LoomCancellation,
    ) -> Result<Self, LoomError> {
        cancellation.check()?;
        if sample_rate == 0 {
            return Err(LoomError::InvalidSampleRate);
        }
        if config.template_len() == 0 {
            return Err(LoomError::EmptyTemplateWindow);
        }
        if config.max_exemplars_per_cluster == 0 {
            return Err(LoomError::NoExemplars);
        }

        let mut grouped = BTreeMap::<usize, Vec<usize>>::new();
        for (event_id, observation) in observations.iter().enumerate() {
            cancellation.check()?;
            grouped
                .entry(observation.cluster_id)
                .or_default()
                .push(event_id);
        }

        let mut clusters = Vec::with_capacity(grouped.len());
        let mut inferred = vec![None; observations.len()];

        for (cluster_id, event_ids) in grouped {
            cancellation.check()?;
            let mut ranked = event_ids.clone();
            ranked.sort_by(|left, right| {
                let left_score = observation_quality(observations[*left]);
                let right_score = observation_quality(observations[*right]);
                right_score
                    .total_cmp(&left_score)
                    .then_with(|| {
                        observations[*left]
                            .sample_index
                            .cmp(&observations[*right].sample_index)
                    })
                    .then_with(|| left.cmp(right))
            });
            ranked.truncate(config.max_exemplars_per_cluster);

            let reference_id = ranked[0];
            let reference_observation = observations[reference_id];
            let reference = extract_window(
                mono,
                reference_observation.sample_index as i64 - config.pre_roll_samples as i64,
                config.template_len(),
            );

            let mut exemplars = Vec::with_capacity(ranked.len());
            for event_id in ranked {
                cancellation.check()?;
                let observation = observations[event_id];
                let (shift, correlation) = best_alignment(
                    mono,
                    observation.sample_index as i64,
                    config.pre_roll_samples,
                    &reference,
                    config.alignment_radius_samples,
                );
                let aligned_onset = observation.sample_index as i64 + shift;
                exemplars.push(Exemplar {
                    event_id,
                    samples: extract_window(
                        mono,
                        aligned_onset - config.pre_roll_samples as i64,
                        config.template_len(),
                    ),
                    weight: 0.05 + observation_quality(observation),
                    reference_correlation: correlation,
                });
            }

            let medoid_index = select_medoid(&exemplars);
            let medoid = &exemplars[medoid_index];
            let agreement = weighted_agreement(medoid_index, &exemplars);
            let template = medoid.samples.clone();

            clusters.push(SequenceCluster {
                template: ClusterTemplate {
                    cluster_id,
                    samples: template.clone(),
                    onset_offset: config.pre_roll_samples,
                    medoid_event_id: medoid.event_id as u64,
                    exemplar_count: exemplars.len(),
                    exemplar_agreement: agreement,
                },
                enabled: true,
                gain: 1.0,
            });

            for event_id in event_ids {
                cancellation.check()?;
                let observation = observations[event_id];
                let (shift, correlation) = best_alignment(
                    mono,
                    observation.sample_index as i64,
                    config.pre_roll_samples,
                    &template,
                    config.alignment_radius_samples,
                );
                let refined_onset = observation.sample_index as i64 + shift;
                let window = extract_window(
                    mono,
                    refined_onset - config.pre_roll_samples as i64,
                    config.template_len(),
                );
                let gain = least_squares_gain(&template, &window);
                inferred[event_id] = Some(SequenceEvent {
                    id: event_id as u64,
                    cluster_id,
                    sample_index: refined_onset,
                    gain,
                    enabled: true,
                    salience: finite_or(observation.salience, 0.0),
                    upstream_similarity: finite_or(observation.template_similarity, 0.0),
                    timing_adjustment: shift.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
                    template_correlation: finite_or(correlation, 0.0),
                });
            }
        }

        Ok(Self {
            sample_rate,
            clusters,
            events: inferred.into_iter().flatten().collect(),
        })
    }

    pub fn cluster(&self, cluster_id: usize) -> Option<&SequenceCluster> {
        self.clusters
            .binary_search_by_key(&cluster_id, |cluster| cluster.template.cluster_id)
            .ok()
            .map(|index| &self.clusters[index])
    }

    pub fn cluster_mut(&mut self, cluster_id: usize) -> Option<&mut SequenceCluster> {
        self.clusters
            .binary_search_by_key(&cluster_id, |cluster| cluster.template.cluster_id)
            .ok()
            .map(|index| &mut self.clusters[index])
    }

    pub fn event(&self, event_id: u64) -> Option<&SequenceEvent> {
        self.events.iter().find(|event| event.id == event_id)
    }

    pub fn event_mut(&mut self, event_id: u64) -> Option<&mut SequenceEvent> {
        self.events.iter_mut().find(|event| event.id == event_id)
    }

    pub fn set_cluster_enabled(&mut self, cluster_id: usize, enabled: bool) -> bool {
        let Some(cluster) = self.cluster_mut(cluster_id) else {
            return false;
        };
        cluster.enabled = enabled;
        true
    }

    pub fn set_cluster_gain(&mut self, cluster_id: usize, gain: f32) -> bool {
        let Some(cluster) = self.cluster_mut(cluster_id) else {
            return false;
        };
        cluster.gain = finite_or(gain, 0.0);
        true
    }

    pub fn set_event_enabled(&mut self, event_id: u64, enabled: bool) -> bool {
        let Some(event) = self.event_mut(event_id) else {
            return false;
        };
        event.enabled = enabled;
        true
    }

    pub fn set_event_gain(&mut self, event_id: u64, gain: f32) -> bool {
        let Some(event) = self.event_mut(event_id) else {
            return false;
        };
        event.gain = finite_or(gain, 0.0);
        true
    }

    pub fn move_event(&mut self, event_id: u64, sample_index: i64) -> bool {
        let Some(event) = self.event_mut(event_id) else {
            return false;
        };
        event.sample_index = sample_index;
        true
    }

    /// Render only `sample_count` samples beginning at an absolute source time.
    pub fn render_span(&self, start_sample: usize, sample_count: usize) -> Vec<f32> {
        let mut output = vec![0.0; sample_count];
        self.render_span_into(start_sample, &mut output);
        output
    }

    /// Clear and fill a caller-provided selected-span buffer by overlap-add.
    pub fn render_span_into(&self, start_sample: usize, output: &mut [f32]) {
        output.fill(0.0);
        if output.is_empty() {
            return;
        }

        let span_start = start_sample as i128;
        let span_end = span_start + output.len() as i128;
        for event in self.events.iter().filter(|event| event.enabled) {
            let Some(cluster) = self.cluster(event.cluster_id) else {
                continue;
            };
            if !cluster.enabled {
                continue;
            }
            let template = &cluster.template.samples;
            let event_start = event.sample_index as i128 - cluster.template.onset_offset as i128;
            let event_end = event_start + template.len() as i128;
            let overlap_start = span_start.max(event_start);
            let overlap_end = span_end.min(event_end);
            if overlap_start >= overlap_end {
                continue;
            }

            let output_start = (overlap_start - span_start) as usize;
            let template_start = (overlap_start - event_start) as usize;
            let count = (overlap_end - overlap_start) as usize;
            let gain = finite_or(cluster.gain, 0.0) * finite_or(event.gain, 0.0);
            for offset in 0..count {
                output[output_start + offset] += template[template_start + offset] * gain;
            }
        }
    }

    /// Return source minus reconstruction for a selected source span.
    pub fn residual_span(
        &self,
        source_mono: &[f32],
        start_sample: usize,
        sample_count: usize,
    ) -> Vec<f32> {
        let count = source_mono
            .len()
            .saturating_sub(start_sample)
            .min(sample_count);
        let rendered = self.render_span(start_sample, count);
        rendered
            .into_iter()
            .enumerate()
            .map(|(offset, sample)| finite_or(source_mono[start_sample + offset], 0.0) - sample)
            .collect()
    }

    /// Measure a selected span. Source PCM is not stored in the sketch.
    pub fn fit_span(
        &self,
        source_mono: &[f32],
        start_sample: usize,
        sample_count: usize,
    ) -> FitMetrics {
        self.fit_span_windowed(MonoWindow::whole(source_mono), start_sample, sample_count)
    }

    /// Measure a selected span held in a bounded window. The window must
    /// cover the span; what it does not hold measures as silence.
    pub fn fit_span_windowed(
        &self,
        source_mono: MonoWindow<'_>,
        start_sample: usize,
        sample_count: usize,
    ) -> FitMetrics {
        let count = source_mono
            .source_len()
            .saturating_sub(start_sample)
            .min(sample_count);
        let rendered = self.render_span(start_sample, count);
        measure_fit(
            source_mono.slice(start_sample, start_sample.saturating_add(count)),
            &rendered,
            start_sample,
        )
    }
}

#[derive(Clone, Debug)]
struct Exemplar {
    event_id: usize,
    samples: Vec<f32>,
    weight: f32,
    reference_correlation: f32,
}

fn observation_quality(observation: EventObservation) -> f32 {
    let salience = finite_or(observation.salience, 0.0).max(0.0);
    let similarity = finite_or(observation.template_similarity, 0.0).max(0.0);
    salience * (0.25 + 0.75 * similarity)
}

fn select_medoid(exemplars: &[Exemplar]) -> usize {
    let mut best_index = 0;
    let mut best_score = f64::NEG_INFINITY;
    for (candidate_index, candidate) in exemplars.iter().enumerate() {
        let mut weighted_similarity = 0.0_f64;
        let mut total_weight = 0.0_f64;
        for other in exemplars {
            let weight = f64::from(other.weight.max(0.0));
            weighted_similarity +=
                f64::from(normalized_correlation(&candidate.samples, &other.samples)) * weight;
            total_weight += weight;
        }
        let score = if total_weight > 0.0 {
            weighted_similarity / total_weight
        } else {
            0.0
        };
        if score > best_score {
            best_score = score;
            best_index = candidate_index;
        }
    }
    best_index
}

fn weighted_agreement(medoid_index: usize, exemplars: &[Exemplar]) -> f32 {
    let mut weighted_similarity = 0.0_f64;
    let mut total_weight = 0.0_f64;
    for exemplar in exemplars {
        let weight = f64::from(exemplar.weight.max(0.0));
        let similarity =
            normalized_correlation(&exemplars[medoid_index].samples, &exemplar.samples);
        weighted_similarity += f64::from(similarity) * weight;
        total_weight += weight;
    }
    // Include alignment-to-reference as a deterministic fallback diagnostic
    // for the one-exemplar case without changing the selected waveform.
    if total_weight <= f64::EPSILON {
        exemplars[medoid_index].reference_correlation
    } else {
        (weighted_similarity / total_weight) as f32
    }
}

fn best_alignment(
    source: MonoWindow<'_>,
    coarse_onset: i64,
    onset_offset: usize,
    template: &[f32],
    radius: usize,
) -> (i64, f32) {
    let radius = radius.min(i64::MAX as usize) as i64;
    let mut best_shift = 0_i64;
    let mut best_correlation = f32::NEG_INFINITY;
    // Search a bounded coarse grid, then refine around its winner. At audio
    // sample rates an exhaustive radius-by-template scan is needlessly
    // expensive, while the second pass still returns sample-level timing.
    let coarse_step = (radius / 12).clamp(1, 32) as usize;
    for shift in (-radius..=radius).step_by(coarse_step) {
        let window_start = coarse_onset + shift - onset_offset as i64;
        let correlation = correlation_with_source(source, window_start, onset_offset, template);
        let better = correlation > best_correlation + 1.0e-7
            || ((correlation - best_correlation).abs() <= 1.0e-7
                && (shift.abs(), shift) < (best_shift.abs(), best_shift));
        if better {
            best_shift = shift;
            best_correlation = correlation;
        }
    }
    let refine_radius = (coarse_step as i64).min(4);
    let refine_start = (best_shift - refine_radius).max(-radius);
    let refine_end = (best_shift + refine_radius).min(radius);
    for shift in refine_start..=refine_end {
        let window_start = coarse_onset + shift - onset_offset as i64;
        let correlation = correlation_with_source(source, window_start, onset_offset, template);
        let better = correlation > best_correlation + 1.0e-7
            || ((correlation - best_correlation).abs() <= 1.0e-7
                && (shift.abs(), shift) < (best_shift.abs(), best_shift));
        if better {
            best_shift = shift;
            best_correlation = correlation;
        }
    }
    (best_shift, finite_or(best_correlation, 0.0))
}

fn correlation_with_source(
    source: MonoWindow<'_>,
    start: i64,
    onset_offset: usize,
    template: &[f32],
) -> f32 {
    let mut dot = 0.0_f64;
    let mut source_energy = 0.0_f64;
    let mut template_energy = 0.0_f64;
    // Timing comes from the attack, not a 240 ms tail containing unrelated
    // voices and reverberation. Preserve the full excerpt for rendering but
    // bound alignment evidence to a small pre-onset and ~9 ms post-onset
    // region at 44.1 kHz. Tiny test/low-rate templates still use every sample.
    let (focus_start, focus_end) = if template.len() <= 2_048 {
        (0, template.len())
    } else {
        (
            onset_offset.saturating_sub(32),
            (onset_offset + 384).min(template.len()),
        )
    };
    for offset in focus_start..focus_end {
        let template_sample = template[offset];
        let source_sample = source.sample(start + offset as i64);
        let template_sample = finite_or(template_sample, 0.0);
        dot += f64::from(source_sample) * f64::from(template_sample);
        source_energy += f64::from(source_sample) * f64::from(source_sample);
        template_energy += f64::from(template_sample) * f64::from(template_sample);
    }
    normalized_dot(dot, source_energy, template_energy)
}

fn normalized_correlation(left: &[f32], right: &[f32]) -> f32 {
    let mut dot = 0.0_f64;
    let mut left_energy = 0.0_f64;
    let mut right_energy = 0.0_f64;
    for (&left, &right) in left.iter().zip(right) {
        let left = finite_or(left, 0.0);
        let right = finite_or(right, 0.0);
        dot += f64::from(left) * f64::from(right);
        left_energy += f64::from(left) * f64::from(left);
        right_energy += f64::from(right) * f64::from(right);
    }
    normalized_dot(dot, left_energy, right_energy)
}

fn normalized_dot(dot: f64, left_energy: f64, right_energy: f64) -> f32 {
    let denominator = (left_energy * right_energy).sqrt();
    if denominator <= f64::EPSILON {
        if left_energy <= f64::EPSILON && right_energy <= f64::EPSILON {
            1.0
        } else {
            0.0
        }
    } else {
        (dot / denominator).clamp(-1.0, 1.0) as f32
    }
}

fn least_squares_gain(template: &[f32], occurrence: &[f32]) -> f32 {
    let mut dot = 0.0_f64;
    let mut template_energy = 0.0_f64;
    for (&template, &occurrence) in template.iter().zip(occurrence) {
        let template = finite_or(template, 0.0);
        let occurrence = finite_or(occurrence, 0.0);
        dot += f64::from(template) * f64::from(occurrence);
        template_energy += f64::from(template) * f64::from(template);
    }
    if template_energy <= f64::EPSILON {
        0.0
    } else {
        finite_or((dot / template_energy) as f32, 0.0)
    }
}

fn extract_window(source: MonoWindow<'_>, start: i64, len: usize) -> Vec<f32> {
    (0..len)
        .map(|offset| source.sample(start + offset as i64))
        .collect()
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

fn measure_fit(source: &[f32], rendered: &[f32], start_sample: usize) -> FitMetrics {
    let mut source_energy = 0.0_f64;
    let mut rendered_energy = 0.0_f64;
    let mut residual_energy = 0.0_f64;
    let mut dot = 0.0_f64;
    for (&source, &rendered) in source.iter().zip(rendered) {
        let source = f64::from(finite_or(source, 0.0));
        let rendered = f64::from(finite_or(rendered, 0.0));
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
    FitMetrics {
        start_sample,
        sample_count: source.len().min(rendered.len()),
        source_energy,
        rendered_energy,
        residual_energy,
        normalized_error,
        explained_energy: 1.0 - normalized_error,
        correlation: normalized_dot(dot, source_energy, rendered_energy),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untuned_loom_settings_are_the_builds_own_window_and_template() {
        let settings = LoomLensSettings::default();
        assert_eq!(settings.lookbehind_seconds(), 60);
        assert_eq!(settings.template_milliseconds(), 240);
        assert_eq!(
            settings.build_config(48_000),
            TemplateBuildConfig::for_sample_rate(48_000)
        );
    }

    #[test]
    fn the_knobs_step_inside_their_tables_and_say_when_they_stop() {
        let mut settings = LoomLensSettings::default();
        assert!(settings.step_lookbehind(-1));
        assert_eq!(settings.lookbehind_seconds(), 30);
        assert!(!settings.step_lookbehind(-1), "30 s is the shortest offer");
        assert!(settings.step_lookbehind(1));
        assert!(settings.step_lookbehind(1));
        assert_eq!(settings.lookbehind_seconds(), 120);
        assert!(!settings.step_lookbehind(1));

        assert!(settings.step_template_length(1));
        assert_eq!(settings.template_milliseconds(), 500);
        assert!(settings.step_template_length(1));
        assert_eq!(settings.template_milliseconds(), 1_000);
        assert!(!settings.step_template_length(1));
        assert!(
            settings.step_template_length(-4),
            "a signed count is honoured"
        );
        assert_eq!(settings.template_milliseconds(), 120);
    }

    #[test]
    fn a_longer_template_is_a_longer_template_and_nothing_else() {
        let default = LoomLensSettings::default().build_config(44_100);
        let mut longer = LoomLensSettings::default();
        longer.step_template_length(1);
        longer.step_template_length(1);
        let longer = longer.build_config(44_100);
        assert_eq!(longer.post_roll_samples, 44_100);
        assert_eq!(longer.pre_roll_samples, default.pre_roll_samples);
        assert_eq!(
            longer.alignment_radius_samples,
            default.alignment_radius_samples
        );
        assert_eq!(
            longer.max_exemplars_per_cluster,
            default.max_exemplars_per_cluster
        );
        assert_eq!(
            longer.template_len(),
            default.pre_roll_samples + 44_100,
            "the template is the pre-roll plus the chosen post-roll"
        );
    }

    #[test]
    fn indices_from_another_build_fall_back_to_the_defaults() {
        let wild = LoomLensSettings {
            lookbehind: 9,
            template_length: 42,
        }
        .normalized();
        assert_eq!(wild, LoomLensSettings::default());
    }

    fn add_wave(destination: &mut [f32], onset: usize, waveform: &[f32], gain: f32) {
        for (offset, &sample) in waveform.iter().enumerate() {
            if let Some(destination) = destination.get_mut(onset + offset) {
                *destination += sample * gain;
            }
        }
    }

    fn pulse_a() -> Vec<f32> {
        (0..36)
            .map(|index| {
                let time = index as f32;
                (time * 0.43).sin() * (-time / 11.0).exp()
            })
            .collect()
    }

    fn pulse_b() -> Vec<f32> {
        (0..28)
            .map(|index| {
                let time = index as f32;
                ((time * 0.91).cos() - 0.4 * (time * 0.21).sin()) * (-time / 8.0).exp()
            })
            .collect()
    }

    fn two_cluster_fixture() -> (Vec<f32>, Vec<EventObservation>, Vec<usize>) {
        let mut source = vec![0.0; 1_500];
        let a = pulse_a();
        let b = pulse_b();
        let actual = vec![100, 270, 455, 690, 880, 1_120];
        let clusters = [4, 9, 4, 9, 4, 9];
        let gains = [1.0, 0.75, 1.35, 1.1, 0.6, 1.4];
        let coarse_offsets = [-2_i64, 3, 1, -3, 2, -1];
        for ((&onset, &cluster), &gain) in actual.iter().zip(&clusters).zip(&gains) {
            add_wave(&mut source, onset, if cluster == 4 { &a } else { &b }, gain);
        }
        let observations = actual
            .iter()
            .zip(&clusters)
            .zip(&coarse_offsets)
            .enumerate()
            .map(
                |(index, ((&onset, &cluster_id), &offset))| EventObservation {
                    sample_index: (onset as i64 - offset) as usize,
                    cluster_id,
                    salience: 1.0 - index as f32 * 0.04,
                    template_similarity: 0.9,
                },
            )
            .collect();
        (source, observations, actual)
    }

    fn fixture_config() -> TemplateBuildConfig {
        TemplateBuildConfig {
            pre_roll_samples: 6,
            post_roll_samples: 48,
            alignment_radius_samples: 8,
            max_exemplars_per_cluster: 8,
        }
    }

    /// A window that retains every observation's template neighbourhood gives
    /// the same sketch, bit for bit, as the whole file: the bound is on what
    /// is read, not on what is computed.
    #[test]
    fn a_covering_window_infers_the_same_sketch_as_the_whole_file() {
        let (source, observations, _) = two_cluster_fixture();
        let config = fixture_config();
        let padding = config.template_len() + config.alignment_radius_samples;
        let first = observations
            .iter()
            .map(|observation| observation.sample_index)
            .min()
            .unwrap();
        let last = observations
            .iter()
            .map(|observation| observation.sample_index)
            .max()
            .unwrap();
        let start = first.saturating_sub(padding);
        let end = (last + padding).min(source.len());
        let window = MonoWindow::new(start, &source[start..end], source.len());
        assert!(window.covers(first.saturating_sub(padding), last + padding));
        assert!(window.retained().1 - window.retained().0 < source.len());

        let whole = SequenceSketch::infer(&source, 1_000, &observations, config).unwrap();
        let windowed = SequenceSketch::infer_windowed(
            window,
            1_000,
            &observations,
            config,
            &LoomCancellation::default(),
        )
        .unwrap();
        assert_eq!(whole, windowed);
        assert_eq!(
            whole.fit_span(&source, start, end - start),
            windowed.fit_span_windowed(window, start, end - start)
        );
    }

    #[test]
    fn recovers_two_recurring_waveforms_and_renders_them() {
        let (source, observations, actual) = two_cluster_fixture();
        let sketch = SequenceSketch::infer(&source, 1_000, &observations, fixture_config())
            .expect("infer sketch");

        assert_eq!(sketch.clusters.len(), 2);
        assert_eq!(sketch.events.len(), actual.len());
        for (event, _actual_onset) in sketch.events.iter().zip(actual) {
            assert!(event.timing_adjustment.abs() <= 8);
            assert!(event.template_correlation > 0.999);
        }

        let rendered = sketch.render_span(0, source.len());
        let metrics = measure_fit(&source, &rendered, 0);
        assert!(metrics.normalized_error < 1.0e-10, "{metrics:?}");
        assert!(metrics.correlation > 0.99999);
        assert!(metrics.residual_energy < metrics.source_energy);
    }

    #[test]
    fn cluster_and_event_edits_change_selected_span_output() {
        let (source, observations, _) = two_cluster_fixture();
        let mut sketch =
            SequenceSketch::infer(&source, 1_000, &observations, fixture_config()).unwrap();
        let original = sketch.render_span(0, source.len());

        assert!(sketch.set_cluster_enabled(9, false));
        let muted = sketch.render_span(0, source.len());
        assert_ne!(muted, original);
        assert!(muted[270..310].iter().all(|sample| sample.abs() < 1.0e-7));

        assert!(sketch.set_cluster_enabled(9, true));
        assert!(sketch.set_cluster_gain(9, 0.5));
        assert!(sketch.set_event_gain(1, 2.0));
        let gain_edited = sketch.render_span(0, source.len());
        for index in 270..298 {
            assert!((gain_edited[index] - original[index]).abs() < 1.0e-6);
        }

        assert!(sketch.move_event(1, 320));
        let moved = sketch.render_span(250, 120);
        assert!(moved[..60].iter().all(|sample| sample.abs() < 1.0e-7));
        assert!(moved[70..].iter().any(|sample| sample.abs() > 0.01));

        assert!(sketch.set_event_enabled(1, false));
        let disabled = sketch.render_span(250, 120);
        assert!(disabled.iter().all(|sample| sample.abs() < 1.0e-7));
    }

    #[test]
    fn inferred_residual_improves_over_silence() {
        let (source, observations, _) = two_cluster_fixture();
        let sketch =
            SequenceSketch::infer(&source, 1_000, &observations, fixture_config()).unwrap();
        let fit = sketch.fit_span(&source, 80, 1_100);
        let silence_error: f64 = source[80..1_180]
            .iter()
            .map(|sample| f64::from(*sample) * f64::from(*sample))
            .sum();
        assert!(fit.residual_energy < silence_error * 0.001, "{fit:?}");
        assert!(fit.explained_energy > 0.999);
    }

    #[test]
    fn medoid_keeps_a_real_phase_coherent_exemplar_and_rejects_an_outlier() {
        let mut source = vec![0.0; 900];
        let recurring = pulse_a();
        let outlier = pulse_b();
        for onset in [100, 300, 500] {
            add_wave(&mut source, onset, &recurring, 1.0);
        }
        add_wave(&mut source, 700, &outlier, 1.0);
        let observations: Vec<_> = [100, 300, 500, 700]
            .into_iter()
            .enumerate()
            .map(|(id, sample_index)| EventObservation {
                sample_index,
                cluster_id: 12,
                salience: 1.0 - id as f32 * 0.05,
                template_similarity: 0.9,
            })
            .collect();

        let sketch =
            SequenceSketch::infer(&source, 1_000, &observations, fixture_config()).unwrap();
        let template = &sketch.clusters[0].template;
        assert_ne!(template.medoid_event_id, 3);
        // A medoid is an actual occurrence, not a phase-smearing average.
        let medoid = &sketch.events[template.medoid_event_id as usize];
        let expected = extract_window(
            MonoWindow::whole(&source),
            medoid.sample_index - template.onset_offset as i64,
            template.samples.len(),
        );
        assert_eq!(template.samples, expected);
    }

    #[test]
    fn deterministic_and_degenerate_inputs_are_finite() {
        let (source, observations, _) = two_cluster_fixture();
        let first = SequenceSketch::infer(&source, 1_000, &observations, fixture_config()).unwrap();
        let second =
            SequenceSketch::infer(&source, 1_000, &observations, fixture_config()).unwrap();
        assert_eq!(first, second);

        let empty = SequenceSketch::infer(&[], 48_000, &[], fixture_config()).unwrap();
        assert!(empty.clusters.is_empty());
        assert!(empty.events.is_empty());
        assert_eq!(empty.render_span(100, 4), vec![0.0; 4]);
        assert_eq!(empty.fit_span(&[], 0, 4).normalized_error, 0.0);

        let silent_observation = [EventObservation {
            sample_index: 200,
            cluster_id: 3,
            salience: f32::NAN,
            template_similarity: f32::INFINITY,
        }];
        let silent =
            SequenceSketch::infer(&[0.0; 16], 48_000, &silent_observation, fixture_config())
                .unwrap();
        assert_eq!(silent.events[0].gain, 0.0);
        assert!(silent
            .render_span(0, 16)
            .iter()
            .all(|sample| sample.is_finite()));

        assert_eq!(
            SequenceSketch::infer(&source, 0, &observations, fixture_config()),
            Err(LoomError::InvalidSampleRate)
        );
    }

    #[test]
    fn selected_span_clips_events_at_both_edges() {
        let (source, observations, _) = two_cluster_fixture();
        let sketch =
            SequenceSketch::infer(&source, 1_000, &observations, fixture_config()).unwrap();
        let complete = sketch.render_span(0, source.len());
        assert_eq!(sketch.render_span(95, 20), complete[95..115]);
        assert_eq!(sketch.residual_span(&source, 95, 20), vec![0.0; 20]);
    }

    #[test]
    fn cancelled_inference_refuses_before_extracting_templates() {
        let cancellation = LoomCancellation::default();
        cancellation.cancel();
        let (source, observations, _) = two_cluster_fixture();
        assert_eq!(
            SequenceSketch::infer_cancellable(
                &source,
                1_000,
                &observations,
                fixture_config(),
                &cancellation,
            ),
            Err(LoomError::Cancelled)
        );
    }
}
