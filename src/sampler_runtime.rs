//! Runtime resolution of persistent sample-kit targets.
//!
//! The sample-kit domain stores exact material and route intent, not decoded
//! buffers or live voices. This module is the control-thread boundary that
//! proves those references against the decoder cache and produces immutable
//! sampler definitions. It never guesses a bus, widens a virtual slice, or
//! treats a content fingerprint as an instrument identity.

use std::collections::BTreeMap;
use std::fmt;

use crate::assets::{AssetAvailability, AssetId};
use crate::daw_engine::AssetPcmMap;
use crate::daw_project::{BridgeError, DawProject};
use crate::instruments::{SampleData, SamplerMode, SamplerParams};
use crate::mixer::BusId;
use crate::sample_kit::SampleTargetRef;
use crate::sample_material::{
    canonical_pcm_identity, extract_virtual_slice, CanonicalPcmIdentity, DecodedPcmView,
    SourceMaterialRef,
};
use crate::sequencer::SampleAssetId;

/// One persistent sample alias resolved into an immutable runtime definition.
///
/// `sample_alias` remains the event-consumption identity. Kit, pad, and zone
/// identities are retained for diagnostics and future pad inspection; they
/// are not collapsed into the sequencer's alias space.
#[derive(Clone, Debug)]
pub struct ResolvedSamplerRoute {
    pub sample_alias: SampleAssetId,
    pub target: SampleTargetRef,
    pub bus: BusId,
    pub sample: SampleData,
    pub params: SamplerParams,
}

/// A deterministic control-thread product. Routes are ordered by sequencer
/// alias because the project bindings use a `BTreeMap`.
#[derive(Clone, Debug, Default)]
pub struct SamplerRouteBuild {
    pub routes: Vec<ResolvedSamplerRoute>,
    pub diagnostics: Vec<SamplerRuntimeDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SamplerRuntimeDiagnostic {
    SourceOffline {
        sample_alias: SampleAssetId,
        target: SampleTargetRef,
        asset: AssetId,
    },
    PcmNotSupplied {
        sample_alias: SampleAssetId,
        target: SampleTargetRef,
        asset: AssetId,
    },
    PcmMetadataMismatch {
        sample_alias: SampleAssetId,
        target: SampleTargetRef,
        asset: AssetId,
        registry_sample_rate: u32,
        pcm_sample_rate: u32,
        registry_channels: u16,
        pcm_channels: u16,
        registry_frames: u64,
        pcm_frames: u64,
    },
    MaterialResolutionFailed {
        sample_alias: SampleAssetId,
        target: SampleTargetRef,
        asset: AssetId,
        reason: String,
    },
    CanonicalIdentityMismatch {
        sample_alias: SampleAssetId,
        target: SampleTargetRef,
        expected: CanonicalPcmIdentity,
        actual: CanonicalPcmIdentity,
    },
    UnsupportedChannelCount {
        sample_alias: SampleAssetId,
        target: SampleTargetRef,
        channels: u16,
    },
}

impl fmt::Display for SamplerRuntimeDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "sample runtime refused a route: {self:?}")
    }
}

