//! Pure resolution of sampler audition intents against one coherent project
//! publication. This module never owns a device or transport.

use std::error::Error;
use std::fmt;

use crate::assets::{AssetFrameRange, SampleFrames};
use crate::daw_render::PcmAsset;
use crate::live_project::LiveProjectSnapshot;
use crate::render_runtime::AuditionOwner;
use crate::sample_actions::SampleAuditionIntent;
use crate::sample_kit::{KitId, PadId, SampleTargetRef, ZoneId};
use crate::sample_material::SourceMaterialRef;

/// Ownership token for the independent preview bus. Generations are allocated
/// by the pane/session adapter and are local to one owner.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SamplePreviewToken {
    pub owner: AuditionOwner,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SamplePreviewTarget {
    Material(SourceMaterialRef),
    Pad {
        kit: KitId,
        pad: PadId,
        zone: Option<ZoneId>,
    },
}

/// Zero-copy reference to exact PCM and its half-open decoded-frame range.
/// Applying gain/pan/tuning belongs to the existing preview bus adapter; the
/// resolver never scans or duplicates sample buffers on the UI thread.
#[derive(Clone, Debug)]
pub struct SamplePreviewClipRef {
    pub pcm: PcmAsset,
    pub source_range: AssetFrameRange,
    pub gain: f32,
    pub pan: f32,
    pub tuning_cents: f32,
}

#[derive(Clone, Debug)]
pub enum SamplePreviewCommand {
    Start {
        token: SamplePreviewToken,
        target: SamplePreviewTarget,
        clip: SamplePreviewClipRef,
    },
    Stop {
        token: SamplePreviewToken,
        target: SamplePreviewTarget,
    },
}

#[derive(Clone, Debug)]
pub struct ResolvedSamplePreview {
    pub project_revision: u64,
    pub command: SamplePreviewCommand,
}

/// Resolve a semantic one-shot/pad gate without touching audio hardware. Pad
/// releases intentionally do not require the kit to still exist: their typed
/// owner token must remain capable of stopping an older preview.
pub fn resolve_sample_audition(
    snapshot: &LiveProjectSnapshot,
    token: SamplePreviewToken,
    intent: SampleAuditionIntent,
) -> Result<ResolvedSamplePreview, SamplePreviewError> {
    let command = match intent {
        SampleAuditionIntent::MaterialOneShot { material, velocity } => {
            let velocity = normalized_velocity(velocity)?;
            let asset = material.asset_id();
            let pcm = snapshot
                .pcm
                .get(&asset)
                .cloned()
                .ok_or(SamplePreviewError::MissingAssetPcm(asset))?;
            let source_range = material_range(material, &pcm)?;
            SamplePreviewCommand::Start {
                token,
                target: SamplePreviewTarget::Material(material),
                clip: SamplePreviewClipRef {
                    pcm,
                    source_range,
                    gain: velocity,
                    pan: 0.0,
                    tuning_cents: 0.0,
                },
            }
        }
        SampleAuditionIntent::PadGate {
            kit,
            pad,
            velocity: _,
            pressed: false,
        } => SamplePreviewCommand::Stop {
            token,
            target: SamplePreviewTarget::Pad {
                kit,
                pad,
                // A release addresses the pad gate, not a possibly replaced zone.
                zone: None,
            },
        },
        SampleAuditionIntent::PadGate {
            kit,
            pad,
            velocity,
            pressed: true,
        } => {
            let velocity = normalized_velocity(velocity)?;
            let sample_kit = snapshot
                .project
                .state()
                .domains
                .sample_kits
                .kits
                .get(&kit)
                .ok_or(SamplePreviewError::MissingKit(kit))?;
            let target = sample_kit
                .primary_target(pad)
                .ok_or(SamplePreviewError::MissingPadMaterial { kit, pad })?;
            let zone = sample_kit
                .zone_for_target(target)
                .ok_or(SamplePreviewError::MissingPadMaterial { kit, pad })?;
            let pcm = snapshot
                .sample_pcm
                .get(&target)
                .cloned()
                .ok_or(SamplePreviewError::MissingZonePcm(target))?;
            let source_range = whole_pcm_range(&pcm)?;
            let gain = velocity * 10.0_f32.powf(zone.gain_db / 20.0);
            SamplePreviewCommand::Start {
                token,
                target: SamplePreviewTarget::Pad {
                    kit,
                    pad,
                    zone: Some(target.zone),
                },
                clip: SamplePreviewClipRef {
                    pcm,
                    source_range,
                    gain,
                    pan: zone.pan,
                    tuning_cents: zone.tuning_cents,
                },
            }
        }
    };
    Ok(ResolvedSamplePreview {
        project_revision: snapshot.revisions().aggregate,
        command,
    })
}

