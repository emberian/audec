//! Typed drag/drop terms shared by audec's production panes.
//!
//! This module translates a drag payload and a semantic drop target into an
//! edit *intent*. It deliberately does not know about GPUI pointer events,
//! project locks, ID bindings, command application, or audio rendering. In
//! particular, it never converts one domain's typed ID into another domain's
//! ID just because their raw numbers happen to match.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use crate::arrangement::{ClipId, Frame, TrackId, TrackKind};
use crate::aspect::Aspect;
use crate::assets::{AssetFrameRange, AssetId};
use crate::mixer::BusId;
use crate::reconstruction::ReconstructionProposalId;
use crate::sequencer::{PatternId, StepLaneId};

/// Keyboard state captured when a drop is interpreted.
///
/// The GPUI adapter owns the platform mapping. Core policy only needs the
/// semantic distinction between copying, making a linked object unique, and
/// temporarily bypassing a snap decision.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DragModifiers {
    pub duplicate: bool,
    pub make_unique: bool,
    pub suppress_snap: bool,
}

/// A media-pool object, optionally narrowed to an exact decoded-frame span.
///
/// `source_range == None` denotes the whole immutable asset. The controller
/// validates an explicit range against current decoded metadata before it
/// creates a clip or sampler zone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AssetDrag {
    pub asset: AssetId,
    pub source_range: Option<AssetFrameRange>,
}

/// A typed term carried between panes.
#[derive(Clone, Debug, PartialEq)]
pub enum DragPayload {
    Asset(AssetDrag),
    /// A sequencer definition, not an arrangement pattern alias.
    Pattern(PatternId),
    /// Existing arrangement objects. Their relative offsets remain project
    /// truth; the drop target supplies only a new anchor/track.
    ArrangementClips {
        clips: BTreeSet<ClipId>,
        anchor: Frame,
    },
    /// A pure source/evidence selection. Dropping it requests a preview; it
    /// never silently promotes an inference into authored project state.
    Aspect(Aspect),
    /// A ranked reconstruction alternative. Like `Aspect`, it always lowers
    /// to a preview request first.
    Reconstruction(ReconstructionProposalId),
    /// A mixer route source. Route validation remains the controller's job.
    MixerBus(BusId),
}

impl DragPayload {
    pub fn arrangement_clips(
        clips: impl IntoIterator<Item = ClipId>,
        anchor: Frame,
    ) -> Result<Self, DragContractError> {
        let clips = clips.into_iter().collect::<BTreeSet<_>>();
        if clips.is_empty() {
            return Err(DragContractError::EmptyClipSelection);
        }
        Ok(Self::ArrangementClips { clips, anchor })
    }
}

/// A semantic destination captured by a view's hit tester.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DropTarget {
    ArrangementCanvas {
        at: Frame,
    },
    ArrangementTrack {
        track: TrackId,
        kind: TrackKind,
        at: Frame,
    },
    StepPattern {
        pattern: PatternId,
    },
    StepLane {
        pattern: PatternId,
        lane: StepLaneId,
    },
    SamplerPad {
        rack: u64,
        pad: u16,
    },
    MixerBus {
        bus: BusId,
    },
    PatternLibrary,
}

/// Destination for an evidence-preserving preview.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeprojectionDestination {
    Arrangement {
        track: Option<TrackId>,
        at: Frame,
    },
    StepPattern {
        pattern: PatternId,
        lane: Option<StepLaneId>,
    },
    PatternLibrary,
}

