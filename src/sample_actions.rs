//! Controller-facing intents emitted by audec's sample browser and pad editor.
//!
//! These values are deliberately project-model agnostic. Views may select and
//! inspect authoritative [`SampleKit`](crate::sample_kit::SampleKit) snapshots,
//! but every audible or authored consequence crosses this typed callback seam.
//! A controller is responsible for validating revisions, allocating IDs,
//! constructing commands, and publishing constructive plans atomically.

use std::sync::Arc;

use crate::assets::{AssetFrameRange, AssetId, SampleFrames};
use crate::mixer::BusId;
use crate::sample_kit::{KitId, PadId, ZoneId};
use crate::sample_material::{
    SampleMaterialProvenance, ScopedEvidenceRef, SourceMaterialRef, VirtualSliceRef,
};
use crate::ui_drag::DropIntent;

/// The exact source material under the browser's playhead or range selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SampleSelection {
    pub asset: AssetId,
    pub source_range: Option<AssetFrameRange>,
}

impl SampleSelection {
    pub const fn whole_asset(asset: AssetId) -> Self {
        Self {
            asset,
            source_range: None,
        }
    }

    pub const fn material(self) -> SourceMaterialRef {
        match self.source_range {
            Some(source_range) => SourceMaterialRef::VirtualSlice(VirtualSliceRef {
                source_asset: self.asset,
                source_range,
            }),
            None => SourceMaterialRef::Asset(self.asset),
        }
    }
}

/// The audition engine receives semantic starts/stops rather than UI events.
/// A one-shot may naturally finish without a later stop; gate mode must stop
/// when its pointer/key is released or the view loses focus.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SampleAuditionIntent {
    MaterialOneShot {
        material: SourceMaterialRef,
        velocity: f32,
    },
    PadGate {
        kit: KitId,
        pad: PadId,
        velocity: f32,
        pressed: bool,
    },
}

/// How a selected span should become playable zones and initial beat events.
#[derive(Clone, Debug, PartialEq)]
pub enum SampleChopIntent {
    OneShot,
    EqualSlices {
        count: u16,
    },
    DetectOnsets {
        analyzer: String,
        sensitivity: f32,
        minimum_gap_frames: u64,
    },
}

impl SampleChopIntent {
    pub fn is_previewable(&self) -> bool {
        matches!(self, Self::DetectOnsets { .. })
    }
}

/// An ephemeral, controller-computed onset preview. It is not authored kit
/// state and must be requested again if its exact source selection changes.
#[derive(Clone, Debug, PartialEq)]
pub struct OnsetChopPreview {
    pub source: SampleSelection,
    pub analyzer: String,
    /// Sorted decoded-frame boundaries strictly inside the selected material.
    pub boundaries: Vec<SampleFrames>,
    pub confidence: Option<f32>,
    pub diagnostic: Option<String>,
}

impl OnsetChopPreview {
    pub fn is_for(self: &Self, selection: SampleSelection) -> bool {
        self.source == selection
    }

    pub fn is_valid(&self) -> bool {
        if self.analyzer.trim().is_empty()
            || self
                .confidence
                .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
            || self.boundaries.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return false;
        }
        match self.source.source_range {
            Some(range) => self
                .boundaries
                .iter()
                .all(|boundary| *boundary > range.start && *boundary < range.end),
            None => self.boundaries.iter().all(|boundary| boundary.0 > 0),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChopPreviewIntent {
    pub source: SampleSelection,
    pub chop: SampleChopIntent,
}

/// Stable semantic target for a dynamic sampler workspace item.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SamplerTarget {
    NewKit,
    Kit(KitId),
    NewPad { kit: KitId },
    Pad { kit: KitId, pad: PadId },
}

impl SamplerTarget {
    pub const fn kit(self) -> Option<KitId> {
        match self {
            Self::NewKit => None,
            Self::Kit(kit) | Self::NewPad { kit } | Self::Pad { kit, .. } => Some(kit),
        }
    }

