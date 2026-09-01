//! Musician-facing contract for turning an exact source span into samples.
//!
//! The constructive controller already owns the atomic project mutation. This
//! module gives that mutation the missing product vocabulary: where the span
//! came from, what the musician expects to make, what it should be called,
//! where it should land, and which durable objects may be auditioned or
//! revealed afterward. It contains no GPUI state and never invents a target.

use std::error::Error;
use std::fmt;

use crate::arrangement::{ClipId, TrackId};
use crate::daw_project::DawProject;
use crate::mixer::BusId;
use crate::sample_actions::{
    MakeBeatIntent, MakeBeatResultFocus, SampleAuditionIntent, SampleChopIntent,
    SampleKitDestination, SamplePublishedResult, SampleResultFocus, SampleSelection,
    SamplerViewDisposition,
};
use crate::sample_kit::{KitId, PadId, SampleKitLibrary, SampleTargetRef};
use crate::sample_material::{SampleMaterialProvenance, SourceMaterialRef};
use crate::sequencer::PatternId;

const MAX_PRODUCT_NAME_CHARS: usize = 160;

/// Why this exact half-open source span is being offered to the workflow.
/// Selection and loop have identical frame semantics, but retaining the origin
/// lets the completion say "from loop" without pretending it was a new file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleSpanOrigin {
    Selection,
    Loop,
}

impl SampleSpanOrigin {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Selection => "selection",
            Self::Loop => "loop",
        }
    }
}

/// The explicit instrument destination shown before committing the workflow.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SampleInstrumentDestination {
    New { name: String },
    Existing { kit: KitId, expected_revision: u64 },
}

impl SampleInstrumentDestination {
    pub const fn kit_destination(&self) -> SampleKitDestination {
        match self {
            Self::New { .. } => SampleKitDestination::NewKit,
            Self::Existing {
                kit,
                expected_revision,
            } => SampleKitDestination::ExistingKit {
                kit: *kit,
                expected_revision: *expected_revision,
            },
        }
    }
}

/// The durable outcome chosen in the sample-creation sheet.
#[derive(Clone, Debug, PartialEq)]
pub enum SampleWorkflowProduct {
    OneSample {
        name: String,
    },
    SliceToKit {
        sample_name: String,
        chop: SampleChopIntent,
    },
    MakeBeat {
        sample_name: String,
        pattern_name: String,
        chop: SampleChopIntent,
        bars: u16,
        quantize_ticks: u64,
    },
}

