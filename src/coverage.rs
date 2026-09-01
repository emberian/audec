//! Time-frequency reconstruction coverage and excess-energy fields.
//!
//! Coverage answers only how much measured signal energy a construction
//! accounts for. It is not confidence, correctness, causal identity, musical
//! similarity, or approval. Every consumer must retain residual audition and
//! the separate excess channel; a high explained value alone is not a verdict.

use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

use crate::aspect::{
    BandSpan, ChannelMask, ConcreteAspect, ConcreteRegion, ExplanationRef, FrameSpan, SignalLayer,
};
use crate::audio::{AudioFormat, ProjectAudio};
use crate::change_set::{BusImpact, ChangeSet};
use crate::comparison::{ComparisonId, RenderedComparison};
use crate::daw_render::RenderCancellation;
use crate::explanation::ExplanationId;
use crate::render_products::{RenderProduct, RenderProductId};
use crate::render_runtime::canonical_pcm_digest;

const MAX_RESOLVED_FFT_SIZE: usize = 131_072;
const MAX_TILE_COLUMNS: usize = 8_192;

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

/// Durable semantic identity carried by every coverage product. Render
/// revisions are deliberately not used as the identity of the experiment:
/// those live in the content-addressed signal pins below, while this pair
/// continues to name the same persistent comparison after refresh.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CoverageComparisonIdentity {
    pub comparison: ComparisonId,
    pub explanation: ExplanationId,
}

impl CoverageComparisonIdentity {
    pub fn new(
        comparison: ComparisonId,
        explanation: ExplanationId,
    ) -> Result<Self, CoverageError> {
        if comparison.0 == 0 || explanation.0 == 0 {
            return Err(CoverageError::ZeroComparisonIdentity);
        }
        Ok(Self {
            comparison,
            explanation,
        })
    }
}

/// The immutable shared render products from which coverage may be derived.
/// This type only admits aligned products. It neither renders an explanation
/// nor recomputes a residual, and thus cannot diverge from the comparison
/// runtime's source = construction + residual equation.
#[derive(Clone, Debug)]
pub struct CoverageProductInputs {
    pub identity: CoverageComparisonIdentity,
    pub span: FrameSpan,
    pub source: Arc<RenderProduct>,
    pub construction: Arc<RenderProduct>,
    pub residual: Arc<RenderProduct>,
}

impl CoverageProductInputs {
    pub fn new(
        identity: CoverageComparisonIdentity,
        source: Arc<RenderProduct>,
        construction: Arc<RenderProduct>,
        residual: Arc<RenderProduct>,
    ) -> Result<Self, CoverageError> {
        CoverageComparisonIdentity::new(identity.comparison, identity.explanation)?;
        let render_span = source.produced_by.core;
        if construction.produced_by.core != render_span
            || residual.produced_by.core != render_span
            || construction.produced_by.plan != source.produced_by.plan
            || residual.produced_by.plan != source.produced_by.plan
            || construction.id.format != source.id.format
            || residual.id.format != source.id.format
            || construction.id.frames != source.id.frames
            || residual.id.frames != source.id.frames
        {
            return Err(CoverageError::UnalignedRenderProducts);
        }
        let span = FrameSpan::new(render_span.start, render_span.end)
            .ok_or(CoverageError::UnalignedRenderProducts)?;
        Ok(Self {
            identity,
            span,
            source,
            construction,
            residual,
        })
    }

    pub fn pins(&self) -> CoverageProductPins {
        CoverageProductPins {
            source: self.source.id,
            construction: self.construction.id,
            residual: self.residual.id,
        }
    }

    fn rendered_comparison(&self) -> Result<RenderedComparison, CoverageError> {
        let format = AudioFormat::new(
            self.source.id.format.sample_rate.get(),
            self.source.id.format.channels.get(),
        )
        .map_err(|error| CoverageError::Audio(error.to_string()))?;
        let audio = |product: &Arc<RenderProduct>| {
            ProjectAudio::new(format, product.shared_interleaved())
                .map_err(|error| CoverageError::Audio(error.to_string()))
        };
        Ok(RenderedComparison {
            origin_frame: self.span.start,
            source: audio(&self.source)?,
            construction: audio(&self.construction)?,
            residual: audio(&self.residual)?,
            // Coverage consumes the three canonical signals. Sample-domain
            // metrics remain owned by comparison::render_comparison.
            metrics: Default::default(),
        })
    }

    fn sample_range(&self, span: FrameSpan) -> Result<std::ops::Range<usize>, CoverageError> {
        if span.start < self.span.start || span.end > self.span.end {
            return Err(CoverageError::SpanOutsideComparison {
                requested: span,
                available: self.span,
            });
        }
        let channels = usize::from(self.source.id.format.channels.get());
        let start = usize::try_from(span.start - self.span.start)
            .ok()
            .and_then(|frames| frames.checked_mul(channels))
            .ok_or(CoverageError::FieldTooLarge)?;
        let end = usize::try_from(span.end - self.span.start)
            .ok()
            .and_then(|frames| frames.checked_mul(channels))
            .ok_or(CoverageError::FieldTooLarge)?;
        Ok(start..end)
    }