/// Controller-facing result of a valid drop.
///
/// These variants intentionally retain source-domain IDs. The controller
/// resolves media-to-arrangement and sequencer-to-arrangement bindings while
/// constructing one aggregate command envelope.
#[derive(Clone, Debug, PartialEq)]
pub enum DropIntent {
    InsertAudio {
        source: AssetDrag,
        track: Option<TrackId>,
        at: Frame,
    },
    InsertPattern {
        pattern: PatternId,
        track: Option<TrackId>,
        at: Frame,
        make_unique: bool,
    },
    MoveArrangementClips {
        clips: BTreeSet<ClipId>,
        original_anchor: Frame,
        target_track: Option<TrackId>,
        target_anchor: Frame,
        duplicate: bool,
        suppress_snap: bool,
    },
    MapAssetToStepPattern {
        source: AssetDrag,
        pattern: PatternId,
        lane: Option<StepLaneId>,
    },
    MapAssetToPad {
        source: AssetDrag,
        rack: u64,
        pad: u16,
    },
    AddPatternToLibrary {
        pattern: PatternId,
        make_unique: bool,
    },
    PreviewAspectDeprojection {
        aspect: Aspect,
        destination: DeprojectionDestination,
    },
    PreviewReconstruction {
        proposal: ReconstructionProposalId,
        destination: DeprojectionDestination,
    },
    RouteBus {
        source: BusId,
        destination: BusId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DragContractError {
    EmptyClipSelection,
    Incompatible {
        payload: &'static str,
        target: &'static str,
    },
    SelfRoute(BusId),
}

impl fmt::Display for DragContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyClipSelection => formatter.write_str("a clip drag cannot be empty"),
            Self::Incompatible { payload, target } => {
                write!(formatter, "{payload} cannot be dropped on {target}")
            }
            Self::SelfRoute(bus) => write!(formatter, "bus {bus} cannot route to itself"),
        }
    }
}

impl Error for DragContractError {}

/// Interpret one drop without consulting or mutating project state.
///
/// Exact target existence, track lock state, asset availability, source-range
/// bounds, routing cycles, ID allocation, and command construction belong to
/// the controller. This function handles only stable interaction semantics.
pub fn interpret_drop(
    payload: DragPayload,
    target: DropTarget,
    modifiers: DragModifiers,
) -> Result<DropIntent, DragContractError> {
    match (payload, target) {
        (
            DragPayload::Asset(source),
            DropTarget::ArrangementTrack {
                track,
                kind: TrackKind::Audio | TrackKind::Hybrid,
                at,
            },
        ) => Ok(DropIntent::InsertAudio {
            source,
            track: Some(track),
            at,
        }),
        (DragPayload::Asset(source), DropTarget::ArrangementCanvas { at }) => {
            Ok(DropIntent::InsertAudio {
                source,
                track: None,
                at,
            })
        }
        (DragPayload::Asset(source), DropTarget::StepPattern { pattern }) => {
            Ok(DropIntent::MapAssetToStepPattern {
                source,
                pattern,
                lane: None,
            })
        }
        (DragPayload::Asset(source), DropTarget::StepLane { pattern, lane }) => {
            Ok(DropIntent::MapAssetToStepPattern {
                source,
                pattern,
                lane: Some(lane),
            })
        }
        (DragPayload::Asset(source), DropTarget::SamplerPad { rack, pad }) => {
            Ok(DropIntent::MapAssetToPad { source, rack, pad })
        }
        (
            DragPayload::Pattern(pattern),
            DropTarget::ArrangementTrack {
                track,
                kind: TrackKind::Pattern | TrackKind::Hybrid,
                at,
            },
        ) => Ok(DropIntent::InsertPattern {
            pattern,
            track: Some(track),
            at,
            make_unique: modifiers.make_unique,
        }),
        (DragPayload::Pattern(pattern), DropTarget::ArrangementCanvas { at }) => {
            Ok(DropIntent::InsertPattern {
                pattern,
                track: None,
                at,
                make_unique: modifiers.make_unique,
            })
        }
        (DragPayload::Pattern(pattern), DropTarget::PatternLibrary) => {
            Ok(DropIntent::AddPatternToLibrary {
                pattern,
                make_unique: modifiers.make_unique,
            })
        }
        (
            DragPayload::ArrangementClips { clips, anchor },
            DropTarget::ArrangementTrack { track, at, .. },
        ) => {
            if clips.is_empty() {
                return Err(DragContractError::EmptyClipSelection);
            }
            Ok(DropIntent::MoveArrangementClips {
                clips,
                original_anchor: anchor,
                target_track: Some(track),
                target_anchor: at,
                duplicate: modifiers.duplicate,
                suppress_snap: modifiers.suppress_snap,
            })
        }
        (DragPayload::ArrangementClips { clips, anchor }, DropTarget::ArrangementCanvas { at }) => {
            if clips.is_empty() {
                return Err(DragContractError::EmptyClipSelection);
            }
            Ok(DropIntent::MoveArrangementClips {
                clips,
                original_anchor: anchor,
                target_track: None,
                target_anchor: at,
                duplicate: modifiers.duplicate,
                suppress_snap: modifiers.suppress_snap,
            })
        }
        (DragPayload::Aspect(aspect), target) => Ok(DropIntent::PreviewAspectDeprojection {
            aspect,
            destination: deprojection_destination(target)
                .ok_or_else(|| incompatible("an aspect", target))?,
        }),
        (DragPayload::Reconstruction(proposal), target) => Ok(DropIntent::PreviewReconstruction {
            proposal,
            destination: deprojection_destination(target)
                .ok_or_else(|| incompatible("a reconstruction proposal", target))?,
        }),
        (DragPayload::MixerBus(source), DropTarget::MixerBus { bus: destination }) => {
            if source == destination {
                Err(DragContractError::SelfRoute(source))
            } else {
                Ok(DropIntent::RouteBus {
                    source,
                    destination,
                })
            }
        }
        (payload, target) => Err(incompatible(payload_name(&payload), target)),
    }
}

