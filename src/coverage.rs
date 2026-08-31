//! Time-frequency reconstruction coverage and excess-energy fields.
//!
//! Coverage answers only how much measured signal energy a construction
//! accounts for. It is not confidence, correctness, causal identity, musical
//! similarity, or approval. Every consumer must retain residual audition and
//! the separate excess channel; a high explained value alone is not a verdict.

use std::fmt;

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

use crate::comparison::RenderedComparison;
use crate::daw_render::RenderCancellation;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoverageRecipe {
    pub fft_size: usize,
    pub hop_size: usize,
    /// Power-domain denominator floor. Silent-cell explained values must be
    /// interpreted alongside `source_power`, never as useful coverage.
    pub power_floor: f32,
}

impl Default for CoverageRecipe {
    fn default() -> Self {
        Self {
            fft_size: 2_048,
            hop_size: 512,
            power_floor: 1.0e-12,
        }
    }
}

impl CoverageRecipe {
    pub fn validate(self) -> Result<Self, CoverageError> {
        if self.fft_size < 2 || !self.fft_size.is_power_of_two() {
            return Err(CoverageError::InvalidRecipe(
                "fft_size must be a power of two of at least 2",
            ));
        }
        if self.hop_size == 0 || self.hop_size > self.fft_size {
            return Err(CoverageError::InvalidRecipe(
                "hop_size must be in 1..=fft_size",
            ));
        }
        if !self.power_floor.is_finite() || self.power_floor <= 0.0 {
            return Err(CoverageError::InvalidRecipe(
                "power_floor must be finite and positive",
            ));
        }
        Ok(self)
    }
}

/// Energy-weighted whole-field navigation summary. Names deliberately avoid
/// quality/correctness language.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CoverageSummary {
    pub source_power: f64,
    pub construction_power: f64,
    pub residual_power: f64,
    pub signed_explained_energy: f64,
    pub clamped_explained_energy: f64,
    pub excess_energy_ratio: f64,
}

/// Channel-major, then time-column-major, then low-to-high frequency bin.
#[derive(Clone, Debug, PartialEq)]
pub struct CoverageField {
    pub origin_frame: i64,
    pub sample_rate: u32,
    pub channels: u16,
    pub frame_count: u64,
    pub recipe: CoverageRecipe,
    pub columns: usize,
    pub bins: usize,
    pub source_power: Vec<f32>,
    pub construction_power: Vec<f32>,
    pub residual_power: Vec<f32>,
    /// `clamp(1 - residual/source, 0, 1)`.
    pub explained: Vec<f32>,
    /// `max(0, construction-source)/source`.
    pub excess: Vec<f32>,
    pub summary: CoverageSummary,
}

impl CoverageField {
    pub fn cell_index(&self, channel: usize, column: usize, bin: usize) -> Option<usize> {
        (channel < usize::from(self.channels) && column < self.columns && bin < self.bins)
            .then(|| (channel * self.columns + column) * self.bins + bin)
    }

    pub fn frequency_hz(&self, bin: usize) -> Option<f32> {
        (bin < self.bins)
            .then(|| bin as f32 * self.sample_rate as f32 / self.recipe.fft_size as f32)
    }

    pub fn frame_span_for_column(&self, column: usize) -> Option<(i64, i64)> {
        if column >= self.columns {
            return None;
        }
        let start = self
            .origin_frame
            .saturating_add((column * self.recipe.hop_size) as i64);
        let end = start
            .saturating_add(self.recipe.fft_size as i64)
            .min(self.origin_frame.saturating_add(self.frame_count as i64));
        Some((start, end.max(start)))
    }
}

pub fn compute_coverage(
    comparison: &RenderedComparison,
    recipe: CoverageRecipe,
    cancellation: &RenderCancellation,
) -> Result<CoverageField, CoverageError> {
    let recipe = recipe.validate()?;
    if cancellation.is_cancelled() {
        return Err(CoverageError::Cancelled);
    }
    let format = comparison.source.format();
    if comparison.construction.format() != format
        || comparison.residual.format() != format
        || comparison.source.frame_count() != comparison.construction.frame_count()
        || comparison.source.frame_count() != comparison.residual.frame_count()
    {
        return Err(CoverageError::UnalignedComparison);
    }
    let channels = usize::from(format.channels.get());
    let frames = comparison.source.frame_count().0 as usize;
    let columns = if frames == 0 {
        0
    } else {
        frames.div_ceil(recipe.hop_size)
    };
    let bins = recipe.fft_size / 2 + 1;
    let cells = channels
        .checked_mul(columns)
        .and_then(|count| count.checked_mul(bins))
        .ok_or(CoverageError::FieldTooLarge)?;
    let mut source_power = vec![0.0_f32; cells];
    let mut construction_power = vec![0.0_f32; cells];
    let mut residual_power = vec![0.0_f32; cells];

    let window = hann(recipe.fft_size);
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(recipe.fft_size);
    let mut scratch = vec![Complex::default(); recipe.fft_size];

    for channel in 0..channels {
        for column in 0..columns {
            if cancellation.is_cancelled() {
                return Err(CoverageError::Cancelled);
            }
            let start = column * recipe.hop_size;
            analyze_column(
                comparison.source.interleaved(),
                channels,
                channel,
                start,
                &window,
                &fft,
                &mut scratch,
                &mut source_power
                    [(channel * columns + column) * bins..(channel * columns + column + 1) * bins],
            );
            analyze_column(
                comparison.construction.interleaved(),
                channels,
                channel,
                start,
                &window,
                &fft,
                &mut scratch,
                &mut construction_power
                    [(channel * columns + column) * bins..(channel * columns + column + 1) * bins],
            );
            analyze_column(
                comparison.residual.interleaved(),
                channels,
                channel,
                start,
                &window,
                &fft,
                &mut scratch,
                &mut residual_power
                    [(channel * columns + column) * bins..(channel * columns + column + 1) * bins],
            );
        }
    }

    let mut explained = Vec::with_capacity(cells);
    let mut excess = Vec::with_capacity(cells);
    let floor = recipe.power_floor;
    for ((&source, &construction), &residual) in source_power
        .iter()
        .zip(&construction_power)
        .zip(&residual_power)
    {
        let denominator = source.max(floor);
        explained.push((1.0 - residual / denominator).clamp(0.0, 1.0));
        excess.push(((construction - source).max(0.0) / denominator).max(0.0));
    }

    let source_sum: f64 = source_power.iter().map(|value| f64::from(*value)).sum();
    let construction_sum: f64 = construction_power
        .iter()
        .map(|value| f64::from(*value))
        .sum();
    let residual_sum: f64 = residual_power.iter().map(|value| f64::from(*value)).sum();
    let denominator = source_sum.max(f64::from(recipe.power_floor));
    let signed = 1.0 - residual_sum / denominator;
    let summary = CoverageSummary {
        source_power: source_sum,
        construction_power: construction_sum,
        residual_power: residual_sum,
        signed_explained_energy: signed,
        clamped_explained_energy: signed.clamp(0.0, 1.0),
        excess_energy_ratio: (construction_sum - source_sum).max(0.0) / denominator,
    };

    Ok(CoverageField {
        origin_frame: comparison.origin_frame,
        sample_rate: format.sample_rate.get(),
        channels: format.channels.get(),
        frame_count: comparison.source.frame_count().0,
        recipe,
        columns,
        bins,
        source_power,
        construction_power,
        residual_power,
        explained,
        excess,
        summary,
    })
}