    fn validate_residual_equation(&self, span: FrameSpan) -> Result<(), CoverageError> {
        let range = self.sample_range(span)?;
        let channels = usize::from(self.source.id.format.channels.get());
        for (offset, ((&source, &construction), &residual)) in self.source.interleaved()
            [range.clone()]
        .iter()
        .zip(&self.construction.interleaved()[range.clone()])
        .zip(&self.residual.interleaved()[range])
        .enumerate()
        {
            if (source - construction).to_bits() != residual.to_bits() {
                return Err(CoverageError::ResidualEquationMismatch {
                    frame: span.start + i64::try_from(offset / channels).unwrap_or(i64::MAX),
                    channel: (offset % channels) as u16,
                });
            }
        }
        Ok(())
    }

    fn slice_id(
        &self,
        product: &RenderProduct,
        span: FrameSpan,
    ) -> Result<RenderProductId, CoverageError> {
        let range = self.sample_range(span)?;
        Ok(RenderProductId {
            pcm: canonical_pcm_digest(&product.interleaved()[range]),
            format: product.id.format,
            frames: u64::try_from(span.end - span.start)
                .map_err(|_| CoverageError::FieldTooLarge)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CoverageProductPins {
    pub source: RenderProductId,
    pub construction: RenderProductId,
    pub residual: RenderProductId,
}

/// One viewport-driven coverage request. `target_columns` is physical display
/// width, not a bitmap scale request: resolving a different width changes the
/// FFT/hop key and produces new numeric evidence.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoverageTileRequest {
    pub frames: FrameSpan,
    pub target_columns: usize,
    pub recipe: CoverageRecipe,
}

/// Complete cache identity for one numeric time x frequency tile. Slice IDs
/// hash exactly the PCM support read by the FFT windows, so an edit elsewhere
/// in a larger shared render product does not evict an unaffected tile.
#[derive(Clone, Copy, Debug)]
pub struct CoverageTileKey {
    pub identity: CoverageComparisonIdentity,
    pub frames: FrameSpan,
    pub analysis_support: FrameSpan,
    pub target_columns: usize,
    pub recipe: CoverageRecipe,
    pub source_slice: RenderProductId,
    pub construction_slice: RenderProductId,
    pub residual_slice: RenderProductId,
}

impl PartialEq for CoverageTileKey {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
            && self.frames == other.frames
            && self.analysis_support == other.analysis_support
            && self.target_columns == other.target_columns
            && self.recipe.fft_size == other.recipe.fft_size
            && self.recipe.hop_size == other.recipe.hop_size
            && self.recipe.power_floor.to_bits() == other.recipe.power_floor.to_bits()
            && self.source_slice == other.source_slice
            && self.construction_slice == other.construction_slice
            && self.residual_slice == other.residual_slice
    }
}

impl Eq for CoverageTileKey {}

impl Hash for CoverageTileKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.identity.hash(state);
        self.frames.hash(state);
        self.analysis_support.hash(state);
        self.target_columns.hash(state);
        self.recipe.fft_size.hash(state);
        self.recipe.hop_size.hash(state);
        self.recipe.power_floor.to_bits().hash(state);
        self.source_slice.hash(state);
        self.construction_slice.hash(state);
        self.residual_slice.hash(state);
    }
}

impl CoverageTileKey {
    pub fn column_for_frame(self, frame: i64) -> Option<usize> {
        (frame >= self.frames.start && frame < self.frames.end).then(|| {
            usize::try_from(frame - self.frames.start)
                .unwrap_or(usize::MAX)
                .checked_div(self.recipe.hop_size.max(1))
                .unwrap_or(0)
                .min(self.column_count().saturating_sub(1))
        })
    }