fn deprojection_destination(target: DropTarget) -> Option<DeprojectionDestination> {
    match target {
        DropTarget::ArrangementCanvas { at } => {
            Some(DeprojectionDestination::Arrangement { track: None, at })
        }
        DropTarget::ArrangementTrack { track, at, .. } => {
            Some(DeprojectionDestination::Arrangement {
                track: Some(track),
                at,
            })
        }
        DropTarget::StepPattern { pattern } => Some(DeprojectionDestination::StepPattern {
            pattern,
            lane: None,
        }),
        DropTarget::StepLane { pattern, lane } => Some(DeprojectionDestination::StepPattern {
            pattern,
            lane: Some(lane),
        }),
        DropTarget::PatternLibrary => Some(DeprojectionDestination::PatternLibrary),
        DropTarget::SamplerPad { .. } | DropTarget::MixerBus { .. } => None,
    }
}

fn incompatible(payload: &'static str, target: DropTarget) -> DragContractError {
    DragContractError::Incompatible {
        payload,
        target: target_name(target),
    }
}

fn payload_name(payload: &DragPayload) -> &'static str {
    match payload {
        DragPayload::Asset(_) => "an asset",
        DragPayload::Pattern(_) => "a pattern",
        DragPayload::ArrangementClips { .. } => "arrangement clips",
        DragPayload::Aspect(_) => "an aspect",
        DragPayload::Reconstruction(_) => "a reconstruction proposal",
        DragPayload::MixerBus(_) => "a mixer bus",
    }
}