#[allow(clippy::too_many_arguments)]
fn analyze_column(
    interleaved: &[f32],
    channels: usize,
    channel: usize,
    start: usize,
    window: &[f32],
    fft: &std::sync::Arc<dyn rustfft::Fft<f32>>,
    scratch: &mut [Complex<f32>],
    output_power: &mut [f32],
) {
    let frames = interleaved.len() / channels;
    for (offset, value) in scratch.iter_mut().enumerate() {
        let frame = start + offset;
        let sample = if frame < frames {
            let sample = interleaved[frame * channels + channel];
            if sample.is_finite() {
                sample
            } else {
                0.0
            }
        } else {
            0.0
        };
        *value = Complex::new(sample * window[offset], 0.0);
    }
    fft.process(scratch);
    for (destination, bin) in output_power.iter_mut().zip(scratch.iter()) {
        *destination = bin.norm_sqr();
    }
}

fn hann(size: usize) -> Vec<f32> {
    (0..size)
        .map(|index| {
            let phase = std::f64::consts::TAU * (index as f64 + 0.5) / size as f64;
            (0.5 - 0.5 * phase.cos()) as f32
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoverageError {
    InvalidRecipe(&'static str),
    UnalignedComparison,
    FieldTooLarge,
    Cancelled,
}

impl fmt::Display for CoverageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRecipe(message) => write!(formatter, "invalid coverage recipe: {message}"),
            Self::UnalignedComparison => {
                formatter.write_str("comparison signals are not exactly aligned")
            }
            Self::FieldTooLarge => formatter.write_str("coverage field is too large"),
            Self::Cancelled => formatter.write_str("coverage computation cancelled"),
        }
    }
}

impl std::error::Error for CoverageError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{AudioFormat, ProjectAudio};
    use crate::comparison::render_comparison;
    use crate::explanation::RenderedExplanation;

    fn comparison(construction_gain: f32) -> RenderedComparison {
        let format = AudioFormat::new(8_000, 1).unwrap();
        let source_samples = (0..32)
            .map(|index| if index % 2 == 0 { 1.0 } else { -1.0 })
            .collect::<Vec<_>>();
        let construction_samples = source_samples
            .iter()
            .map(|sample| sample * construction_gain)
            .collect::<Vec<_>>();
        render_comparison(
            0,
            ProjectAudio::from_interleaved(format, source_samples).unwrap(),
            RenderedExplanation {
                origin_frame: 0,
                audio: ProjectAudio::from_interleaved(format, construction_samples).unwrap(),
            },
        )
        .unwrap()
    }

    #[test]
    fn exact_construction_has_full_explained_field_and_no_excess() {
        let field = compute_coverage(
            &comparison(1.0),
            CoverageRecipe {
                fft_size: 8,
                hop_size: 4,
                power_floor: 1.0e-12,
            },
            &RenderCancellation::new(),
        )
        .unwrap();
        assert!(field.explained.iter().all(|value| *value == 1.0));
        assert!(field.excess.iter().all(|value| *value == 0.0));
        assert_eq!(field.summary.clamped_explained_energy, 1.0);
    }

    #[test]
    fn over_gained_construction_lights_excess_instead_of_hiding_it() {
        let field = compute_coverage(
            &comparison(2.0),
            CoverageRecipe {
                fft_size: 8,
                hop_size: 4,
                power_floor: 1.0e-12,
            },
            &RenderCancellation::new(),
        )
        .unwrap();
        assert!(field.excess.iter().any(|value| *value > 0.0));
        assert!(field.summary.excess_energy_ratio > 0.0);
        assert_eq!(field.summary.clamped_explained_energy, 0.0);
    }
}