    pub const fn pad(self) -> Option<PadId> {
        match self {
            Self::Pad { pad, .. } => Some(pad),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SamplerViewDisposition {
    RetargetCurrent,
    OpenNew,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SamplerWorkspaceIntent {
    pub target: SamplerTarget,
    pub disposition: SamplerViewDisposition,
}

impl Default for SampleChopIntent {
    fn default() -> Self {
        Self::EqualSlices { count: 8 }
    }
}

/// Where a constructive adapter should publish the pads it creates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleKitDestination {
    NewKit,
    ExistingKit { kit: KitId, expected_revision: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MakeBeatResultFocus {
    Stay,
    Sampler(SamplerViewDisposition),
    PatternEditor,
    Arrangement,
}

/// Complete user intent behind “Sample selection & make beat”.
///
/// Timing, onset evidence, ID allocation, material fingerprints, and the
/// resulting `ConstructiveEditPlan` remain controller/constructive concerns.
#[derive(Clone, Debug, PartialEq)]
pub struct MakeBeatIntent {
    pub source: SampleSelection,
    pub chop: SampleChopIntent,
    pub kit: SampleKitDestination,
    pub target_bus: Option<BusId>,
    pub bars: u16,
    pub quantize_ticks: u64,
    /// Applied only after the constructive edit publishes successfully.
    pub result_focus: MakeBeatResultFocus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZoneEditTarget {
    pub kit: KitId,
    pub pad: PadId,
    pub zone: ZoneId,
    pub expected_revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleLoopMode {
    Forward,
    PingPong,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SampleEnvelopeIntent {
    pub attack_frames: u64,
    pub decay_frames: u64,
    pub sustain: f32,
    pub release_frames: u64,
}

impl SampleEnvelopeIntent {
    pub const fn percussive() -> Self {
        Self {
            attack_frames: 64,
            decay_frames: 4_800,
            sustain: 0.0,
            release_frames: 1_200,
        }
    }

    pub fn is_valid(self) -> bool {
        self.sustain.is_finite() && (0.0..=1.0).contains(&self.sustain)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ZoneEditIntent {
    Trim {
        target: ZoneEditTarget,
        source_range: AssetFrameRange,
    },
    SetLoop {
        target: ZoneEditTarget,
        enabled: bool,
        source_range: Option<AssetFrameRange>,
        mode: SampleLoopMode,
    },
    SetEnvelope {
        target: ZoneEditTarget,
        envelope: SampleEnvelopeIntent,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SamplerDiagnosticSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SamplerDiagnostic {
    pub severity: SamplerDiagnosticSeverity,
    pub code: String,
    pub message: String,
    pub target: Option<SamplerTarget>,
}

/// A stable target for provenance/evidence disclosure in an inspector pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SampleInspectTarget {
    Material(SourceMaterialRef),
    Zone { kit: KitId, zone: ZoneId },
    Provenance(SampleMaterialProvenance),
    Evidence(ScopedEvidenceRef),
}

/// All semantic output from sampler-facing views.
#[derive(Clone, Debug, PartialEq)]
pub enum SampleAction {
    Audition(SampleAuditionIntent),
    /// A drag/drop interpretation retaining its exact source range.
    ApplyDrop(DropIntent),
    SetKitOutput {
        kit: KitId,
        bus: BusId,
        expected_revision: u64,
    },
    RemoveZone {
        kit: KitId,
        pad: PadId,
        zone: ZoneId,
        expected_revision: u64,
    },
    Inspect(SampleInspectTarget),
    PreviewChop(ChopPreviewIntent),
    EditZone(ZoneEditIntent),
    Workspace(SamplerWorkspaceIntent),
    MakeBeat(MakeBeatIntent),
}

pub type SampleActionCallback = Arc<dyn Fn(SampleAction) + Send + Sync + 'static>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::SampleFrames;

    #[test]
    fn selection_material_preserves_an_exact_half_open_range() {
        let range = AssetFrameRange::new(SampleFrames(120), SampleFrames(960)).unwrap();
        let selection = SampleSelection {
            asset: AssetId(7),
            source_range: Some(range),
        };

        assert_eq!(
            selection.material(),
            SourceMaterialRef::VirtualSlice(VirtualSliceRef {
                source_asset: AssetId(7),
                source_range: range,
            })
        );
    }

    #[test]
    fn sampler_targets_retain_typed_kit_and_pad_identity() {
        let target = SamplerTarget::Pad {
            kit: KitId::from_raw(4),
            pad: PadId::from_raw(9),
        };
        assert_eq!(target.kit(), Some(KitId::from_raw(4)));
        assert_eq!(target.pad(), Some(PadId::from_raw(9)));
        assert_eq!(SamplerTarget::NewKit.kit(), None);
    }

    #[test]
    fn onset_preview_is_scoped_to_the_exact_selection() {
        let source = SampleSelection {
            asset: AssetId(3),
            source_range: Some(AssetFrameRange::new(SampleFrames(10), SampleFrames(100)).unwrap()),
        };
        let preview = OnsetChopPreview {
            source,
            analyzer: "test-onset".into(),
            boundaries: vec![SampleFrames(30), SampleFrames(60)],
            confidence: Some(0.9),
            diagnostic: None,
        };
        assert!(preview.is_for(source));
        assert!(preview.is_valid());
        assert!(!preview.is_for(SampleSelection::whole_asset(AssetId(3))));

        let invalid = OnsetChopPreview {
            boundaries: vec![SampleFrames(60), SampleFrames(30)],
            ..preview
        };
        assert!(!invalid.is_valid());
    }

    #[test]
    fn envelope_validation_rejects_non_normalized_sustain() {
        assert!(SampleEnvelopeIntent::percussive().is_valid());
        assert!(!SampleEnvelopeIntent {
            sustain: 1.2,
            ..SampleEnvelopeIntent::percussive()
        }
        .is_valid());
    }
}