fn target_name(target: DropTarget) -> &'static str {
    match target {
        DropTarget::ArrangementCanvas { .. } => "the arrangement canvas",
        DropTarget::ArrangementTrack { .. } => "that arrangement track",
        DropTarget::StepPattern { .. } => "a step pattern",
        DropTarget::StepLane { .. } => "a step lane",
        DropTarget::SamplerPad { .. } => "a sampler pad",
        DropTarget::MixerBus { .. } => "a mixer bus",
        DropTarget::PatternLibrary => "the pattern library",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aspect::{Aspect, FrameSpan};
    use crate::assets::{AssetFrameRange, SampleFrames};

    fn asset() -> AssetDrag {
        AssetDrag {
            asset: AssetId(7),
            source_range: Some(AssetFrameRange::new(SampleFrames(120), SampleFrames(480)).unwrap()),
        }
    }

    #[test]
    fn an_asset_span_remains_exact_across_track_and_pad_drops() {
        let source = asset();
        let track = interpret_drop(
            DragPayload::Asset(source),
            DropTarget::ArrangementTrack {
                track: TrackId::from_raw(2),
                kind: TrackKind::Audio,
                at: Frame::new(9_600),
            },
            DragModifiers::default(),
        )
        .unwrap();
        let pad = interpret_drop(
            DragPayload::Asset(source),
            DropTarget::SamplerPad { rack: 4, pad: 9 },
            DragModifiers::default(),
        )
        .unwrap();

        assert!(matches!(
            track,
            DropIntent::InsertAudio { source: found, .. } if found == source
        ));
        assert!(matches!(
            pad,
            DropIntent::MapAssetToPad { source: found, .. } if found == source
        ));
    }

    #[test]
    fn incompatible_track_kinds_are_refused_before_command_construction() {
        let error = interpret_drop(
            DragPayload::Asset(asset()),
            DropTarget::ArrangementTrack {
                track: TrackId::from_raw(1),
                kind: TrackKind::Pattern,
                at: Frame::ZERO,
            },
            DragModifiers::default(),
        )
        .unwrap_err();
        assert!(matches!(error, DragContractError::Incompatible { .. }));

        let error = interpret_drop(
            DragPayload::Pattern(PatternId::from_raw(3)),
            DropTarget::ArrangementTrack {
                track: TrackId::from_raw(1),
                kind: TrackKind::Audio,
                at: Frame::ZERO,
            },
            DragModifiers::default(),
        )
        .unwrap_err();
        assert!(matches!(error, DragContractError::Incompatible { .. }));
    }

    #[test]
    fn clip_drag_preserves_ids_anchor_and_modifier_semantics() {
        let clips = [ClipId::from_raw(9), ClipId::from_raw(3)];
        let intent = interpret_drop(
            DragPayload::arrangement_clips(clips, Frame::new(-240)).unwrap(),
            DropTarget::ArrangementTrack {
                track: TrackId::from_raw(8),
                kind: TrackKind::Hybrid,
                at: Frame::new(2_000),
            },
            DragModifiers {
                duplicate: true,
                make_unique: false,
                suppress_snap: true,
            },
        )
        .unwrap();

        assert_eq!(
            intent,
            DropIntent::MoveArrangementClips {
                clips: BTreeSet::from([ClipId::from_raw(3), ClipId::from_raw(9)]),
                original_anchor: Frame::new(-240),
                target_track: Some(TrackId::from_raw(8)),
                target_anchor: Frame::new(2_000),
                duplicate: true,
                suppress_snap: true,
            }
        );
    }

    #[test]
    fn even_a_directly_constructed_empty_clip_payload_is_refused() {
        assert_eq!(
            interpret_drop(
                DragPayload::ArrangementClips {
                    clips: BTreeSet::new(),
                    anchor: Frame::ZERO,
                },
                DropTarget::ArrangementCanvas { at: Frame::ZERO },
                DragModifiers::default(),
            ),
            Err(DragContractError::EmptyClipSelection)
        );
    }

    #[test]
    fn evidence_drops_always_request_preview_instead_of_authorship() {
        let aspect = Aspect::Time(FrameSpan {
            start: 1_000,
            end: 2_000,
        });
        let intent = interpret_drop(
            DragPayload::Aspect(aspect.clone()),
            DropTarget::StepLane {
                pattern: PatternId::from_raw(11),
                lane: StepLaneId::from_raw(12),
            },
            DragModifiers::default(),
        )
        .unwrap();

        assert_eq!(
            intent,
            DropIntent::PreviewAspectDeprojection {
                aspect,
                destination: DeprojectionDestination::StepPattern {
                    pattern: PatternId::from_raw(11),
                    lane: Some(StepLaneId::from_raw(12)),
                },
            }
        );
    }

    #[test]
    fn mixer_self_routes_are_rejected_without_guessing_about_cycles() {
        let bus = BusId::from_raw(5);
        assert_eq!(
            interpret_drop(
                DragPayload::MixerBus(bus),
                DropTarget::MixerBus { bus },
                DragModifiers::default(),
            ),
            Err(DragContractError::SelfRoute(bus))
        );
    }
}
