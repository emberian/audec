//! Portable query hits and coverage navigation targets.
//!
//! These DTOs let GPUI, a headless client, or a reading document navigate the
//! same evidence without importing renderer or ontology internals. Coverage
//! ranking is explicitly residual-energy navigation: it is not a correctness,
//! confidence, or source-identity score.

use serde::{Deserialize, Serialize};

use crate::aspect::{ConcreteAspect, SignalLayer};
use crate::comparison::ComparisonId;
use crate::coverage::CoverageField;
use crate::reading::{QualifiedEntityId, ReadingId};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "namespace", rename_all = "snake_case")]
pub enum EntityRefDto {
    Project { kind: String, local_id: u64 },
    Reading(QualifiedEntityId),
}

impl EntityRefDto {
    pub fn validate(&self) -> Result<(), NavigationError> {
        match self {
            Self::Project { kind, local_id } => {
                if *local_id == 0 || kind.trim().is_empty() {
                    return Err(NavigationError::InvalidEntity(self.clone()));
                }
            }
            Self::Reading(id) => id
                .validate()
                .map_err(|_| NavigationError::InvalidEntity(self.clone()))?,
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionDto {
    pub start_frame: i64,
    pub end_frame: i64,
    pub min_hz_bits: u32,
    pub max_hz_bits: u32,
    pub channels: u16,
}

impl RegionDto {
    pub fn min_hz(self) -> f32 {
        f32::from_bits(self.min_hz_bits)
    }

    pub fn max_hz(self) -> f32 {
        f32::from_bits(self.max_hz_bits)
    }

    pub fn validate(self) -> Result<(), NavigationError> {
        let min = self.min_hz();
        let max = self.max_hz();
        if self.start_frame >= self.end_frame
            || !min.is_finite()
            || !max.is_finite()
            || min < 0.0
            || min >= max
            || self.channels == 0
        {
            return Err(NavigationError::InvalidRegion(self));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SignalLayerDto {
    Source,
    Explanation { reference: EntityRefDto },
    Residual { reference: EntityRefDto },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AspectGeometryDto {
    pub regions: Vec<RegionDto>,
    #[serde(default)]
    pub objects: Vec<EntityRefDto>,
    pub signal: SignalLayerDto,
}

impl AspectGeometryDto {
    /// Project-local conversion used by current GPUI surfaces. Reading export
    /// must qualify object and explanation IDs with its own `ReadingId`.
    pub fn from_project(aspect: &ConcreteAspect) -> Self {
        let regions = aspect
            .regions
            .iter()
            .map(|region| RegionDto {
                start_frame: region.time.start,
                end_frame: region.time.end,
                min_hz_bits: region.band.min_hz.to_bits(),
                max_hz_bits: region.band.max_hz.to_bits(),
                channels: region.channels.0,
            })
            .collect();
        let objects = aspect
            .objects
            .iter()
            .map(|object| EntityRefDto::Project {
                kind: "air-object".into(),
                local_id: object.get(),
            })
            .collect();
        let reference = |reference: crate::aspect::ExplanationRef| EntityRefDto::Project {
            kind: match reference {
                crate::aspect::ExplanationRef::Definition(_) => "explanation",
                crate::aspect::ExplanationRef::Proposal(_) => "reconstruction-proposal",
                crate::aspect::ExplanationRef::Comparison(_) => "comparison",
            }
            .into(),
            local_id: match reference {
                crate::aspect::ExplanationRef::Definition(id)
                | crate::aspect::ExplanationRef::Comparison(id) => id,
                crate::aspect::ExplanationRef::Proposal(id) => id.get(),
            },
        };
        let signal = match aspect.signal {
            SignalLayer::Source => SignalLayerDto::Source,
            SignalLayer::Explanation(value) => SignalLayerDto::Explanation {
                reference: reference(value),
            },
            SignalLayer::Residual(value) => SignalLayerDto::Residual {
                reference: reference(value),
            },
        };
        Self {
            regions,
            objects,
            signal,
        }
    }

    pub fn qualify_project_ids(mut self, reading: ReadingId) -> Result<Self, NavigationError> {
        fn qualify(
            reference: &mut EntityRefDto,
            reading: ReadingId,
        ) -> Result<(), NavigationError> {
            if let EntityRefDto::Project { kind, local_id } = reference {
                *reference = EntityRefDto::Reading(
                    QualifiedEntityId::new(reading, kind.clone(), *local_id)
                        .map_err(|_| NavigationError::InvalidEntity(reference.clone()))?,
                );
            }
            Ok(())
        }
        for object in &mut self.objects {
            qualify(object, reading)?;
        }
        match &mut self.signal {
            SignalLayerDto::Explanation { reference } | SignalLayerDto::Residual { reference } => {
                qualify(reference, reading)?
            }
            SignalLayerDto::Source => {}
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryDerivationDto {
    pub rule: String,
    #[serde(default)]
    pub premises: Vec<EntityRefDto>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryHitDto {
    pub fact: EntityRefDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extent: Option<AspectGeometryDto>,
    pub derivation: QueryDerivationDto,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryResultPageDto {
    pub query_term: String,
    pub result_revision: u64,
    pub hits: Vec<QueryHitDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoverageHotspotDto {
    pub comparison: EntityRefDto,
    pub region: RegionDto,
    pub source_power: f32,
    pub residual_power: f32,
    pub explained: f32,
    pub excess: f32,
}

/// Select deterministic high-residual cells for navigation. Silent cells are
/// skipped, and each returned row retains explained and excess side-by-side.
pub fn rank_coverage_hotspots(
    field: &CoverageField,
    comparison: ComparisonId,
    limit: usize,
) -> Vec<CoverageHotspotDto> {
    let mut candidates = Vec::new();
    let comparison = EntityRefDto::Project {
        kind: "comparison".into(),
        local_id: comparison.0,
    };
    for channel in 0..usize::from(field.channels).min(16) {
        for column in 0..field.columns {
            let Some((start_frame, end_frame)) = field.frame_span_for_column(column) else {
                continue;
            };
            for bin in 0..field.bins {
                let index = field
                    .cell_index(channel, column, bin)
                    .expect("loop indices are in range");
                let source_power = field.source_power[index];
                if !source_power.is_finite() || source_power <= field.recipe.power_floor {
                    continue;
                }
                let Some((min_hz, max_hz)) = hotspot_frequency_band(field, bin) else {
                    continue;
                };
                candidates.push((
                    field.residual_power[index],
                    channel,
                    column,
                    bin,
                    CoverageHotspotDto {
                        comparison: comparison.clone(),
                        region: RegionDto {
                            start_frame,
                            end_frame,
                            min_hz_bits: min_hz.to_bits(),
                            max_hz_bits: max_hz.to_bits(),
                            channels: 1_u16.checked_shl(channel as u32).unwrap_or(0),
                        },
                        source_power,
                        residual_power: field.residual_power[index],
                        explained: field.explained[index],
                        excess: field.excess[index],
                    },
                ));
            }
        }
    }
    candidates.sort_by(|left, right| {
        right
            .0
            .total_cmp(&left.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
            .then_with(|| left.3.cmp(&right.3))
    });
    candidates
        .into_iter()
        .take(limit)
        .map(|(_, _, _, _, hotspot)| hotspot)
        .collect()
}

/// Convert an FFT-bin center to honest half-bin geometry. In particular the
/// Nyquist bin extends down by half a bin instead of fabricating an upper
/// center beyond Nyquist or collapsing both bounds onto the same `f32`.
fn hotspot_frequency_band(field: &CoverageField, bin: usize) -> Option<(f32, f32)> {
    if bin >= field.bins || field.sample_rate == 0 || field.recipe.fft_size == 0 {
        return None;
    }
    let width = f64::from(field.sample_rate) / field.recipe.fft_size as f64;
    let center = bin as f64 * width;
    let nyquist = f64::from(field.sample_rate) * 0.5;
    let min_hz = if bin == 0 { 0.0 } else { center - width * 0.5 } as f32;
    let max_hz = (center + width * 0.5).min(nyquist) as f32;
    (min_hz.is_finite() && max_hz.is_finite() && min_hz >= 0.0 && min_hz < max_hz)
        .then_some((min_hz, max_hz))
}

#[derive(Clone, Debug, PartialEq)]
pub enum NavigationError {
    InvalidEntity(EntityRefDto),
    InvalidRegion(RegionDto),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coverage::{CoverageRecipe, CoverageSummary};

    #[test]
    fn hotspot_ranking_uses_residual_energy_and_deterministic_coordinates() {
        let field = CoverageField {
            origin_frame: 0,
            sample_rate: 48_000,
            channels: 1,
            frame_count: 8,
            recipe: CoverageRecipe {
                fft_size: 2,
                hop_size: 2,
                power_floor: 0.01,
            },
            columns: 2,
            bins: 2,
            source_power: vec![1.0, 1.0, 1.0, 1.0],
            construction_power: vec![0.0; 4],
            residual_power: vec![0.2, 0.9, 0.9, 0.1],
            explained: vec![0.8, 0.1, 0.1, 0.9],
            excess: vec![0.0, 0.0, 0.4, 0.0],
            summary: CoverageSummary::default(),
        };
        let ranked = rank_coverage_hotspots(&field, ComparisonId(7), 2);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].region.start_frame, 0);
        assert_eq!(ranked[1].region.start_frame, 2);
        assert_eq!(ranked[1].excess, 0.4);
    }
}