fn normalized_velocity(velocity: f32) -> Result<f32, SamplePreviewError> {
    if !velocity.is_finite() {
        return Err(SamplePreviewError::InvalidVelocity);
    }
    Ok(velocity.clamp(0.0, 1.0))
}

fn material_range(
    material: SourceMaterialRef,
    pcm: &PcmAsset,
) -> Result<AssetFrameRange, SamplePreviewError> {
    let range = match material {
        SourceMaterialRef::Asset(_) => whole_pcm_range(pcm)?,
        SourceMaterialRef::VirtualSlice(slice) => slice.source_range,
    };
    if !range.is_within(SampleFrames(pcm.frame_count())) {
        return Err(SamplePreviewError::InvalidSourceRange(range));
    }
    Ok(range)
}

fn whole_pcm_range(pcm: &PcmAsset) -> Result<AssetFrameRange, SamplePreviewError> {
    AssetFrameRange::new(SampleFrames(0), SampleFrames(pcm.frame_count()))
        .map_err(|_| SamplePreviewError::EmptyPcm)
}

/// Pure last-writer/ownership reducer used before touching `AudioHost`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SamplePreviewState {
    pub active: Option<SamplePreviewToken>,
}

#[derive(Clone, Debug)]
pub enum SamplePreviewEffect {
    Play(SamplePreviewClipRef),
    Stop,
    IgnoreStale,
}

impl SamplePreviewState {
    pub fn apply(&mut self, command: SamplePreviewCommand) -> SamplePreviewEffect {
        match command {
            SamplePreviewCommand::Start { token, clip, .. } => {
                if self.active.is_some_and(|active| {
                    active.owner == token.owner && active.generation > token.generation
                }) {
                    return SamplePreviewEffect::IgnoreStale;
                }
                self.active = Some(token);
                SamplePreviewEffect::Play(clip)
            }
            SamplePreviewCommand::Stop { token, .. } if self.active == Some(token) => {
                self.active = None;
                SamplePreviewEffect::Stop
            }
            SamplePreviewCommand::Stop { .. } => SamplePreviewEffect::IgnoreStale,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SamplePreviewError {
    MissingAssetPcm(crate::assets::AssetId),
    MissingKit(KitId),
    MissingPadMaterial { kit: KitId, pad: PadId },
    MissingZonePcm(SampleTargetRef),
    InvalidVelocity,
    InvalidSourceRange(AssetFrameRange),
    EmptyPcm,
}

impl fmt::Display for SamplePreviewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl Error for SamplePreviewError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(generation: u64) -> SamplePreviewToken {
        SamplePreviewToken {
            owner: AuditionOwner {
                namespace: 7,
                local: 9,
            },
            generation,
        }
    }

    fn stop(token: SamplePreviewToken) -> SamplePreviewCommand {
        SamplePreviewCommand::Stop {
            token,
            target: SamplePreviewTarget::Pad {
                kit: KitId::from_raw(1),
                pad: PadId::from_raw(1),
                zone: None,
            },
        }
    }

    #[test]
    fn stale_gate_release_cannot_stop_a_newer_generation() {
        let mut state = SamplePreviewState {
            active: Some(token(2)),
        };
        assert!(matches!(
            state.apply(stop(token(1))),
            SamplePreviewEffect::IgnoreStale
        ));
        assert_eq!(state.active, Some(token(2)));
        assert!(matches!(
            state.apply(stop(token(2))),
            SamplePreviewEffect::Stop
        ));
        assert_eq!(state.active, None);
    }
}