impl SampleWorkflowProduct {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::OneSample { .. } => "Make sample",
            Self::SliceToKit { .. } => "Slice to kit",
            Self::MakeBeat { .. } => "Make beat",
        }
    }

    pub fn chop(&self) -> SampleChopIntent {
        match self {
            Self::OneSample { .. } => SampleChopIntent::OneShot,
            Self::SliceToKit { chop, .. } | Self::MakeBeat { chop, .. } => chop.clone(),
        }
    }

    pub const fn makes_pattern(&self) -> bool {
        matches!(self, Self::MakeBeat { .. })
    }

    pub fn sample_name(&self, index: usize, count: usize) -> String {
        let base = match self {
            Self::OneSample { name }
            | Self::SliceToKit {
                sample_name: name, ..
            }
            | Self::MakeBeat {
                sample_name: name, ..
            } => name,
        };
        if count <= 1 {
            base.clone()
        } else {
            format!("{base} {:02}", index + 1)
        }
    }

    pub fn pattern_name(&self) -> Option<&str> {
        match self {
            Self::MakeBeat { pattern_name, .. } => Some(pattern_name),
            Self::OneSample { .. } | Self::SliceToKit { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleWorkflowAfter {
    OpenInstrument,
    OpenPattern,
    OpenArrangement,
    Stay,
}

impl SampleWorkflowAfter {
    pub const fn make_beat_focus(self) -> MakeBeatResultFocus {
        match self {
            Self::OpenInstrument => {
                MakeBeatResultFocus::Sampler(SamplerViewDisposition::RetargetCurrent)
            }
            Self::OpenPattern => MakeBeatResultFocus::PatternEditor,
            Self::OpenArrangement => MakeBeatResultFocus::Arrangement,
            Self::Stay => MakeBeatResultFocus::Stay,
        }
    }
}

/// Complete, inspectable input to the cohesive sample workflow.
#[derive(Clone, Debug, PartialEq)]
pub struct SampleWorkflowSpec {
    pub span_origin: SampleSpanOrigin,
    pub product: SampleWorkflowProduct,
    pub destination: SampleInstrumentDestination,
    pub target_bus: Option<BusId>,
    pub after: SampleWorkflowAfter,
}

impl SampleWorkflowSpec {
    pub fn validate(&self) -> Result<(), SampleWorkflowValidationError> {
        match &self.destination {
            SampleInstrumentDestination::New { name } => validate_name("instrument", name)?,
            SampleInstrumentDestination::Existing { kit, .. } if kit.get() == 0 => {
                return Err(SampleWorkflowValidationError::ZeroInstrument)
            }
            SampleInstrumentDestination::Existing { .. } => {}
        }
        if self.target_bus.is_some_and(|bus| bus.get() == 0) {
            return Err(SampleWorkflowValidationError::ZeroBus);
        }
        match &self.product {
            SampleWorkflowProduct::OneSample { name } => validate_name("sample", name)?,
            SampleWorkflowProduct::SliceToKit { sample_name, chop } => {
                validate_name("sample", sample_name)?;
                if matches!(chop, SampleChopIntent::OneShot) {
                    return Err(SampleWorkflowValidationError::SliceRequiresMultipleZones);
                }
                validate_chop(chop)?;
            }
            SampleWorkflowProduct::MakeBeat {
                sample_name,
                pattern_name,
                chop,
                bars,
                quantize_ticks,
            } => {
                validate_name("sample", sample_name)?;
                validate_name("pattern", pattern_name)?;
                validate_chop(chop)?;
                if *bars == 0 {
                    return Err(SampleWorkflowValidationError::ZeroBars);
                }
                if *quantize_ticks == 0 {
                    return Err(SampleWorkflowValidationError::ZeroQuantize);
                }
            }
        }
        if !self.product.makes_pattern()
            && matches!(
                self.after,
                SampleWorkflowAfter::OpenPattern | SampleWorkflowAfter::OpenArrangement
            )
        {
            return Err(SampleWorkflowValidationError::LandingNeedsPattern);
        }
        Ok(())
    }

    pub fn plan_intent(
        &self,
        source: SampleSelection,
    ) -> Result<SampleWorkflowPlanIntent, SampleWorkflowValidationError> {
        self.validate()?;
        let kit = self.destination.kit_destination();
        Ok(match &self.product {
            SampleWorkflowProduct::OneSample { .. } => SampleWorkflowPlanIntent::BuildInstrument {
                chop: SampleChopIntent::OneShot,
                kit,
                target_bus: self.target_bus,
            },
            SampleWorkflowProduct::SliceToKit { chop, .. } => {
                SampleWorkflowPlanIntent::BuildInstrument {
                    chop: chop.clone(),
                    kit,
                    target_bus: self.target_bus,
                }
            }
            SampleWorkflowProduct::MakeBeat {
                chop,
                bars,
                quantize_ticks,
                ..
            } => SampleWorkflowPlanIntent::MakeBeat(MakeBeatIntent {
                source,
                chop: chop.clone(),
                kit,
                target_bus: self.target_bus,
                bars: *bars,
                quantize_ticks: *quantize_ticks,
                result_focus: self.after.make_beat_focus(),
            }),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SampleWorkflowPlanIntent {
    BuildInstrument {
        chop: SampleChopIntent,
        kit: SampleKitDestination,
        target_bus: Option<BusId>,
    },
    MakeBeat(MakeBeatIntent),
}

/// A sample-library row derived only from durable kit state. `target` makes
/// two zones over the same material independently addressable, while
/// `material` retains the exact source and range for provenance/reveal.
#[derive(Clone, Debug, PartialEq)]
pub struct NamedSampleAsset {
    pub target: SampleTargetRef,
    pub name: String,
    pub material: SourceMaterialRef,
    pub provenance: SampleMaterialProvenance,
    pub instrument_name: String,
    pub output_bus: BusId,
}

impl NamedSampleAsset {
    pub fn audition(&self, velocity: f32, pressed: bool) -> SampleAuditionIntent {
        SampleAuditionIntent::PadGate {
            kit: self.target.kit,
            pad: self.target.pad,
            velocity,
            pressed,
        }
    }
}

/// Deterministic library projection. Names are authored pad names; a pad with
/// layered zones receives explicit zone suffixes rather than silently merging
/// distinct material references.
pub fn named_sample_library(library: &SampleKitLibrary) -> Vec<NamedSampleAsset> {
    let mut samples = Vec::new();
    for kit in library.kits.values() {
        for pad in kit.ordered_pads() {
            let zone_count = pad.zone_order.len();
            for (index, zone) in kit.ordered_zones(pad.id).enumerate() {
                let name = if zone_count <= 1 {
                    pad.name.clone()
                } else {
                    format!("{} · zone {}", pad.name, index + 1)
                };
                samples.push(NamedSampleAsset {
                    target: SampleTargetRef {
                        kit: kit.id,
                        pad: pad.id,
                        zone: zone.id,
                    },
                    name,
                    material: zone.material,
                    provenance: zone.provenance.clone(),
                    instrument_name: kit.name.clone(),
                    output_bus: kit.output.bus,
                });
            }
        }
    }
    samples
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SampleWorkflowLanding {
    Instrument {
        kit: KitId,
        selected_pad: Option<PadId>,
        highlighted_pads: Vec<PadId>,
    },
    Pattern {
        pattern: PatternId,
        kit: KitId,
        pads: Vec<PadId>,
    },
    Arrangement {
        clip: ClipId,
        track: Option<TrackId>,
        pattern: Option<PatternId>,
        kit: KitId,
    },
    Stay {
        kit: KitId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SampleWorkflowNextAction {
    AuditionPad { kit: KitId, pad: PadId },
    RevealSource { material: SourceMaterialRef },
    OpenInstrument { kit: KitId },
    OpenPattern { pattern: PatternId },
    OpenArrangement { clip: ClipId },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SampleWorkflowPresentation {
    pub headline: String,
    pub detail: String,
    pub breadcrumb: Vec<String>,
    pub next_actions: Vec<SampleWorkflowNextAction>,
}

/// One completion object with all names and durable destinations needed by a
/// status strip, Explorer, Inspector, and audition adapter.
#[derive(Clone, Debug, PartialEq)]
pub struct SampleWorkflowReceipt {
    pub span_origin: SampleSpanOrigin,
    pub source: SampleSelection,
    pub publication: SamplePublishedResult,
    pub instrument_name: String,
    pub samples: Vec<NamedSampleAsset>,
    pub pattern_name: Option<String>,
    pub landing: SampleWorkflowLanding,
}

impl SampleWorkflowReceipt {
    pub fn from_project(
        spec: &SampleWorkflowSpec,
        source: SampleSelection,
        publication: SamplePublishedResult,
        project: &DawProject,
    ) -> Result<Self, SampleWorkflowValidationError> {
        spec.validate()?;
        let state = project.state();
        let kit = state.domains.sample_kits.kits.get(&publication.kit).ok_or(
            SampleWorkflowValidationError::MissingPublishedInstrument(publication.kit),
        )?;
        let library = named_sample_library(&state.domains.sample_kits);
        let samples = publication
            .created_zones
            .iter()
            .map(|target| {
                library
                    .iter()
                    .find(|sample| sample.target == *target)
                    .cloned()
                    .ok_or(SampleWorkflowValidationError::MissingPublishedSample(
                        *target,
                    ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if samples.is_empty() {
            return Err(SampleWorkflowValidationError::NoPublishedSamples);
        }
        let pattern_name = publication
            .pattern
            .map(|pattern| {
                state
                    .domains
                    .sequencer
                    .patterns()
                    .get(pattern)
                    .map(|pattern| pattern.name.clone())
                    .ok_or(SampleWorkflowValidationError::MissingPublishedPattern(
                        pattern,
                    ))
            })
            .transpose()?;
        if spec.product.makes_pattern() && pattern_name.is_none() {
            return Err(SampleWorkflowValidationError::PatternWasNotPublished);
        }
        let landing = landing_from_publication(&publication)?;
        Ok(Self {
            span_origin: spec.span_origin,
            source,
            publication,
            instrument_name: kit.name.clone(),
            samples,
            pattern_name,
            landing,
        })
    }

    pub fn primary_audition(&self, velocity: f32, pressed: bool) -> Option<SampleAuditionIntent> {
        self.samples
            .first()
            .map(|sample| sample.audition(velocity, pressed))
    }

    pub fn presentation(&self) -> SampleWorkflowPresentation {
        let mut next_actions = Vec::new();
        if let Some(first) = self.samples.first() {
            next_actions.push(SampleWorkflowNextAction::AuditionPad {
                kit: first.target.kit,
                pad: first.target.pad,
            });
            next_actions.push(SampleWorkflowNextAction::RevealSource {
                material: first.material,
            });
        }
        next_actions.push(SampleWorkflowNextAction::OpenInstrument {
            kit: self.publication.kit,
        });
        if let Some(pattern) = self.publication.pattern {
            next_actions.push(SampleWorkflowNextAction::OpenPattern { pattern });
        }
        if let Some(clip) = self.publication.arrangement_clip {
            next_actions.push(SampleWorkflowNextAction::OpenArrangement { clip });
        }

        let (destination, breadcrumb) = match &self.landing {
            SampleWorkflowLanding::Instrument { .. } => (
                format!("Instrument › {}", self.instrument_name),
                vec![
                    "Project".into(),
                    "Instruments".into(),
                    self.instrument_name.clone(),
                ],
            ),
            SampleWorkflowLanding::Pattern { .. } => {
                let name = self.pattern_name.as_deref().unwrap_or("Pattern");
                (
                    format!("Pattern › {name}"),
                    vec!["Project".into(), "Patterns".into(), name.into()],
                )
            }
            SampleWorkflowLanding::Arrangement { .. } => (
                "Arrange › new pattern occurrence".into(),
                vec!["Song".into(), "Arrange".into()],
            ),
            SampleWorkflowLanding::Stay { .. } => (
                format!("Created in Instrument › {}", self.instrument_name),
                vec![
                    "Project".into(),
                    "Instruments".into(),
                    self.instrument_name.clone(),
                ],
            ),
        };
        let range = self.source.source_range.map_or_else(
            || "the full material".into(),
            |range| format!("frames {}..{}", range.start.0, range.end.0),
        );
        SampleWorkflowPresentation {
            headline: destination,
            detail: format!(
                "{} named sample{} from {} {} · route {}",
                self.samples.len(),
                if self.samples.len() == 1 { "" } else { "s" },
                self.span_origin.label(),
                range,
                self.publication
                    .output_bus
                    .map_or_else(|| "unchanged".into(), |bus| bus.get().to_string())
            ),
            breadcrumb,
            next_actions,
        }
    }
}

fn landing_from_publication(
    publication: &SamplePublishedResult,
) -> Result<SampleWorkflowLanding, SampleWorkflowValidationError> {
    Ok(match publication.focus {
        SampleResultFocus::Stay => SampleWorkflowLanding::Stay {
            kit: publication.kit,
        },
        SampleResultFocus::Kit(kit)
        | SampleResultFocus::Sampler {
            target: crate::sample_actions::SamplerTarget::Kit(kit),
            ..
        } => SampleWorkflowLanding::Instrument {
            kit,
            selected_pad: publication.pad,
            highlighted_pads: publication.created_pads.clone(),
        },
        SampleResultFocus::Pad { kit, pad }
        | SampleResultFocus::Sampler {
            target: crate::sample_actions::SamplerTarget::Pad { kit, pad },
            ..
        } => SampleWorkflowLanding::Instrument {
            kit,
            selected_pad: Some(pad),
            highlighted_pads: publication.created_pads.clone(),
        },
        SampleResultFocus::Pattern(pattern) => SampleWorkflowLanding::Pattern {
            pattern,
            kit: publication.kit,
            pads: publication.created_pads.clone(),
        },
        SampleResultFocus::Arrangement {
            arrangement_clip,
            pattern,
            ..
        } => SampleWorkflowLanding::Arrangement {
            clip: arrangement_clip,
            track: publication.arrangement_track,
            pattern,
            kit: publication.kit,
        },
        SampleResultFocus::Sampler {
            target:
                crate::sample_actions::SamplerTarget::NewKit
                | crate::sample_actions::SamplerTarget::NewPad { .. },
            ..
        } => return Err(SampleWorkflowValidationError::UnresolvedLanding),
    })
}

fn validate_name(subject: &'static str, value: &str) -> Result<(), SampleWorkflowValidationError> {
    let count = value.chars().count();
    if value.trim().is_empty() {
        return Err(SampleWorkflowValidationError::EmptyName(subject));
    }
    if count > MAX_PRODUCT_NAME_CHARS {
        return Err(SampleWorkflowValidationError::NameTooLong {
            subject,
            characters: count,
        });
    }
    Ok(())
}

fn validate_chop(chop: &SampleChopIntent) -> Result<(), SampleWorkflowValidationError> {
    match chop {
        SampleChopIntent::OneShot => Ok(()),
        SampleChopIntent::EqualSlices { count } if *count == 0 => {
            Err(SampleWorkflowValidationError::ZeroSlices)
        }
        SampleChopIntent::EqualSlices { .. } => Ok(()),
        SampleChopIntent::DetectOnsets {
            analyzer,
            sensitivity,
            minimum_gap_frames,
        } => {
            if analyzer.trim().is_empty() {
                return Err(SampleWorkflowValidationError::EmptyAnalyzer);
            }
            if !sensitivity.is_finite() || !(0.0..=1.0).contains(sensitivity) {
                return Err(SampleWorkflowValidationError::InvalidSensitivity);
            }
            if *minimum_gap_frames == 0 {
                return Err(SampleWorkflowValidationError::ZeroOnsetGap);
            }
            Ok(())
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SampleWorkflowValidationError {
    EmptyName(&'static str),
    NameTooLong {
        subject: &'static str,
        characters: usize,
    },
    ZeroInstrument,
    ZeroBus,
    ZeroSlices,
    SliceRequiresMultipleZones,
    EmptyAnalyzer,
    InvalidSensitivity,
    ZeroOnsetGap,
    ZeroBars,
    ZeroQuantize,
    LandingNeedsPattern,
    MissingPublishedInstrument(KitId),
    MissingPublishedSample(SampleTargetRef),
    NoPublishedSamples,
    MissingPublishedPattern(PatternId),
    PatternWasNotPublished,
    UnresolvedLanding,
}

impl fmt::Display for SampleWorkflowValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid sample workflow: {self:?}")
    }
}

impl Error for SampleWorkflowValidationError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{AssetFrameRange, AssetId, SampleFrames};
    use crate::sample_kit::{SampleKit, SamplePad, SampleRouteIntent, SampleZone, ZoneId};
    use crate::sample_material::VirtualSliceRef;

    fn spec(product: SampleWorkflowProduct) -> SampleWorkflowSpec {
        SampleWorkflowSpec {
            span_origin: SampleSpanOrigin::Loop,
            product,
            destination: SampleInstrumentDestination::New {
                name: "Loop drums".into(),
            },
            target_bus: Some(BusId::from_raw(2)),
            after: SampleWorkflowAfter::OpenInstrument,
        }
    }

    #[test]
    fn product_contract_rejects_hidden_or_impossible_destinations() {
        let mut invalid = spec(SampleWorkflowProduct::SliceToKit {
            sample_name: "Loop chop".into(),
            chop: SampleChopIntent::OneShot,
        });
        assert_eq!(
            invalid.validate(),
            Err(SampleWorkflowValidationError::SliceRequiresMultipleZones)
        );
        invalid.product = SampleWorkflowProduct::OneSample {
            name: "Loop hit".into(),
        };
        invalid.after = SampleWorkflowAfter::OpenPattern;
        assert_eq!(
            invalid.validate(),
            Err(SampleWorkflowValidationError::LandingNeedsPattern)
        );
    }

    #[test]
    fn named_sample_library_retains_exact_material_and_audition_target() {
        let kit_id = KitId::from_raw(1);
        let pad_id = PadId::from_raw(2);
        let zone_id = ZoneId::from_raw(3);
        let range = AssetFrameRange::new(SampleFrames(12), SampleFrames(48)).unwrap();
        let material =
            SourceMaterialRef::VirtualSlice(VirtualSliceRef::new(AssetId(7), range).unwrap());
        let mut kit = SampleKit::new(
            kit_id,
            "Loop drums",
            SampleRouteIntent::new(BusId::from_raw(4)).unwrap(),
        );
        let mut pad = SamplePad::new(pad_id, "Snare bright");
        pad.zone_order.push(zone_id);
        kit.pad_order.push(pad_id);
        kit.pads.insert(pad_id, pad);
        kit.zones
            .insert(zone_id, SampleZone::new(zone_id, pad_id, material));
        let mut library = SampleKitLibrary::new();
        library
            .apply_puts(&[crate::sample_kit::SampleKitPut {
                before: None,
                after: Some(kit),
            }])
            .unwrap();

        let samples = named_sample_library(&library);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].name, "Snare bright");
        assert_eq!(samples[0].material, material);
        assert_eq!(
            samples[0].audition(0.8, true),
            SampleAuditionIntent::PadGate {
                kit: kit_id,
                pad: pad_id,
                velocity: 0.8,
                pressed: true,
            }
        );
    }

    #[test]
    fn slice_names_are_stable_and_human_readable() {
        let product = SampleWorkflowProduct::SliceToKit {
            sample_name: "Amen chop".into(),
            chop: SampleChopIntent::EqualSlices { count: 8 },
        };
        assert_eq!(product.sample_name(0, 8), "Amen chop 01");
        assert_eq!(product.sample_name(7, 8), "Amen chop 08");
        assert_eq!(product.sample_name(0, 1), "Amen chop");
    }
}