    pub fn column_count(self) -> usize {
        usize::try_from(self.frames.end - self.frames.start)
            .unwrap_or(usize::MAX)
            .div_ceil(self.recipe.hop_size)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CoverageTilePlanner;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoverageViewportRequest {
    pub visible_frames: FrameSpan,
    pub target_columns: usize,
    pub tile_frames: u32,
    pub recipe: CoverageRecipe,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoverageTileSpec {
    pub index: i64,
    /// Stable grid-aligned analysis request, clipped only by the comparison
    /// extent. `visible_frames` may cover a smaller part of this tile.
    pub request: CoverageTileRequest,
    pub visible_frames: FrameSpan,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CoverageTileLayout {
    pub identity: CoverageComparisonIdentity,
    pub visible_frames: FrameSpan,
    pub tiles: Vec<CoverageTileSpec>,
}

impl CoverageTilePlanner {
    /// Partition a viewport onto a stable power-of-two project-frame grid.
    /// Panning at the same scale therefore preserves interior tile requests;
    /// the presentation layer crops `visible_frames` instead of asking for a
    /// newly scaled bitmap.
    pub fn plan_viewport(
        &self,
        inputs: &CoverageProductInputs,
        request: CoverageViewportRequest,
    ) -> Result<CoverageTileLayout, CoverageError> {
        if request.visible_frames.start >= request.visible_frames.end {
            return Err(CoverageError::InvalidSpan(request.visible_frames));
        }
        if !request.tile_frames.is_power_of_two() {
            return Err(CoverageError::InvalidTileFrames(request.tile_frames));
        }
        if request.visible_frames.start < inputs.span.start
            || request.visible_frames.end > inputs.span.end
        {
            return Err(CoverageError::SpanOutsideComparison {
                requested: request.visible_frames,
                available: inputs.span,
            });
        }
        request.recipe.validate()?;
        let tile_frames = i64::from(request.tile_frames);
        let first = request.visible_frames.start.div_euclid(tile_frames);
        let last = (request.visible_frames.end - 1).div_euclid(tile_frames);
        let count = usize::try_from(last - first + 1).map_err(|_| CoverageError::FieldTooLarge)?;
        let visible_len =
            usize::try_from(request.visible_frames.end - request.visible_frames.start)
                .map_err(|_| CoverageError::FieldTooLarge)?;
        let mut tiles = Vec::with_capacity(count);
        for index in first..=last {
            let grid_start = i128::from(index) * i128::from(tile_frames);
            let grid_end = grid_start + i128::from(tile_frames);
            if grid_start < i128::from(i64::MIN) || grid_end > i128::from(i64::MAX) {
                return Err(CoverageError::FieldTooLarge);
            }
            let frames = FrameSpan {
                start: (grid_start as i64).max(inputs.span.start),
                end: (grid_end as i64).min(inputs.span.end),
            };
            let visible_frames = FrameSpan {
                start: frames.start.max(request.visible_frames.start),
                end: frames.end.min(request.visible_frames.end),
            };
            let frames_len = usize::try_from(frames.end - frames.start)
                .map_err(|_| CoverageError::FieldTooLarge)?;
            let target_columns = frames_len
                .checked_mul(request.target_columns.max(1))
                .ok_or(CoverageError::FieldTooLarge)?
                .div_ceil(visible_len)
                .clamp(1, MAX_TILE_COLUMNS);
            tiles.push(CoverageTileSpec {
                index,
                request: CoverageTileRequest {
                    frames,
                    target_columns,
                    recipe: request.recipe,
                },
                visible_frames,
            });
        }
        Ok(CoverageTileLayout {
            identity: inputs.identity,
            visible_frames: request.visible_frames,
            tiles,
        })
    }

    pub fn resolve(
        &self,
        inputs: &CoverageProductInputs,
        request: CoverageTileRequest,
    ) -> Result<CoverageTileKey, CoverageError> {
        let requested = request.recipe.validate()?;
        if request.frames.start >= request.frames.end {
            return Err(CoverageError::InvalidSpan(request.frames));
        }
        if request.frames.start < inputs.span.start || request.frames.end > inputs.span.end {
            return Err(CoverageError::SpanOutsideComparison {
                requested: request.frames,
                available: inputs.span,
            });
        }
        let target_columns = request.target_columns.clamp(1, MAX_TILE_COLUMNS);
        let frame_count = usize::try_from(request.frames.end - request.frames.start)
            .map_err(|_| CoverageError::FieldTooLarge)?;
        let display_hop = frame_count.div_ceil(target_columns).max(1);
        let hop_size = requested
            .hop_size
            .max(display_hop)
            .min(MAX_RESOLVED_FFT_SIZE);
        let fft_size = requested
            .fft_size
            .max(hop_size.next_power_of_two())
            .min(MAX_RESOLVED_FFT_SIZE);
        let recipe = CoverageRecipe {
            fft_size,
            hop_size,
            power_floor: requested.power_floor,
        }
        .validate()?;
        let columns = frame_count.div_ceil(recipe.hop_size);
        let last_start = (columns.saturating_sub(1))
            .checked_mul(recipe.hop_size)
            .ok_or(CoverageError::FieldTooLarge)?;
        let support_end = request
            .frames
            .start
            .checked_add(
                i64::try_from(last_start.saturating_add(recipe.fft_size))
                    .map_err(|_| CoverageError::FieldTooLarge)?,
            )
            .ok_or(CoverageError::FieldTooLarge)?
            .min(inputs.span.end);
        let analysis_support = FrameSpan {
            start: request.frames.start,
            end: support_end,
        };
        inputs.validate_residual_equation(analysis_support)?;
        Ok(CoverageTileKey {
            identity: inputs.identity,
            frames: request.frames,
            analysis_support,
            target_columns,
            recipe,
            source_slice: inputs.slice_id(&inputs.source, analysis_support)?,
            construction_slice: inputs.slice_id(&inputs.construction, analysis_support)?,
            residual_slice: inputs.slice_id(&inputs.residual, analysis_support)?,
        })
    }
}

/// Why residual and excess cannot be stacked as parts of a whole. The exact
/// power identity is `R = S + C - 2 Re(S*conj(C))`; excess is only the
/// nonnegative surplus `max(C-S, 0)`, not a fourth signal or a partition.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CoverageAccountingDiagnostics {
    pub phase_cross_term_power: f64,
    pub phase_cross_term_ratio: f64,
    pub cells_with_residual_and_excess: u64,
    pub silent_source_cells_with_construction: u64,
    pub silent_source_construction_power: f64,
}

impl CoverageAccountingDiagnostics {
    pub const DISCLOSURE: &'static str = "explained, residual, and excess overlap and must not be stacked to 100%; excess is a spectral diagnostic with no PCM, while residual remains the exact audible null";

    fn from_field(field: &CoverageField) -> Self {
        let denominator = field
            .summary
            .source_power
            .max(f64::from(field.recipe.power_floor));
        let phase_cross_term_power = field.summary.source_power + field.summary.construction_power
            - field.summary.residual_power;
        let mut cells_with_residual_and_excess = 0_u64;
        let mut silent_source_cells_with_construction = 0_u64;
        let mut silent_source_construction_power = 0.0_f64;
        for ((&source, &construction), (&residual, &excess)) in field
            .source_power
            .iter()
            .zip(&field.construction_power)
            .zip(field.residual_power.iter().zip(&field.excess))
        {
            if residual > field.recipe.power_floor && excess > 0.0 {
                cells_with_residual_and_excess += 1;
            }
            if source <= field.recipe.power_floor && construction > field.recipe.power_floor {
                silent_source_cells_with_construction += 1;
                silent_source_construction_power += f64::from(construction);
            }
        }
        Self {
            phase_cross_term_power,
            phase_cross_term_ratio: phase_cross_term_power / denominator,
            cells_with_residual_and_excess,
            silent_source_cells_with_construction,
            silent_source_construction_power,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CoverageTile {
    pub key: CoverageTileKey,
    pub comparison_span: FrameSpan,
    /// Whole-product pins used by an audition adapter. They are intentionally
    /// not part of `key`: cache reuse is proven by exact analysis-slice PCM.
    pub products: CoverageProductPins,
    pub field: Arc<CoverageField>,
    pub accounting: CoverageAccountingDiagnostics,
}

impl CoverageTile {
    pub fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(
            5_usize
                .saturating_mul(self.field.source_power.len())
                .saturating_mul(std::mem::size_of::<f32>()),
        )
    }

    fn repin(&self, inputs: &CoverageProductInputs) -> Self {
        Self {
            key: self.key,
            comparison_span: inputs.span,
            products: inputs.pins(),
            field: Arc::clone(&self.field),
            accounting: self.accounting,
        }
    }

    pub fn interaction(
        &self,
        channel: usize,
        column: usize,
        bin: usize,
        layer: CoverageLayer,
    ) -> Result<CoverageInteractionPlan, CoverageError> {
        let index =
            self.field
                .cell_index(channel, column, bin)
                .ok_or(CoverageError::CellOutsideTile {
                    channel,
                    column,
                    bin,
                })?;
        let (start, end) = self
            .field
            .frame_span_for_column(column)
            .filter(|(start, end)| start < end)
            .ok_or(CoverageError::CellOutsideTile {
                channel,
                column,
                bin,
            })?;
        let band = coverage_bin_band(&self.field, bin).ok_or(CoverageError::CellOutsideTile {
            channel,
            column,
            bin,
        })?;
        let mask = 1_u16
            .checked_shl(channel as u32)
            .filter(|mask| *mask != 0)
            .ok_or(CoverageError::CellOutsideTile {
                channel,
                column,
                bin,
            })?;
        let signal = match layer {
            CoverageLayer::Explained => SignalLayer::Explanation(ExplanationRef::Definition(
                self.key.identity.explanation.0,
            )),
            CoverageLayer::Residual => {
                SignalLayer::Residual(ExplanationRef::Comparison(self.key.identity.comparison.0))
            }
            // Aspect has no invented Excess signal layer. Its geometry remains
            // source-addressed and the disclosure carries spectral semantics.
            CoverageLayer::Excess => SignalLayer::Source,
        };
        let focus_span = FrameSpan { start, end };
        let aspect = ConcreteAspect::new(
            vec![ConcreteRegion {
                time: focus_span,
                band,
                channels: ChannelMask(mask),
            }],
            signal,
        )
        .map_err(|error| CoverageError::Aspect(error.to_string()))?;
        let audition = |signal, product| CoverageAuditionPlan {
            comparison: self.key.identity.comparison,
            signal,
            product,
            comparison_span: self.comparison_span,
            focus_span,
        };
        let primary_audition = match layer {
            CoverageLayer::Explained => Some(audition(
                CoverageAuditionSignal::Construction,
                self.products.construction,
            )),
            CoverageLayer::Residual => Some(audition(
                CoverageAuditionSignal::Residual,
                self.products.residual,
            )),
            CoverageLayer::Excess => None,
        };
        Ok(CoverageInteractionPlan {
            cell: CoverageCellMeasurement {
                source_power: self.field.source_power[index],
                construction_power: self.field.construction_power[index],
                residual_power: self.field.residual_power[index],
                explained: self.field.explained[index],
                excess: self.field.excess[index],
            },
            layer,
            aspect: aspect.clone(),
            reveal: CoverageRevealPlan {
                comparison: self.key.identity.comparison,
                aspect,
            },
            primary_audition,
            residual_audition: audition(CoverageAuditionSignal::Residual, self.products.residual),
            disclosure: CoverageDisclosure {
                energy_is_not_correctness: true,
                channels_are_non_additive: true,
                excess_has_no_pcm: layer == CoverageLayer::Excess,
            },
        })
    }
}

pub fn compute_coverage_tile(
    inputs: &CoverageProductInputs,
    key: CoverageTileKey,
    cancellation: &RenderCancellation,
) -> Result<CoverageTile, CoverageError> {
    let expected = CoverageTilePlanner.resolve(
        inputs,
        CoverageTileRequest {
            frames: key.frames,
            target_columns: key.target_columns,
            recipe: key.recipe,
        },
    )?;
    if expected != key {
        return Err(CoverageError::TileInputIdentityMismatch);
    }
    let rendered = inputs.rendered_comparison()?;
    let field = compute_coverage_span(&rendered, key.frames, key.recipe, cancellation)?;
    let accounting = CoverageAccountingDiagnostics::from_field(&field);
    Ok(CoverageTile {
        key,
        comparison_span: inputs.span,
        products: inputs.pins(),
        field: Arc::new(field),
        accounting,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoverageLayer {
    Explained,
    Residual,
    Excess,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoverageCellMeasurement {
    pub source_power: f32,
    pub construction_power: f32,
    pub residual_power: f32,
    pub explained: f32,
    pub excess: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageRevealPlan {
    pub comparison: ComparisonId,
    pub aspect: ConcreteAspect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoverageAuditionSignal {
    Construction,
    Residual,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoverageAuditionPlan {
    pub comparison: ComparisonId,
    pub signal: CoverageAuditionSignal,
    pub product: RenderProductId,
    /// Extent of the currently pinned product. An adapter may loop or seek to
    /// `focus_span`, but must not pretend the time-frequency cell is PCM.
    pub comparison_span: FrameSpan,
    pub focus_span: FrameSpan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoverageDisclosure {
    pub energy_is_not_correctness: bool,
    pub channels_are_non_additive: bool,
    pub excess_has_no_pcm: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CoverageInteractionPlan {
    pub cell: CoverageCellMeasurement,
    pub layer: CoverageLayer,
    pub aspect: ConcreteAspect,
    pub reveal: CoverageRevealPlan,
    pub primary_audition: Option<CoverageAuditionPlan>,
    /// Present for every layer, including excess, so a coverage scalar never
    /// becomes a dead-end scoreboard disconnected from its audible null.
    pub residual_audition: CoverageAuditionPlan,
    pub disclosure: CoverageDisclosure,
}

fn coverage_bin_band(field: &CoverageField, bin: usize) -> Option<BandSpan> {
    if bin >= field.bins || field.recipe.fft_size == 0 || field.sample_rate == 0 {
        return None;
    }
    let width = f64::from(field.sample_rate) / field.recipe.fft_size as f64;
    let center = bin as f64 * width;
    let nyquist = f64::from(field.sample_rate) * 0.5;
    let min_hz = if bin == 0 { 0.0 } else { center - width * 0.5 } as f32;
    let max_hz = (center + width * 0.5).min(nyquist) as f32;
    BandSpan::new(min_hz, max_hz)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoverageInvalidationImpact {
    Clean,
    IntersectsAnalysisSupport,
    WholeSignal,
    /// A non-AIR domain changed without an audio-range receipt. Exact slice
    /// identity may still prove reuse once new products exist, but a scheduler
    /// cannot assume the tile is clean beforehand.
    UnboundedDomainChange,
}

pub fn coverage_invalidation_impact(
    key: CoverageTileKey,
    changes: &ChangeSet,
) -> CoverageInvalidationImpact {
    if changes.routing_changed
        || changes
            .audio
            .values()
            .any(|impact| matches!(impact, BusImpact::Whole))
    {
        return CoverageInvalidationImpact::WholeSignal;
    }
    let support = key.analysis_support;
    if changes.audio.values().any(|impact| match impact {
        BusImpact::Whole => true,
        BusImpact::Ranges(ranges) => ranges
            .iter()
            .any(|range| range.start < support.end && support.start < range.end),
    }) {
        return CoverageInvalidationImpact::IntersectsAnalysisSupport;
    }
    if changes.audio.is_empty()
        && changes
            .domains
            .iter()
            .any(|domain| *domain != crate::daw_project::ProjectDomain::Air)
    {
        return CoverageInvalidationImpact::UnboundedDomainChange;
    }
    CoverageInvalidationImpact::Clean
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoverageTileDisposition {
    ComputedCold,
    ComputedAfterReportedInvalidation,
    /// Exact product slices changed even though the supplied ChangeSet did not
    /// cover the analysis support. Computation is safe; the diagnostic exposes
    /// the invalidation-contract mismatch instead of silently trusting it.
    ComputedAfterUnreportedSignalChange,
    ReusedExactSliceIdentity,
    ReusedDespiteReportedInvalidation,
}

#[derive(Clone, Debug)]
pub struct CoverageTileResolution {
    pub tile: Arc<CoverageTile>,
    pub disposition: CoverageTileDisposition,
    pub invalidation: CoverageInvalidationImpact,
}

#[derive(Clone, Debug)]
pub struct CoverageViewportTile {
    pub spec: CoverageTileSpec,
    pub resolution: CoverageTileResolution,
}

#[derive(Clone, Debug)]
pub struct CoverageViewportProduct {
    pub layout: CoverageTileLayout,
    pub tiles: Vec<CoverageViewportTile>,
}

impl CoverageViewportProduct {
    pub fn tile(&self, index: i64) -> Option<&CoverageViewportTile> {
        self.tiles.iter().find(|tile| tile.spec.index == index)
    }

    pub fn computed_count(&self) -> usize {
        self.tiles
            .iter()
            .filter(|tile| {
                matches!(
                    tile.resolution.disposition,
                    CoverageTileDisposition::ComputedCold
                        | CoverageTileDisposition::ComputedAfterReportedInvalidation
                        | CoverageTileDisposition::ComputedAfterUnreportedSignalChange
                )
            })
            .count()
    }

    pub fn reused_count(&self) -> usize {
        self.tiles.len().saturating_sub(self.computed_count())
    }
}

/// Stateful but UI-agnostic presenter for an Explanation/Coverage pane. It
/// owns only numeric tile cache and selection state; rendering, navigation,
/// object reveal, and audio publication remain typed effects returned to the
/// host through [`CoverageInteractionPlan`].
pub struct CoverageWorkbenchPresenter {
    cache: CoverageTileCache,
    layer: CoverageLayer,
    current: Option<CoverageViewportProduct>,
}

impl CoverageWorkbenchPresenter {
    pub fn new(max_tiles: usize, max_bytes: usize) -> Self {
        Self {
            cache: CoverageTileCache::new(max_tiles, max_bytes),
            layer: CoverageLayer::Residual,
            current: None,
        }
    }

    pub fn layer(&self) -> CoverageLayer {
        self.layer
    }

    pub fn set_layer(&mut self, layer: CoverageLayer) {
        self.layer = layer;
    }

    pub fn current(&self) -> Option<&CoverageViewportProduct> {
        self.current.as_ref()
    }

    pub fn update(
        &mut self,
        inputs: &CoverageProductInputs,
        request: CoverageViewportRequest,
        changes: &ChangeSet,
        cancellation: &RenderCancellation,
    ) -> Result<&CoverageViewportProduct, CoverageError> {
        let next = self.cache.resolve_viewport(
            inputs,
            request,
            self.current.as_ref(),
            changes,
            cancellation,
        )?;
        self.current = Some(next);
        Ok(self.current.as_ref().expect("coverage viewport installed"))
    }

    pub fn click(
        &self,
        tile_index: i64,
        channel: usize,
        column: usize,
        bin: usize,
    ) -> Result<CoverageInteractionPlan, CoverageError> {
        let viewport = self
            .current
            .as_ref()
            .ok_or(CoverageError::NoPresentedCoverage)?;
        let tile = viewport
            .tile(tile_index)
            .ok_or(CoverageError::UnknownCoverageTile(tile_index))?;
        tile.resolution
            .tile
            .interaction(channel, column, bin, self.layer)
    }

    pub fn clear_comparison(&mut self, comparison: ComparisonId) {
        self.cache.evict_comparison(comparison);
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.layout.identity.comparison == comparison)
        {
            self.current = None;
        }
    }
}

struct CoverageCacheEntry {
    tile: Arc<CoverageTile>,
    bytes: usize,
    last_used: u64,
}

/// Bounded cache for numeric coverage products. Entries survive comparison
/// refreshes until eviction because their keys include persistent comparison
/// identity plus exact PCM slice identities. This permits both history and
/// provable reuse across render plans without pinning an obsolete plan ID.
pub struct CoverageTileCache {
    entries: HashMap<CoverageTileKey, CoverageCacheEntry>,
    max_entries: usize,
    max_bytes: usize,
    resident_bytes: usize,
    clock: u64,
}

impl CoverageTileCache {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries,
            max_bytes,
            resident_bytes: 0,
            clock: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    pub fn get(&mut self, key: &CoverageTileKey) -> Option<Arc<CoverageTile>> {
        self.clock = self.clock.wrapping_add(1);
        self.entries.get_mut(key).map(|entry| {
            entry.last_used = self.clock;
            Arc::clone(&entry.tile)
        })
    }

    pub fn resolve(
        &mut self,
        inputs: &CoverageProductInputs,
        request: CoverageTileRequest,
        previous: Option<&CoverageTile>,
        changes: &ChangeSet,
        cancellation: &RenderCancellation,
    ) -> Result<CoverageTileResolution, CoverageError> {
        let key = CoverageTilePlanner.resolve(inputs, request)?;
        let invalidation = coverage_invalidation_impact(key, changes);
        if let Some(tile) = self.get(&key).or_else(|| {
            previous
                .filter(|tile| tile.key == key)
                .map(|tile| Arc::new(tile.clone()))
        }) {
            let tile = Arc::new(tile.repin(inputs));
            self.insert(Arc::clone(&tile));
            let disposition = if invalidation == CoverageInvalidationImpact::Clean {
                CoverageTileDisposition::ReusedExactSliceIdentity
            } else {
                CoverageTileDisposition::ReusedDespiteReportedInvalidation
            };
            return Ok(CoverageTileResolution {
                tile,
                disposition,
                invalidation,
            });
        }

        let same_view = previous.is_some_and(|tile| {
            tile.key.identity == key.identity
                && tile.key.frames == key.frames
                && tile.key.target_columns == key.target_columns
                && tile.key.recipe == key.recipe
        });
        let disposition = match (previous, invalidation, same_view) {
            (None, _, _) => CoverageTileDisposition::ComputedCold,
            (
                Some(_),
                CoverageInvalidationImpact::IntersectsAnalysisSupport
                | CoverageInvalidationImpact::WholeSignal
                | CoverageInvalidationImpact::UnboundedDomainChange,
                _,
            ) => CoverageTileDisposition::ComputedAfterReportedInvalidation,
            (Some(_), CoverageInvalidationImpact::Clean, true) => {
                CoverageTileDisposition::ComputedAfterUnreportedSignalChange
            }
            (Some(_), CoverageInvalidationImpact::Clean, false) => {
                CoverageTileDisposition::ComputedCold
            }
        };
        let tile = Arc::new(compute_coverage_tile(inputs, key, cancellation)?);
        self.insert(Arc::clone(&tile));
        Ok(CoverageTileResolution {
            tile,
            disposition,
            invalidation,
        })
    }

    pub fn resolve_viewport(
        &mut self,
        inputs: &CoverageProductInputs,
        request: CoverageViewportRequest,
        previous: Option<&CoverageViewportProduct>,
        changes: &ChangeSet,
        cancellation: &RenderCancellation,
    ) -> Result<CoverageViewportProduct, CoverageError> {
        let layout = CoverageTilePlanner.plan_viewport(inputs, request)?;
        let mut tiles = Vec::with_capacity(layout.tiles.len());
        for spec in &layout.tiles {
            if cancellation.is_cancelled() {
                return Err(CoverageError::Cancelled);
            }
            let previous_tile = previous
                .and_then(|surface| surface.tile(spec.index))
                .map(|tile| tile.resolution.tile.as_ref());
            tiles.push(CoverageViewportTile {
                spec: *spec,
                resolution: self.resolve(
                    inputs,
                    spec.request,
                    previous_tile,
                    changes,
                    cancellation,
                )?,
            });
        }
        Ok(CoverageViewportProduct { layout, tiles })
    }

    pub fn evict_comparison(&mut self, comparison: ComparisonId) {
        let keys = self
            .entries
            .keys()
            .filter(|key| key.identity.comparison == comparison)
            .copied()
            .collect::<Vec<_>>();
        for key in keys {
            self.remove(&key);
        }
    }

    fn insert(&mut self, tile: Arc<CoverageTile>) {
        let bytes = tile.estimated_bytes();
        if self.max_entries == 0 || bytes > self.max_bytes {
            return;
        }
        self.clock = self.clock.wrapping_add(1);
        if let Some(previous) = self.entries.remove(&tile.key) {
            self.resident_bytes = self.resident_bytes.saturating_sub(previous.bytes);
        }
        self.resident_bytes = self.resident_bytes.saturating_add(bytes);
        self.entries.insert(
            tile.key,
            CoverageCacheEntry {
                tile,
                bytes,
                last_used: self.clock,
            },
        );
        while self.entries.len() > self.max_entries || self.resident_bytes > self.max_bytes {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            self.remove(&oldest);
        }
    }

    fn remove(&mut self, key: &CoverageTileKey) {
        if let Some(entry) = self.entries.remove(key) {
            self.resident_bytes = self.resident_bytes.saturating_sub(entry.bytes);
        }
    }
}

pub fn compute_coverage(
    comparison: &RenderedComparison,
    recipe: CoverageRecipe,
    cancellation: &RenderCancellation,
) -> Result<CoverageField, CoverageError> {
    let frame_count = i64::try_from(comparison.source.frame_count().0)
        .map_err(|_| CoverageError::FieldTooLarge)?;
    let end = comparison
        .origin_frame
        .checked_add(frame_count)
        .ok_or(CoverageError::FieldTooLarge)?;
    let span = FrameSpan::new(comparison.origin_frame, end).unwrap_or(FrameSpan {
        start: comparison.origin_frame,
        end: comparison.origin_frame.saturating_add(1),
    });
    if comparison.source.frame_count().0 == 0 {
        return compute_coverage_window(
            comparison,
            comparison.origin_frame,
            0,
            0,
            recipe,
            cancellation,
        );
    }
    compute_coverage_span(comparison, span, recipe, cancellation)
}

/// Compute the canonical coverage equation for one exact half-open project
/// span. FFT windows begin at each hop inside `span` and may read later frames
/// from the retained comparison products; reads beyond the comparison extent
/// are zero padded. This makes time tiling an execution partition of the same
/// analysis used by [`compute_coverage`], rather than a second DSP truth.
pub fn compute_coverage_span(
    comparison: &RenderedComparison,
    span: FrameSpan,
    recipe: CoverageRecipe,
    cancellation: &RenderCancellation,
) -> Result<CoverageField, CoverageError> {
    if span.start >= span.end {
        return Err(CoverageError::InvalidSpan(span));
    }
    let comparison_frames = i64::try_from(comparison.source.frame_count().0)
        .map_err(|_| CoverageError::FieldTooLarge)?;
    let comparison_end = comparison
        .origin_frame
        .checked_add(comparison_frames)
        .ok_or(CoverageError::FieldTooLarge)?;
    if span.start < comparison.origin_frame || span.end > comparison_end {
        return Err(CoverageError::SpanOutsideComparison {
            requested: span,
            available: FrameSpan {
                start: comparison.origin_frame,
                end: comparison_end,
            },
        });
    }
    let offset = usize::try_from(span.start - comparison.origin_frame)
        .map_err(|_| CoverageError::FieldTooLarge)?;
    let frame_count =
        usize::try_from(span.end - span.start).map_err(|_| CoverageError::FieldTooLarge)?;
    compute_coverage_window(
        comparison,
        span.start,
        offset,
        frame_count,
        recipe,
        cancellation,
    )
}

fn compute_coverage_window(
    comparison: &RenderedComparison,
    origin_frame: i64,
    source_offset: usize,
    frame_count: usize,
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
    let total_frames = usize::try_from(comparison.source.frame_count().0)
        .map_err(|_| CoverageError::FieldTooLarge)?;
    if source_offset > total_frames {
        return Err(CoverageError::FieldTooLarge);
    }
    let frames = frame_count;
    if frames > total_frames.saturating_sub(source_offset) {
        return Err(CoverageError::FieldTooLarge);
    }
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
            let start = source_offset
                .checked_add(column.saturating_mul(recipe.hop_size))
                .ok_or(CoverageError::FieldTooLarge)?;
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
    let excess_sum: f64 = construction_power
        .iter()
        .zip(&source_power)
        .map(|(&construction, &source)| f64::from((construction - source).max(0.0)))
        .sum();
    let denominator = source_sum.max(f64::from(recipe.power_floor));
    let signed = 1.0 - residual_sum / denominator;
    let summary = CoverageSummary {
        source_power: source_sum,
        construction_power: construction_sum,
        residual_power: residual_sum,
        signed_explained_energy: signed,
        clamped_explained_energy: signed.clamp(0.0, 1.0),
        // Sum the nonnegative cell surplus. Taking max only after summing
        // would let a deficit in one band hide over-construction in another.
        excess_energy_ratio: excess_sum / denominator,
    };

    Ok(CoverageField {
        origin_frame,
        sample_rate: format.sample_rate.get(),
        channels: format.channels.get(),
        frame_count: frame_count as u64,
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
        let power = bin.norm_sqr();
        *destination = if power.is_finite() { power } else { 0.0 };
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
    InvalidTileFrames(u32),
    InvalidSpan(FrameSpan),
    UnalignedComparison,
    ZeroComparisonIdentity,
    UnalignedRenderProducts,
    ResidualEquationMismatch {
        frame: i64,
        channel: u16,
    },
    SpanOutsideComparison {
        requested: FrameSpan,
        available: FrameSpan,
    },
    TileInputIdentityMismatch,
    NoPresentedCoverage,
    UnknownCoverageTile(i64),
    CellOutsideTile {
        channel: usize,
        column: usize,
        bin: usize,
    },
    Audio(String),
    Aspect(String),
    FieldTooLarge,
    Cancelled,
}

impl fmt::Display for CoverageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRecipe(message) => write!(formatter, "invalid coverage recipe: {message}"),
            Self::InvalidTileFrames(frames) => write!(
                formatter,
                "coverage tile frame count {frames} is not a power of two"
            ),
            Self::InvalidSpan(span) => write!(
                formatter,
                "coverage span {}..{} is empty or reversed",
                span.start, span.end
            ),
            Self::UnalignedComparison => {
                formatter.write_str("comparison signals are not exactly aligned")
            }
            Self::ZeroComparisonIdentity => {
                formatter.write_str("coverage comparison and explanation identities must be nonzero")
            }
            Self::UnalignedRenderProducts => formatter
                .write_str("coverage render products do not share one format and project span"),
            Self::ResidualEquationMismatch { frame, channel } => write!(
                formatter,
                "coverage residual at frame {frame}, channel {channel} is not exact source - construction"
            ),
            Self::SpanOutsideComparison {
                requested,
                available,
            } => write!(
                formatter,
                "coverage span {}..{} is outside comparison {}..{}",
                requested.start, requested.end, available.start, available.end
            ),
            Self::TileInputIdentityMismatch => formatter
                .write_str("coverage tile key does not match the supplied shared render products"),
            Self::NoPresentedCoverage => {
                formatter.write_str("no coverage viewport has been presented")
            }
            Self::UnknownCoverageTile(index) => {
                write!(formatter, "coverage tile {index} is not in the presented viewport")
            }
            Self::CellOutsideTile {
                channel,
                column,
                bin,
            } => write!(
                formatter,
                "coverage cell ({channel}, {column}, {bin}) is outside the tile"
            ),
            Self::Audio(message) => write!(formatter, "coverage audio error: {message}"),
            Self::Aspect(message) => write!(formatter, "coverage aspect error: {message}"),
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
        assert!((field.summary.excess_energy_ratio - 3.0).abs() < 1.0e-6);
        assert_eq!(field.summary.clamped_explained_energy, 0.0);
        let accounting = CoverageAccountingDiagnostics::from_field(&field);
        assert!(accounting.cells_with_residual_and_excess > 0);
        assert!((accounting.phase_cross_term_ratio - 4.0).abs() < 1.0e-6);
        assert!(CoverageAccountingDiagnostics::DISCLOSURE.contains("must not be stacked"));
    }

    #[test]
    fn span_analysis_is_an_exact_partition_of_the_whole_field() {
        let comparison = comparison(0.5);
        let recipe = CoverageRecipe {
            fft_size: 8,
            hop_size: 4,
            power_floor: 1.0e-12,
        };
        let whole = compute_coverage(&comparison, recipe, &RenderCancellation::new()).unwrap();
        let tile = compute_coverage_span(
            &comparison,
            FrameSpan { start: 8, end: 24 },
            recipe,
            &RenderCancellation::new(),
        )
        .unwrap();

        assert_eq!(tile.origin_frame, 8);
        assert_eq!(tile.frame_count, 16);
        assert_eq!(tile.columns, 4);
        for column in 0..tile.columns {
            for bin in 0..tile.bins {
                let tile_index = tile.cell_index(0, column, bin).unwrap();
                let whole_index = whole.cell_index(0, column + 2, bin).unwrap();
                assert_eq!(
                    tile.source_power[tile_index],
                    whole.source_power[whole_index]
                );
                assert_eq!(
                    tile.construction_power[tile_index],
                    whole.construction_power[whole_index]
                );
                assert_eq!(
                    tile.residual_power[tile_index],
                    whole.residual_power[whole_index]
                );
                assert_eq!(tile.explained[tile_index], whole.explained[whole_index]);
                assert_eq!(tile.excess[tile_index], whole.excess[whole_index]);
            }
        }
    }
}