/// Resolve every typed sample-target binding in a validated aggregate.
///
/// Missing or stale decoder products suppress only the affected route and
/// become diagnostics. Structurally invalid projects remain errors. Whole
/// assets and virtual ranges pass through the same canonical-identity check.
pub fn build_authoritative_sampler_routes(
    project: &DawProject,
    pcm: &AssetPcmMap,
) -> Result<SamplerRouteBuild, BridgeError> {
    project.require_valid()?;
    let state = project.state();
    let mut build = SamplerRouteBuild::default();

    for (&sample_alias, &target) in &state.bindings.sample_targets.targets {
        let kit = state
            .domains
            .sample_kits
            .kits
            .get(&target.kit)
            .expect("aggregate validation guarantees the target kit");
        let pad = kit
            .pads
            .get(&target.pad)
            .expect("aggregate validation guarantees the target pad");
        let zone = kit
            .zone_for_target(target)
            .expect("aggregate validation guarantees the target zone");
        let asset_id = zone.material.asset_id();
        let registry_asset = state
            .domains
            .assets
            .get(asset_id)
            .expect("aggregate validation guarantees source material");
        if matches!(
            registry_asset.availability(),
            AssetAvailability::Missing { .. }
        ) {
            build
                .diagnostics
                .push(SamplerRuntimeDiagnostic::SourceOffline {
                    sample_alias,
                    target,
                    asset: asset_id,
                });
            continue;
        }
        let Some(source_pcm) = pcm.get(&asset_id) else {
            build
                .diagnostics
                .push(SamplerRuntimeDiagnostic::PcmNotSupplied {
                    sample_alias,
                    target,
                    asset: asset_id,
                });
            continue;
        };
        let metadata = registry_asset.metadata();
        if metadata.sample_rate_hz != source_pcm.format.sample_rate.get()
            || metadata.channels != source_pcm.format.channels.get()
            || metadata.frame_count.0 != source_pcm.frame_count()
        {
            build
                .diagnostics
                .push(SamplerRuntimeDiagnostic::PcmMetadataMismatch {
                    sample_alias,
                    target,
                    asset: asset_id,
                    registry_sample_rate: metadata.sample_rate_hz,
                    pcm_sample_rate: source_pcm.format.sample_rate.get(),
                    registry_channels: metadata.channels,
                    pcm_channels: source_pcm.format.channels.get(),
                    registry_frames: metadata.frame_count.0,
                    pcm_frames: source_pcm.frame_count(),
                });
            continue;
        }

        let (format, interleaved, actual_identity) = match zone.material {
            SourceMaterialRef::Asset(_) => {
                let view = DecodedPcmView::from(source_pcm);
                let identity = match canonical_pcm_identity(view) {
                    Ok(identity) => identity,
                    Err(error) => {
                        build.diagnostics.push(
                            SamplerRuntimeDiagnostic::MaterialResolutionFailed {
                                sample_alias,
                                target,
                                asset: asset_id,
                                reason: error.to_string(),
                            },
                        );
                        continue;
                    }
                };
                (source_pcm.format, source_pcm.samples.clone(), identity)
            }
            SourceMaterialRef::VirtualSlice(slice) => {
                let extracted = match extract_virtual_slice(slice, source_pcm) {
                    Ok(extracted) => extracted,
                    Err(error) => {
                        build.diagnostics.push(
                            SamplerRuntimeDiagnostic::MaterialResolutionFailed {
                                sample_alias,
                                target,
                                asset: asset_id,
                                reason: error.to_string(),
                            },
                        );
                        continue;
                    }
                };
                (extracted.format, extracted.interleaved, extracted.identity)
            }
        };

        if let Some(expected) = zone.decoded_pcm {
            if expected != actual_identity {
                build
                    .diagnostics
                    .push(SamplerRuntimeDiagnostic::CanonicalIdentityMismatch {
                        sample_alias,
                        target,
                        expected,
                        actual: actual_identity,
                    });
                continue;
            }
        }

        let channels = format.channels.get();
        if !(1..=2).contains(&channels) {
            build
                .diagnostics
                .push(SamplerRuntimeDiagnostic::UnsupportedChannelCount {
                    sample_alias,
                    target,
                    channels,
                });
            continue;
        }
        let channels = channels as u8;
        let sample = match SampleData::from_interleaved(
            format.sample_rate.get(),
            channels,
            interleaved,
            60,
            zone.tuning_cents,
        ) {
            Ok(sample) => sample,
            Err(error) => {
                build
                    .diagnostics
                    .push(SamplerRuntimeDiagnostic::MaterialResolutionFailed {
                        sample_alias,
                        target,
                        asset: asset_id,
                        reason: error.to_string(),
                    });
                continue;
            }
        };
        build.routes.push(ResolvedSamplerRoute {
            sample_alias,
            target,
            bus: kit.output.bus,
            sample,
            params: SamplerParams {
                mode: SamplerMode::OneShot,
                gain_db: zone.gain_db,
                pan: zone.pan,
                maximum_voices: 32,
                trigger_asset: Some(sample_alias.get()),
                choke_group: pad.choke_group,
            },
        });
    }
    Ok(build)
}

/// Find the route metadata needed to make authored lane choke groups and kit
/// pad choke groups share one runtime behavior.
pub fn route_choke_groups(
    routes: &BTreeMap<u64, crate::daw_engine::BuiltInInstrumentRoute>,
) -> BTreeMap<u64, Option<u32>> {
    routes
        .values()
        .filter_map(|route| match &route.definition {
            crate::daw_engine::BuiltInInstrumentDefinition::Sampler { params, .. } => params
                .trigger_asset
                .map(|alias| (alias, params.choke_group)),
            crate::daw_engine::BuiltInInstrumentDefinition::Subtractive(_) => None,
        })
        .collect()
}
