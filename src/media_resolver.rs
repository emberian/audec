//! Explicit source and derived-media resolution for an opened project.
//!
//! A project package stores route *intent*, not an assertion that its media is
//! present.  This module turns that intent into decoder requests and detailed
//! repair diagnostics.  It never mutates `AssetRegistry`: a UI/controller must
//! explicitly accept a [`RelinkProposal`] before calling `AssetRegistry::relink`.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, BufReader, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rubato::audioadapter_buffers::owned::InterleavedOwned;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CodecType, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::arrangement::AssetId as ArrangementAssetId;
use crate::assets::{
    AbsolutePath, AssetFrameRange, AssetId, AssetLocation, AssetOrigin, AssetProvenance,
    AssetRegistration, ContentFingerprint, ContentHashAlgorithm, ContentId, DecodeIntegrity,
    DecodedAudioMetadata, MediaAsset, PcmMaterializationProvenance, SampleFrames,
    SampleRateMaterializationRecipe, SourceDecodeProvenance,
};
use crate::audio::AudioFormat;
use crate::daw_render::{
    MediaAssetDescriptor, MediaBlockDemand, MediaBlockProvider, MediaBlockSource,
    MediaPreparationError, MediaReadError, MediaReadFailure, PcmAsset,
};
use crate::project_io::AssetPathIntent;
use crate::pyramid::{StreamingWaveformError, StreamingWaveformIndex};
use crate::sample_material::{canonical_pcm_identity, DecodedPcmView};
use crate::streaming_media::{
    BoundedMediaStore, CacheAccounting, CacheBudgets, ChunkLeaseId, DecodeRequest,
    DecodedPcmDescriptor, DecodedPcmId, PcmChunk, PcmChunkGeometry, PcmChunkIndex, RequestPriority,
    StreamingMediaError,
};

/// Direct dependency versions are part of every decode/conversion recipe.
/// Symphonia is MPL-2.0; Rubato is MIT OR Apache-2.0.
pub const SYMPHONIA_DECODER_VERSION: &str = "0.5.5";
pub const RUBATO_CONVERTER_VERSION: &str = "5.0.0";

const DEFAULT_MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_MAX_DECODED_SAMPLES: u64 = 2_000_000_000;
const DEFAULT_RESAMPLE_CHUNK_FRAMES: usize = 1_024;

/// Whether a decoder request is for an original registered file or a material
/// derived from an exact source span.  Derived material stays first-class in
/// the resolver even when its PCM is cached/generated locally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MaterialKind {
    Source,
    Derived {
        source_asset: AssetId,
        source_range: AssetFrameRange,
        derivation: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterialRequest {
    pub asset: AssetId,
    pub name: String,
    pub path: AssetPathIntent,
    pub expected_metadata: DecodedAudioMetadata,
    pub expected_fingerprint: ContentFingerprint,
    pub kind: MaterialKind,
}

impl MaterialRequest {
    pub fn source(
        asset: AssetId,
        name: impl Into<String>,
        path: AssetPathIntent,
        expected_metadata: DecodedAudioMetadata,
        expected_fingerprint: ContentFingerprint,
    ) -> Self {
        Self {
            asset,
            name: name.into(),
            path,
            expected_metadata,
            expected_fingerprint,
            kind: MaterialKind::Source,
        }
    }

    pub fn derived(
        mut source: Self,
        source_asset: AssetId,
        source_range: AssetFrameRange,
        derivation: impl Into<String>,
    ) -> Self {
        source.kind = MaterialKind::Derived {
            source_asset,
            source_range,
            derivation: derivation.into(),
        };
        source
    }

    pub fn candidates(&self, project_manifest: &Path) -> Vec<PathBuf> {
        self.path.candidates(project_manifest)
    }
}

/// A successful decoder result.  The decoder is responsible for producing a
/// fingerprint over the same source bytes that supplied the PCM.
#[derive(Clone, Debug)]
pub struct DecodedMaterial {
    pub path: PathBuf,
    pub metadata: DecodedAudioMetadata,
    pub fingerprint: ContentFingerprint,
    pub pcm: PcmAsset,
}

/// Audec-owned account of how encoded source bytes became canonical PCM.
/// Library identifiers are descriptive recipe facts, never project identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaDecodeProvenance {
    pub backend: &'static str,
    pub backend_version: &'static str,
    pub source_bytes: u64,
    pub stream_count: u32,
    pub selected_track_id: u32,
    pub container: Option<String>,
    pub codec: String,
    pub declared_frames: Option<u64>,
    pub gapless: bool,
    pub verification: DecodeVerification,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeVerification {
    /// The codec supplied and passed an integrity check.
    Passed,
    /// The codec does not expose a whole-stream integrity check.
    Unavailable,
}

/// Extended result available from the production decoder. The small
/// `MediaDecoder` compatibility seam still returns `decoded`; callers that
/// publish derived PCM use this form so the decoder recipe stays attached.
#[derive(Clone, Debug)]
pub struct ProvenancedDecodedMaterial {
    pub decoded: DecodedMaterial,
    pub provenance: MediaDecodeProvenance,
}

/// A project-rate view retains the complete source identity that was decoded.
/// The converted PCM is derived material and never overwrites source metadata.
#[derive(Clone, Debug)]
pub struct ProjectRateMaterial {
    pub source_path: PathBuf,
    pub source_metadata: DecodedAudioMetadata,
    pub source_fingerprint: ContentFingerprint,
    pub decode_provenance: MediaDecodeProvenance,
    pub pcm: PcmAsset,
    pub conversion: Option<SampleRateConversionProvenance>,
}

/// Ready-to-register imported material. Its public metadata and fingerprint
/// identify the exact project-rate PCM consumed by preview and rendering;
/// immutable provenance separately retains the encoded source identity needed
/// to reopen or safely relink the file.
#[derive(Clone, Debug)]
pub struct MaterializedMediaImport {
    pub registration: AssetRegistration,
    pub pcm: PcmAsset,
}

impl ProvenancedDecodedMaterial {
    /// Materialize PCM at an explicitly selected project rate. Equal rates
    /// share the original allocation and do not invent a conversion recipe.
    pub fn pcm_for_project_rate(
        &self,
        project_sample_rate_hz: u32,
        converter: &impl SampleRateConverter,
    ) -> Result<ProjectRateMaterial, SampleRateConversionError> {
        if project_sample_rate_hz == 0 {
            return Err(SampleRateConversionError::ZeroOutputRate);
        }
        let (pcm, conversion) =
            if self.decoded.pcm.format.sample_rate.get() == project_sample_rate_hz {
                (self.decoded.pcm.clone(), None)
            } else {
                let converted = converter.convert(&self.decoded.pcm, project_sample_rate_hz)?;
                (converted.pcm, Some(converted.provenance))
            };
        Ok(ProjectRateMaterial {
            source_path: self.decoded.path.clone(),
            source_metadata: self.decoded.metadata.clone(),
            source_fingerprint: self.decoded.fingerprint,
            decode_provenance: self.provenance.clone(),
            pcm,
            conversion,
        })
    }

    /// Finish a filesystem import as one reusable project-rate PCM asset.
    /// This is UI-neutral and performs no registry mutation; callers can pass
    /// the returned pair to the aggregate import transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn materialize_import(
        &self,
        name: impl Into<String>,
        project_sample_rate_hz: u32,
        converter: &impl SampleRateConverter,
        imported_at_unix_ms: u64,
        importer: impl Into<String>,
        tags: BTreeSet<String>,
        favorite: bool,
    ) -> Result<MaterializedMediaImport, MaterializedImportError> {
        let absolute = AbsolutePath::parse(self.decoded.path.to_string_lossy().into_owned())?;
        let location = AssetLocation::new(Some(absolute), None)?;
        let project_rate = self
            .pcm_for_project_rate(project_sample_rate_hz, converter)
            .map_err(MaterializedImportError::Conversion)?;
        let output_identity =
            canonical_pcm_identity(DecodedPcmView::from_pcm_asset(&project_rate.pcm))
                .map_err(|error| MaterializedImportError::CanonicalPcm(error.to_string()))?;
        let output_metadata = DecodedAudioMetadata {
            sample_rate_hz: project_rate.pcm.format.sample_rate.get(),
            channels: project_rate.pcm.format.channels.get(),
            frame_count: SampleFrames(project_rate.pcm.frame_count()),
            container: Some("audec-canonical-pcm".into()),
            codec: Some("pcm_f32le".into()),
            bit_depth: Some(32),
        };
        let materialization = PcmMaterializationProvenance {
            source_metadata: project_rate.source_metadata,
            source_content: project_rate.source_fingerprint,
            decode: project_rate.decode_provenance.to_durable(),
            sample_rate: project_rate
                .conversion
                .as_ref()
                .map(SampleRateConversionProvenance::to_durable),
        };
        let provenance = AssetProvenance::new(
            imported_at_unix_ms,
            AssetOrigin::ImportedFile {
                importer: importer.into(),
            },
            location.clone(),
        )
        .with_materialization(materialization);
        let registration = AssetRegistration {
            name: name.into(),
            location,
            metadata: output_metadata,
            content: output_identity.fingerprint,
            provenance,
            tags,
            favorite,
        };
        registration.validate()?;
        Ok(MaterializedMediaImport {
            registration,
            pcm: project_rate.pcm,
        })
    }
}

impl MediaDecodeProvenance {
    fn to_durable(&self) -> SourceDecodeProvenance {
        SourceDecodeProvenance {
            backend: self.backend.into(),
            backend_version: self.backend_version.into(),
            source_bytes: self.source_bytes,
            stream_count: self.stream_count,
            selected_track_id: self.selected_track_id,
            container: self.container.clone(),
            codec: self.codec.clone(),
            declared_frames: self.declared_frames,
            gapless: self.gapless,
            verification: match self.verification {
                DecodeVerification::Passed => DecodeIntegrity::Passed,
                DecodeVerification::Unavailable => DecodeIntegrity::Unavailable,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MaterializedImportError {
    Decode(MediaDecodeError),
    Asset(crate::assets::AssetError),
    Conversion(SampleRateConversionError),
    CanonicalPcm(String),
}

impl From<crate::assets::AssetError> for MaterializedImportError {
    fn from(error: crate::assets::AssetError) -> Self {
        Self::Asset(error)
    }
}

impl From<MediaDecodeError> for MaterializedImportError {
    fn from(error: MediaDecodeError) -> Self {
        Self::Decode(error)
    }
}

impl fmt::Display for MaterializedImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => write!(formatter, "could not decode imported media: {error}"),
            Self::Asset(error) => write!(formatter, "invalid imported asset: {error}"),
            Self::Conversion(error) => write!(formatter, "could not materialize import: {error}"),
            Self::CanonicalPcm(error) => {
                write!(formatter, "could not fingerprint materialized PCM: {error}")
            }
        }
    }
}

impl std::error::Error for MaterializedImportError {}

/// Deliberately small async-agnostic seam.  The application can invoke this
/// on a worker, and test decoders can remain deterministic without a window or
/// a particular file-format dependency.
pub trait MediaDecoder {
    fn decode(&self, path: &Path) -> Result<DecodedMaterial, MediaDecodeError>;
}

/// Resolver input for project-rate chunk streaming. Encoded-source facts and
/// canonical project-PCM facts stay separate: a local path can verify the
/// former without being silently accepted as a relink or renamed as the
/// latter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamingMaterialRequest {
    pub asset: AssetId,
    pub path: AssetPathIntent,
    pub source_metadata: DecodedAudioMetadata,
    pub source_fingerprint: ContentFingerprint,
    pub project_metadata: DecodedAudioMetadata,
    pub project_pcm_fingerprint: ContentFingerprint,
}

impl StreamingMaterialRequest {
    pub fn from_asset(asset: &MediaAsset) -> Self {
        Self {
            asset: asset.id(),
            path: AssetPathIntent::from_location(asset.location()),
            source_metadata: asset.source_metadata().clone(),
            source_fingerprint: asset.source_content(),
            project_metadata: asset.metadata().clone(),
            project_pcm_fingerprint: asset.content(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocatedRouteKind {
    ProjectRelative,
    OriginalAbsolute,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocatedMaterialRoute {
    pub path: PathBuf,
    pub kind: LocatedRouteKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissingMaterialRoute {
    pub attempted: Vec<PathBuf>,
}

/// Find a saved route without decoding it and without constructing or
/// accepting a relink. Route discovery is deliberately weaker than identity
/// verification.
pub fn locate_material_route(
    project_manifest: &Path,
    intent: &AssetPathIntent,
) -> Result<LocatedMaterialRoute, MissingMaterialRoute> {
    let project_relative = intent.project_relative.as_ref().map(|path| {
        project_manifest
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path)
    });
    let original = intent.original_absolute.as_ref().map(PathBuf::from);
    let mut attempted = Vec::new();
    if let Some(path) = project_relative {
        attempted.push(path.clone());
        if path.is_file() {
            return Ok(LocatedMaterialRoute {
                path,
                kind: LocatedRouteKind::ProjectRelative,
            });
        }
    }
    if let Some(path) = original {
        if !attempted.contains(&path) {
            attempted.push(path.clone());
        }
        if path.is_file() {
            return Ok(LocatedMaterialRoute {
                path,
                kind: LocatedRouteKind::OriginalAbsolute,
            });
        }
    }
    Err(MissingMaterialRoute { attempted })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedMaterialRoute {
    pub located: LocatedMaterialRoute,
    pub fingerprint: ContentFingerprint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteIdentityMismatch {
    pub located: LocatedMaterialRoute,
    pub expected: ContentFingerprint,
    pub observed: ContentFingerprint,
}

/// Verify encoded bytes incrementally. A mismatch remains a repair fact for a
/// controller to present; it never mutates `AssetRegistry` or creates an
/// accepted relink on its own.
pub fn verify_material_route(
    located: LocatedMaterialRoute,
    expected: ContentFingerprint,
    maximum_bytes: u64,
) -> Result<VerifiedMaterialRoute, StreamingOpenError> {
    let observed = fingerprint_file(&located.path, maximum_bytes)?;
    if observed != expected {
        return Err(StreamingOpenError::IdentityMismatch(
            RouteIdentityMismatch {
                located,
                expected,
                observed,
            },
        ));
    }
    Ok(VerifiedMaterialRoute {
        located,
        fingerprint: observed,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectRateChunkMode {
    /// Decoder packets are traversed into one requested equal-rate chunk. No
    /// complete decoded file is retained.
    PacketStream,
    /// Cross-rate conversion currently uses the established exact whole-file
    /// Rubato path, then exposes its PCM through the same chunk contract.
    WholeFileResampleFallback,
}

#[derive(Clone, Debug)]
pub struct SymphoniaProjectRateChunkSource {
    route: VerifiedMaterialRoute,
    source_metadata: DecodedAudioMetadata,
    descriptor: DecodedPcmDescriptor<ContentId>,
    waveform_bin_frames: u32,
    maximum_decoded_samples: u64,
}

#[derive(Clone, Debug)]
pub struct WholePcmChunkSource {
    route: VerifiedMaterialRoute,
    descriptor: DecodedPcmDescriptor<ContentId>,
    pcm: PcmAsset,
    waveform_bin_frames: u32,
}

#[derive(Clone, Debug)]
pub enum ProjectRateChunkSource {
    PacketStream(SymphoniaProjectRateChunkSource),
    WholeFileResampleFallback(WholePcmChunkSource),
}

impl ProjectRateChunkSource {
    pub fn mode(&self) -> ProjectRateChunkMode {
        match self {
            Self::PacketStream(_) => ProjectRateChunkMode::PacketStream,
            Self::WholeFileResampleFallback(_) => ProjectRateChunkMode::WholeFileResampleFallback,
        }
    }

    pub fn descriptor(&self) -> DecodedPcmDescriptor<ContentId> {
        match self {
            Self::PacketStream(source) => source.descriptor,
            Self::WholeFileResampleFallback(source) => source.descriptor,
        }
    }

    pub fn route(&self) -> &VerifiedMaterialRoute {
        match self {
            Self::PacketStream(source) => &source.route,
            Self::WholeFileResampleFallback(source) => &source.route,
        }
    }

    pub fn diagnostic(&self) -> Option<ResolutionDiagnostic> {
        (self.mode() == ProjectRateChunkMode::WholeFileResampleFallback).then(|| {
            ResolutionDiagnostic {
                path: Some(self.route().located.path.clone()),
                code: "whole-file-resample-fallback",
                message: "source and project sample rates differ; exact Rubato conversion currently retains the converted file before publishing bounded chunks".into(),
            }
        })
    }

    pub fn read_chunk(
        &self,
        index: PcmChunkIndex,
    ) -> Result<PcmChunk<ContentId>, StreamingOpenError> {
        match self {
            Self::PacketStream(source) => source.read_chunk(index),
            Self::WholeFileResampleFallback(source) => source.read_chunk(index),
        }
    }

    /// Hydrate only requested viewport/playback/lookahead chunks. Existing
    /// residents are reused. Waveform side products survive PCM eviction in
    /// the caller-owned sparse index.
    pub fn hydrate_requests(
        &self,
        requests: impl IntoIterator<Item = DecodeRequest<ContentId>>,
        store: &mut BoundedMediaStore<ContentId>,
        waveforms: &mut StreamingWaveformIndex<ContentId>,
    ) -> Result<ChunkHydrationReport, StreamingOpenError> {
        if waveforms.source() != self.descriptor() {
            return Err(StreamingOpenError::Waveform(
                StreamingWaveformError::SourceMismatch,
            ));
        }
        let mut report = ChunkHydrationReport::default();
        for request in requests {
            if request.key.pcm != self.descriptor().id
                || request.key.geometry != self.descriptor().geometry
            {
                return Err(StreamingOpenError::RequestSourceMismatch);
            }
            if store.contains_resident(request.key) {
                report.reused += 1;
                continue;
            }
            let chunk = self.read_chunk(request.key.index)?;
            waveforms.publish(chunk.waveform.clone())?;
            store.publish_resident(chunk)?;
            report.decoded += 1;
        }
        Ok(report)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChunkHydrationReport {
    pub decoded: usize,
    pub reused: usize,
}

#[derive(Clone, Debug)]
struct PreparedStreamingAsset {
    descriptor: MediaAssetDescriptor,
    source: DecodedPcmDescriptor<ContentId>,
    chunks: BTreeMap<PcmChunkIndex, Arc<PcmChunk<ContentId>>>,
}

/// Immutable lease snapshot published to the compiled audio graph. The
/// control side clones only the `Arc`s for demanded chunks; callback reads are
/// therefore bounded map/index lookups and never reach the decoder or cache.
#[derive(Clone, Debug, Default)]
pub struct PreparedStreamingMediaProvider {
    assets: BTreeMap<ArrangementAssetId, PreparedStreamingAsset>,
}

impl MediaBlockProvider for PreparedStreamingMediaProvider {
    fn descriptor(&self, asset: ArrangementAssetId) -> Option<MediaAssetDescriptor> {
        self.assets.get(&asset).map(|prepared| prepared.descriptor)
    }

    fn sample(
        &self,
        asset: ArrangementAssetId,
        frame: u64,
        channel: u16,
    ) -> Result<f32, MediaReadError> {
        let failure = |failure| MediaReadError {
            asset,
            frame,
            channel,
            failure,
        };
        let prepared = self
            .assets
            .get(&asset)
            .ok_or_else(|| failure(MediaReadFailure::UnknownAsset))?;
        if frame >= prepared.descriptor.frame_count {
            return Err(failure(MediaReadFailure::FrameOutsideAsset));
        }
        let channels = prepared.descriptor.format.channels.get();
        if channel >= channels {
            return Err(failure(MediaReadFailure::ChannelOutsideAsset));
        }
        let chunk_index = prepared.source.geometry.chunk_index(frame);
        let chunk = prepared
            .chunks
            .get(&chunk_index)
            .ok_or_else(|| failure(MediaReadFailure::FrameUnavailable))?;
        let chunk_start = chunk
            .key
            .index
            .0
            .checked_mul(u64::from(prepared.source.geometry.frames_per_chunk))
            .ok_or_else(|| failure(MediaReadFailure::FrameOutsideAsset))?;
        let local_frame = frame
            .checked_sub(chunk_start)
            .ok_or_else(|| failure(MediaReadFailure::FrameOutsideAsset))?;
        if !chunk.span.contains(frame) {
            return Err(failure(MediaReadFailure::FrameUnavailable));
        }
        let sample_index = usize::try_from(local_frame)
            .ok()
            .and_then(|local| local.checked_mul(usize::from(channels)))
            .and_then(|start| start.checked_add(usize::from(channel)))
            .ok_or_else(|| failure(MediaReadFailure::FrameOutsideAsset))?;
        chunk
            .interleaved
            .get(sample_index)
            .copied()
            .ok_or_else(|| failure(MediaReadFailure::FrameUnavailable))
    }
}

/// Resolver-backed media source for the native graph. Asset registration is
/// explicit: filesystem discovery/relink identity remains in the resolver,
/// while the graph sees only an arrangement ID and canonical PCM shape.
///
/// `prepare` may decode and touch the bounded cache, but its returned provider
/// is immutable and contains only the exact chunks demanded for the next
/// offline or realtime window.
#[derive(Clone, Debug)]
pub struct StreamingGraphMediaSource {
    sources: BTreeMap<ArrangementAssetId, ProjectRateChunkSource>,
    store: BoundedMediaStore<ContentId>,
    waveforms: BTreeMap<ArrangementAssetId, StreamingWaveformIndex<ContentId>>,
    demand_epoch: u64,
}

impl StreamingGraphMediaSource {
    pub fn new(budgets: CacheBudgets) -> Result<Self, StreamingMediaError> {
        Ok(Self {
            sources: BTreeMap::new(),
            store: BoundedMediaStore::new(budgets)?,
            waveforms: BTreeMap::new(),
            demand_epoch: 0,
        })
    }

    pub fn register(
        &mut self,
        asset: ArrangementAssetId,
        source: ProjectRateChunkSource,
    ) -> Result<(), MediaPreparationError> {
        if self.sources.contains_key(&asset) {
            return Err(MediaPreparationError::Provider(format!(
                "media asset {} already has a streaming source",
                asset.get()
            )));
        }
        let descriptor = source.descriptor();
        self.waveforms
            .insert(asset, StreamingWaveformIndex::new(descriptor));
        self.sources.insert(asset, source);
        Ok(())
    }

    pub fn unregister(&mut self, asset: ArrangementAssetId) -> bool {
        self.waveforms.remove(&asset);
        self.sources.remove(&asset).is_some()
    }

    pub fn waveform(
        &self,
        asset: ArrangementAssetId,
    ) -> Option<&StreamingWaveformIndex<ContentId>> {
        self.waveforms.get(&asset)
    }

    pub fn cache_accounting(&self) -> CacheAccounting {
        self.store.accounting()
    }

    fn prepared_catalog(&self) -> Result<PreparedStreamingMediaProvider, MediaPreparationError> {
        let mut assets = BTreeMap::new();
        for (&asset, source) in &self.sources {
            let source = source.descriptor();
            let format = AudioFormat::new(source.geometry.sample_rate_hz, source.geometry.channels)
                .map_err(|error| MediaPreparationError::Provider(error.to_string()))?;
            assets.insert(
                asset,
                PreparedStreamingAsset {
                    descriptor: MediaAssetDescriptor {
                        format,
                        frame_count: source.frame_count,
                    },
                    source,
                    chunks: BTreeMap::new(),
                },
            );
        }
        Ok(PreparedStreamingMediaProvider { assets })
    }
}

impl MediaBlockSource for StreamingGraphMediaSource {
    fn prepare(
        &mut self,
        demands: &[MediaBlockDemand],
    ) -> Result<Arc<dyn MediaBlockProvider>, MediaPreparationError> {
        let mut provider = self.prepared_catalog()?;
        self.demand_epoch = self.demand_epoch.saturating_add(1);

        let mut demanded = BTreeSet::new();
        for demand in demands {
            let Some(source) = self.sources.get(&demand.asset) else {
                return Err(MediaPreparationError::UnknownAsset(demand.asset));
            };
            let descriptor = source.descriptor();
            if demand.start_frame >= demand.end_frame {
                return Err(MediaPreparationError::InvalidDemand {
                    asset: demand.asset,
                    start_frame: demand.start_frame,
                    end_frame: demand.end_frame,
                });
            }
            if demand.end_frame > descriptor.frame_count {
                return Err(MediaPreparationError::DemandOutsideAsset {
                    asset: demand.asset,
                    end_frame: demand.end_frame,
                    frame_count: descriptor.frame_count,
                });
            }
            let first = descriptor.geometry.chunk_index(demand.start_frame).0;
            let last = descriptor
                .geometry
                .chunk_index(demand.end_frame.saturating_sub(1))
                .0;
            for index in first..=last {
                demanded.insert((
                    demand.asset,
                    descriptor
                        .chunk_key(PcmChunkIndex(index))
                        .map_err(|error| MediaPreparationError::Provider(error.to_string()))?,
                ));
            }
        }

        // Pin every already-prepared chunk until the immutable snapshot owns
        // its Arc. If the configured budget cannot hold one render window, the
        // cache reports that honestly instead of publishing a partial window.
        let mut leases: Vec<ChunkLeaseId> = Vec::with_capacity(demanded.len());
        let result = (|| {
            for (asset, key) in demanded {
                let source = self
                    .sources
                    .get(&asset)
                    .cloned()
                    .ok_or(MediaPreparationError::UnknownAsset(asset))?;
                let waveform = self.waveforms.get_mut(&asset).ok_or_else(|| {
                    MediaPreparationError::Provider(format!(
                        "media asset {} has no waveform side-product index",
                        asset.get()
                    ))
                })?;
                source
                    .hydrate_requests(
                        [DecodeRequest {
                            key,
                            priority: RequestPriority::Playback,
                            distance_chunks: 0,
                            demand_epoch: self.demand_epoch,
                        }],
                        &mut self.store,
                        waveform,
                    )
                    .map_err(|error| MediaPreparationError::Provider(error.to_string()))?;
                let lease = self
                    .store
                    .acquire(key)
                    .map_err(|error| MediaPreparationError::Provider(error.to_string()))?;
                provider
                    .assets
                    .get_mut(&asset)
                    .expect("prepared catalog contains every registered source")
                    .chunks
                    .insert(key.index, Arc::clone(&lease.chunk));
                leases.push(lease.id);
            }
            Ok::<(), MediaPreparationError>(())
        })();

        let release_result = leases.into_iter().try_for_each(|lease| {
            self.store
                .release(lease)
                .map_err(|error| MediaPreparationError::Provider(error.to_string()))
        });
        result?;
        release_result?;
        Ok(Arc::new(provider))
    }
}

/// Canonical decoder for Audec's common import formats. The complete encoded
/// source is read once into a bounded immutable snapshot so its fingerprint
/// describes the exact bytes supplied to Symphonia.
#[derive(Clone, Debug)]
pub struct SymphoniaMediaDecoder {
    maximum_source_bytes: u64,
    maximum_decoded_samples: u64,
}

impl SymphoniaMediaDecoder {
    pub fn new(
        maximum_source_bytes: u64,
        maximum_decoded_samples: u64,
    ) -> Result<Self, MediaDecodeError> {
        if maximum_source_bytes == 0 || maximum_decoded_samples == 0 {
            return Err(MediaDecodeError::InvalidOutput(
                "decoder byte and sample limits must be non-zero".into(),
            ));
        }
        Ok(Self {
            maximum_source_bytes,
            maximum_decoded_samples,
        })
    }

    pub const fn maximum_source_bytes(&self) -> u64 {
        self.maximum_source_bytes
    }

    pub const fn maximum_decoded_samples(&self) -> u64 {
        self.maximum_decoded_samples
    }
}

impl Default for SymphoniaMediaDecoder {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_SOURCE_BYTES, DEFAULT_MAX_DECODED_SAMPLES)
            .expect("default media decode limits are non-zero")
    }
}

impl SymphoniaMediaDecoder {
    /// Open an existing registered asset as bounded project-rate chunks. Equal
    /// sample rates use packet streaming. Cross-rate assets deliberately stay
    /// on the established whole-file converter until a seekable resample cache
    /// can prove the same frame-exact recipe.
    pub fn open_project_rate_chunk_source(
        &self,
        project_manifest: &Path,
        request: &StreamingMaterialRequest,
        project_sample_rate_hz: u32,
        frames_per_chunk: u32,
        waveform_bin_frames: u32,
        converter: &impl SampleRateConverter,
    ) -> Result<ProjectRateChunkSource, StreamingOpenError> {
        if project_sample_rate_hz == 0
            || request.project_metadata.sample_rate_hz != project_sample_rate_hz
            || request.project_metadata.channels == 0
            || request.project_metadata.frame_count.0 == 0
            || request.project_pcm_fingerprint.bytes_hashed == 0
            || request.source_fingerprint.bytes_hashed == 0
        {
            return Err(StreamingOpenError::InvalidRequest(
                "streaming metadata, content identities, and project rate must be complete".into(),
            ));
        }
        let geometry = PcmChunkGeometry::new(
            project_sample_rate_hz,
            request.project_metadata.channels,
            frames_per_chunk,
        )?;
        if waveform_bin_frames == 0
            || !waveform_bin_frames.is_power_of_two()
            || waveform_bin_frames > frames_per_chunk
            || frames_per_chunk % waveform_bin_frames != 0
        {
            return Err(StreamingOpenError::InvalidRequest(
                "waveform bins must be a power-of-two divisor of PCM chunks".into(),
            ));
        }
        let located = locate_material_route(project_manifest, &request.path)
            .map_err(StreamingOpenError::MissingRoute)?;
        let route = verify_material_route(
            located,
            request.source_fingerprint,
            self.maximum_source_bytes,
        )?;
        let descriptor = DecodedPcmDescriptor::new(
            DecodedPcmId(request.project_pcm_fingerprint.id),
            geometry,
            request.project_metadata.frame_count.0,
        )?;

        if request.source_metadata.sample_rate_hz == project_sample_rate_hz {
            if request.source_metadata.channels != request.project_metadata.channels
                || request.source_metadata.frame_count != request.project_metadata.frame_count
            {
                return Err(StreamingOpenError::InvalidRequest(
                    "equal-rate source and canonical project PCM disagree on shape".into(),
                ));
            }
            let total_samples = request
                .source_metadata
                .frame_count
                .0
                .checked_mul(u64::from(request.source_metadata.channels))
                .ok_or_else(|| {
                    StreamingOpenError::Media(MediaDecodeError::LimitExceeded(
                        "streaming source sample count overflowed".into(),
                    ))
                })?;
            if total_samples > self.maximum_decoded_samples {
                return Err(StreamingOpenError::Media(MediaDecodeError::LimitExceeded(
                    format!(
                        "decoded stream exceeds the configured {}-sample limit",
                        self.maximum_decoded_samples
                    ),
                )));
            }
            return Ok(ProjectRateChunkSource::PacketStream(
                SymphoniaProjectRateChunkSource {
                    route,
                    source_metadata: request.source_metadata.clone(),
                    descriptor,
                    waveform_bin_frames,
                    maximum_decoded_samples: self.maximum_decoded_samples,
                },
            ));
        }

        let decoded = self.decode_provenanced(&route.located.path)?;
        if decoded.decoded.fingerprint != request.source_fingerprint
            || decoded.decoded.metadata != request.source_metadata
        {
            return Err(StreamingOpenError::InvalidRequest(
                "whole-file fallback decode disagrees with registered source identity".into(),
            ));
        }
        let project_rate = decoded.pcm_for_project_rate(project_sample_rate_hz, converter)?;
        if project_rate.pcm.format.sample_rate.get() != request.project_metadata.sample_rate_hz
            || project_rate.pcm.format.channels.get() != request.project_metadata.channels
            || project_rate.pcm.frame_count() != request.project_metadata.frame_count.0
        {
            return Err(StreamingOpenError::InvalidRequest(
                "whole-file fallback disagrees with canonical project PCM shape".into(),
            ));
        }
        let identity = canonical_pcm_identity(DecodedPcmView::from_pcm_asset(&project_rate.pcm))
            .map_err(|error| StreamingOpenError::InvalidRequest(error.to_string()))?;
        if identity.fingerprint != request.project_pcm_fingerprint {
            return Err(StreamingOpenError::InvalidRequest(
                "whole-file fallback disagrees with canonical project PCM identity".into(),
            ));
        }
        Ok(ProjectRateChunkSource::WholeFileResampleFallback(
            WholePcmChunkSource {
                route,
                descriptor,
                pcm: project_rate.pcm,
                waveform_bin_frames,
            },
        ))
    }

    pub fn decode_provenanced(
        &self,
        path: &Path,
    ) -> Result<ProvenancedDecodedMaterial, MediaDecodeError> {
        let source = read_source_snapshot(path, self.maximum_source_bytes)?;
        let fingerprint = ContentFingerprint::from_bytes(&source);
        let source_bytes = source.len() as u64;
        let container = identify_container(&source).map(str::to_owned);

        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
            hint.with_extension(extension);
        }
        let stream = MediaSourceStream::new(Box::new(Cursor::new(source)), Default::default());
        let format_options = FormatOptions {
            enable_gapless: true,
            ..FormatOptions::default()
        };
        let probe = symphonia::default::get_probe()
            .format(&hint, stream, &format_options, &MetadataOptions::default())
            .map_err(|error| map_symphonia_error("container probe", error))?;
        let mut format = probe.format;
        let stream_count = u32::try_from(format.tracks().len()).unwrap_or(u32::MAX);
        let track = format.default_track().ok_or_else(|| {
            MediaDecodeError::UnsupportedFormat(
                "container probe found no default audio stream".into(),
            )
        })?;
        let track_id = track.id;
        let codec_type = track.codec_params.codec;
        let codec = codec_name(codec_type);
        let declared_frames = track.codec_params.n_frames;
        let declared_sample_rate = track.codec_params.sample_rate;
        let declared_channels = track.codec_params.channels.map(|channels| channels.count());
        let bit_depth = track
            .codec_params
            .bits_per_sample
            .or(track.codec_params.bits_per_coded_sample)
            .and_then(|value| u16::try_from(value).ok());
        let mut decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions { verify: true })
            .map_err(|error| {
                map_symphonia_error(&format!("codec initialization ({codec})"), error)
            })?;

        let mut samples = Vec::new();
        let mut decoded_format: Option<(u32, u16)> = None;
        loop {
            let packet = match format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(error))
                    if error.kind() == io::ErrorKind::UnexpectedEof =>
                {
                    break;
                }
                Err(error) => return Err(map_symphonia_error("packet stream", error)),
            };
            if packet.track_id() != track_id {
                continue;
            }
            let packet_timestamp = packet.ts;
            let decoded = decoder.decode(&packet).map_err(|error| {
                map_symphonia_error(
                    &format!(
                        "codec decode ({codec}, track {track_id}, timestamp {packet_timestamp})"
                    ),
                    error,
                )
            })?;
            let sample_rate_hz = decoded.spec().rate;
            let channel_count = decoded.spec().channels.count();
            let channels = u16::try_from(channel_count).map_err(|_| {
                MediaDecodeError::InvalidOutput(format!(
                    "codec {codec} produced {channel_count} channels"
                ))
            })?;
            if sample_rate_hz == 0 || channels == 0 {
                return Err(MediaDecodeError::InvalidOutput(format!(
                    "codec {codec} produced a zero sample rate or channel count"
                )));
            }
            if let Some(expected) = decoded_format {
                if expected != (sample_rate_hz, channels) {
                    return Err(MediaDecodeError::Corrupt(format!(
                        "codec stream changed format from {} Hz/{} channels to {sample_rate_hz} Hz/{channels} channels",
                        expected.0, expected.1
                    )));
                }
            } else {
                decoded_format = Some((sample_rate_hz, channels));
            }

            let packet_samples = decoded.frames().checked_mul(channel_count).ok_or_else(|| {
                MediaDecodeError::LimitExceeded("decoded packet sample count overflowed".into())
            })?;
            let new_sample_count = samples.len().checked_add(packet_samples).ok_or_else(|| {
                MediaDecodeError::LimitExceeded("decoded sample count overflowed".into())
            })?;
            let new_sample_count_u64 = u64::try_from(new_sample_count).map_err(|_| {
                MediaDecodeError::LimitExceeded(
                    "decoded sample count cannot be represented by the configured limit".into(),
                )
            })?;
            if new_sample_count_u64 > self.maximum_decoded_samples {
                return Err(MediaDecodeError::LimitExceeded(format!(
                    "decoded stream exceeds the configured {}-sample limit",
                    self.maximum_decoded_samples
                )));
            }
            let mut converted =
                SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
            converted.copy_interleaved_ref(decoded);
            if converted.samples().len() != packet_samples {
                return Err(MediaDecodeError::InvalidOutput(format!(
                    "codec {codec} reported {packet_samples} samples but converted {}",
                    converted.samples().len()
                )));
            }
            if let Some(index) = converted
                .samples()
                .iter()
                .position(|sample| !sample.is_finite())
            {
                return Err(MediaDecodeError::InvalidOutput(format!(
                    "codec {codec} produced a non-finite sample at packet offset {index}"
                )));
            }
            samples.extend_from_slice(converted.samples());
        }

        let (sample_rate_hz, channels) = decoded_format.ok_or_else(|| {
            MediaDecodeError::Corrupt(format!(
                "codec {codec} produced no audio frames for track {track_id}"
            ))
        })?;
        if samples.is_empty() {
            return Err(MediaDecodeError::Corrupt(format!(
                "codec {codec} produced an empty audio stream"
            )));
        }
        if let Some(rate) = declared_sample_rate {
            if rate != sample_rate_hz {
                return Err(MediaDecodeError::Corrupt(format!(
                    "codec metadata declares {rate} Hz but decoded PCM is {sample_rate_hz} Hz"
                )));
            }
        }
        if let Some(count) = declared_channels {
            if count != usize::from(channels) {
                return Err(MediaDecodeError::Corrupt(format!(
                    "codec metadata declares {count} channels but decoded PCM has {channels}"
                )));
            }
        }

        let verification = match decoder.finalize().verify_ok {
            Some(true) => DecodeVerification::Passed,
            Some(false) => {
                return Err(MediaDecodeError::Corrupt(format!(
                    "codec integrity verification failed for {codec} track {track_id}"
                )))
            }
            None => DecodeVerification::Unavailable,
        };
        let frame_count = samples.len() / usize::from(channels);
        let frame_count = u64::try_from(frame_count).map_err(|_| {
            MediaDecodeError::LimitExceeded("decoded frame count does not fit u64".into())
        })?;
        let audio_format = AudioFormat::new(sample_rate_hz, channels)
            .map_err(|error| MediaDecodeError::InvalidOutput(error.to_string()))?;
        let pcm = PcmAsset::new(audio_format, Arc::from(samples))
            .map_err(|error| MediaDecodeError::InvalidOutput(error.to_string()))?;
        let metadata = DecodedAudioMetadata {
            sample_rate_hz,
            channels,
            frame_count: SampleFrames(frame_count),
            container: container.clone(),
            codec: Some(codec.clone()),
            bit_depth,
        };
        metadata
            .validate()
            .map_err(|error| MediaDecodeError::InvalidOutput(error.to_string()))?;

        Ok(ProvenancedDecodedMaterial {
            decoded: DecodedMaterial {
                path: path.to_path_buf(),
                metadata,
                fingerprint,
                pcm,
            },
            provenance: MediaDecodeProvenance {
                backend: "symphonia",
                backend_version: SYMPHONIA_DECODER_VERSION,
                source_bytes,
                stream_count,
                selected_track_id: track_id,
                container,
                codec,
                declared_frames,
                gapless: true,
                verification,
            },
        })
    }

    /// Decode one immutable source snapshot and finish the project-rate asset
    /// registration/PCM pair in one operation. The caller still owns the
    /// aggregate transaction, so a failed registration can never leave a
    /// metadata-only asset behind.
    #[allow(clippy::too_many_arguments)]
    pub fn materialize_import(
        &self,
        path: &Path,
        name: impl Into<String>,
        project_sample_rate_hz: u32,
        converter: &impl SampleRateConverter,
        imported_at_unix_ms: u64,
        importer: impl Into<String>,
        tags: BTreeSet<String>,
        favorite: bool,
    ) -> Result<MaterializedMediaImport, MaterializedImportError> {
        self.decode_provenanced(path)?.materialize_import(
            name,
            project_sample_rate_hz,
            converter,
            imported_at_unix_ms,
            importer,
            tags,
            favorite,
        )
    }
}

impl MediaDecoder for SymphoniaMediaDecoder {
    fn decode(&self, path: &Path) -> Result<DecodedMaterial, MediaDecodeError> {
        self.decode_provenanced(path).map(|decoded| decoded.decoded)
    }
}

impl SymphoniaProjectRateChunkSource {
    fn read_chunk(&self, index: PcmChunkIndex) -> Result<PcmChunk<ContentId>, StreamingOpenError> {
        let wanted = self
            .descriptor
            .geometry
            .chunk_span(index, self.descriptor.frame_count)?;
        let file = File::open(&self.route.located.path).map_err(|error| {
            StreamingOpenError::Media(MediaDecodeError::Io(format!(
                "opening {}: {error}",
                self.route.located.path.display()
            )))
        })?;
        let mut hint = Hint::new();
        if let Some(extension) = self
            .route
            .located
            .path
            .extension()
            .and_then(|value| value.to_str())
        {
            hint.with_extension(extension);
        }
        let stream = MediaSourceStream::new(Box::new(file), Default::default());
        let probe = symphonia::default::get_probe()
            .format(
                &hint,
                stream,
                &FormatOptions {
                    enable_gapless: true,
                    ..FormatOptions::default()
                },
                &MetadataOptions::default(),
            )
            .map_err(|error| {
                StreamingOpenError::Media(map_symphonia_error("container probe", error))
            })?;
        let mut format = probe.format;
        let track = format.default_track().ok_or_else(|| {
            StreamingOpenError::Media(MediaDecodeError::UnsupportedFormat(
                "container probe found no default audio stream".into(),
            ))
        })?;
        let track_id = track.id;
        let codec = codec_name(track.codec_params.codec);
        if track
            .codec_params
            .sample_rate
            .is_some_and(|rate| rate != self.source_metadata.sample_rate_hz)
            || track.codec_params.channels.is_some_and(|channels| {
                channels.count() != usize::from(self.source_metadata.channels)
            })
        {
            return Err(StreamingOpenError::Media(MediaDecodeError::Corrupt(
                "verified source now reports a different stream shape".into(),
            )));
        }
        let mut decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions { verify: true })
            .map_err(|error| {
                StreamingOpenError::Media(map_symphonia_error(
                    &format!("codec initialization ({codec})"),
                    error,
                ))
            })?;
        let channels = usize::from(self.source_metadata.channels);
        let expected_samples = self.descriptor.geometry.interleaved_samples(wanted.len())?;
        let mut output = Vec::with_capacity(expected_samples);
        let mut decoded_cursor = 0_u64;
        if wanted.start > 0 {
            match format.seek(
                SeekMode::Accurate,
                SeekTo::TimeStamp {
                    ts: wanted.start,
                    track_id,
                },
            ) {
                Ok(seeked) => {
                    if seeked.actual_ts > wanted.start {
                        return Err(StreamingOpenError::Media(MediaDecodeError::Corrupt(
                            format!(
                                "accurate seek landed after requested frame {} at {}",
                                wanted.start, seeked.actual_ts
                            ),
                        )));
                    }
                    decoder.reset();
                    decoded_cursor = seeked.actual_ts;
                }
                // Some valid containers are not seekable. Sequential traversal
                // is slower but remains bounded and exact, so retain it as a
                // per-request fallback instead of materializing whole PCM.
                Err(SymphoniaError::SeekError(_)) => {}
                Err(error) => {
                    return Err(StreamingOpenError::Media(map_symphonia_error(
                        "accurate chunk seek",
                        error,
                    )))
                }
            }
        }
        let needs_eof_verification = wanted.end == self.descriptor.frame_count;
        let mut reached_eof = false;
        loop {
            let packet = match format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(error))
                    if error.kind() == io::ErrorKind::UnexpectedEof =>
                {
                    reached_eof = true;
                    break;
                }
                Err(error) => {
                    return Err(StreamingOpenError::Media(map_symphonia_error(
                        "packet stream",
                        error,
                    )))
                }
            };
            if packet.track_id() != track_id {
                continue;
            }
            let decoded = decoder.decode(&packet).map_err(|error| {
                StreamingOpenError::Media(map_symphonia_error(
                    &format!("codec decode ({codec}, track {track_id})"),
                    error,
                ))
            })?;
            if decoded.spec().rate != self.source_metadata.sample_rate_hz
                || decoded.spec().channels.count() != channels
            {
                return Err(StreamingOpenError::Media(MediaDecodeError::Corrupt(
                    "verified source changed format while decoding a chunk".into(),
                )));
            }
            let packet_frames = u64::try_from(decoded.frames()).map_err(|_| {
                StreamingOpenError::Media(MediaDecodeError::LimitExceeded(
                    "decoded packet frame count does not fit u64".into(),
                ))
            })?;
            let packet_end = decoded_cursor.checked_add(packet_frames).ok_or_else(|| {
                StreamingOpenError::Media(MediaDecodeError::LimitExceeded(
                    "decoded frame cursor overflowed".into(),
                ))
            })?;
            let decoded_samples = packet_end.checked_mul(channels as u64).ok_or_else(|| {
                StreamingOpenError::Media(MediaDecodeError::LimitExceeded(
                    "decoded sample cursor overflowed".into(),
                ))
            })?;
            if decoded_samples > self.maximum_decoded_samples {
                return Err(StreamingOpenError::Media(MediaDecodeError::LimitExceeded(
                    format!(
                        "decoded stream exceeds the configured {}-sample limit",
                        self.maximum_decoded_samples
                    ),
                )));
            }
            let mut converted =
                SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
            converted.copy_interleaved_ref(decoded);
            if let Some(sample_index) = converted
                .samples()
                .iter()
                .position(|sample| !sample.is_finite())
            {
                return Err(StreamingOpenError::Media(MediaDecodeError::InvalidOutput(
                    format!(
                        "codec {codec} produced a non-finite sample at packet offset {sample_index}"
                    ),
                )));
            }
            let overlap_start = decoded_cursor.max(wanted.start);
            let overlap_end = packet_end.min(wanted.end);
            if overlap_start < overlap_end {
                let local_start = usize::try_from(overlap_start - decoded_cursor)
                    .map_err(|_| StreamingOpenError::ArithmeticOverflow)?;
                let local_end = usize::try_from(overlap_end - decoded_cursor)
                    .map_err(|_| StreamingOpenError::ArithmeticOverflow)?;
                output.extend_from_slice(
                    &converted.samples()[local_start * channels..local_end * channels],
                );
            }
            decoded_cursor = packet_end;
            if decoded_cursor >= wanted.end && !needs_eof_verification {
                break;
            }
            if decoded_cursor > self.descriptor.frame_count {
                return Err(StreamingOpenError::Media(MediaDecodeError::Corrupt(
                    "verified source decoded beyond registered project PCM".into(),
                )));
            }
        }
        if output.len() != expected_samples {
            return Err(StreamingOpenError::Media(MediaDecodeError::Corrupt(
                format!(
                    "chunk {} decoded {} samples; expected {expected_samples}",
                    index.0,
                    output.len()
                ),
            )));
        }
        if needs_eof_verification {
            if !reached_eof || decoded_cursor != self.descriptor.frame_count {
                return Err(StreamingOpenError::Media(MediaDecodeError::Corrupt(
                    format!(
                        "verified source ended at {decoded_cursor} frames; expected {}",
                        self.descriptor.frame_count
                    ),
                )));
            }
            if decoder.finalize().verify_ok == Some(false) {
                return Err(StreamingOpenError::Media(MediaDecodeError::Corrupt(
                    format!("codec integrity verification failed for {codec} track {track_id}"),
                )));
            }
        }
        PcmChunk::new(
            self.descriptor,
            index,
            output.into(),
            self.waveform_bin_frames,
        )
        .map_err(StreamingOpenError::Streaming)
    }
}

impl WholePcmChunkSource {
    fn read_chunk(&self, index: PcmChunkIndex) -> Result<PcmChunk<ContentId>, StreamingOpenError> {
        let span = self
            .descriptor
            .geometry
            .chunk_span(index, self.descriptor.frame_count)?;
        let channels = usize::from(self.descriptor.geometry.channels);
        let start = usize::try_from(span.start)
            .ok()
            .and_then(|frame| frame.checked_mul(channels))
            .ok_or(StreamingOpenError::ArithmeticOverflow)?;
        let end = usize::try_from(span.end)
            .ok()
            .and_then(|frame| frame.checked_mul(channels))
            .ok_or(StreamingOpenError::ArithmeticOverflow)?;
        PcmChunk::new(
            self.descriptor,
            index,
            Arc::from(&self.pcm.samples[start..end]),
            self.waveform_bin_frames,
        )
        .map_err(StreamingOpenError::Streaming)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamingOpenError {
    MissingRoute(MissingMaterialRoute),
    IdentityMismatch(RouteIdentityMismatch),
    InvalidRequest(String),
    Media(MediaDecodeError),
    Conversion(SampleRateConversionError),
    Streaming(StreamingMediaError),
    Waveform(StreamingWaveformError),
    RequestSourceMismatch,
    ArithmeticOverflow,
}

impl fmt::Display for StreamingOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRoute(route) => write!(
                formatter,
                "none of {} saved media routes is a file",
                route.attempted.len()
            ),
            Self::IdentityMismatch(mismatch) => write!(
                formatter,
                "{} does not match the registered encoded source identity",
                mismatch.located.path.display()
            ),
            Self::InvalidRequest(message) => {
                write!(formatter, "invalid streaming request: {message}")
            }
            Self::Media(error) => write!(formatter, "streaming decode failed: {error}"),
            Self::Conversion(error) => {
                write!(formatter, "streaming conversion fallback failed: {error}")
            }
            Self::Streaming(error) => write!(formatter, "streaming cache failed: {error}"),
            Self::Waveform(error) => write!(formatter, "streaming waveform failed: {error}"),
            Self::RequestSourceMismatch => {
                formatter.write_str("chunk request addresses different project PCM")
            }
            Self::ArithmeticOverflow => {
                formatter.write_str("streaming media arithmetic overflowed")
            }
        }
    }
}

impl Error for StreamingOpenError {}

impl From<MediaDecodeError> for StreamingOpenError {
    fn from(error: MediaDecodeError) -> Self {
        Self::Media(error)
    }
}

impl From<SampleRateConversionError> for StreamingOpenError {
    fn from(error: SampleRateConversionError) -> Self {
        Self::Conversion(error)
    }
}

impl From<StreamingMediaError> for StreamingOpenError {
    fn from(error: StreamingMediaError) -> Self {
        Self::Streaming(error)
    }
}

impl From<StreamingWaveformError> for StreamingOpenError {
    fn from(error: StreamingWaveformError) -> Self {
        Self::Waveform(error)
    }
}

fn fingerprint_file(
    path: &Path,
    maximum_bytes: u64,
) -> Result<ContentFingerprint, StreamingOpenError> {
    const FNV_OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const FNV_PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let file = File::open(path).map_err(|error| {
        StreamingOpenError::Media(MediaDecodeError::Io(format!(
            "opening {}: {error}",
            path.display()
        )))
    })?;
    if file
        .metadata()
        .ok()
        .is_some_and(|metadata| metadata.len() > maximum_bytes)
    {
        return Err(StreamingOpenError::Media(MediaDecodeError::LimitExceeded(
            format!(
                "{} exceeds the configured {maximum_bytes}-byte source limit",
                path.display()
            ),
        )));
    }
    let mut reader = BufReader::new(file).take(maximum_bytes.saturating_add(1));
    let mut buffer = [0_u8; 64 * 1024];
    let mut hash = FNV_OFFSET;
    let mut bytes_hashed = 0_u64;
    loop {
        let count = reader.read(&mut buffer).map_err(|error| {
            StreamingOpenError::Media(MediaDecodeError::Io(format!(
                "reading {}: {error}",
                path.display()
            )))
        })?;
        if count == 0 {
            break;
        }
        bytes_hashed = bytes_hashed
            .checked_add(count as u64)
            .ok_or(StreamingOpenError::ArithmeticOverflow)?;
        if bytes_hashed > maximum_bytes {
            return Err(StreamingOpenError::Media(MediaDecodeError::LimitExceeded(
                format!(
                    "{} exceeds the configured {maximum_bytes}-byte source limit",
                    path.display()
                ),
            )));
        }
        for byte in &buffer[..count] {
            hash ^= u128::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    if bytes_hashed == 0 {
        return Err(StreamingOpenError::Media(MediaDecodeError::Corrupt(
            "source file is empty".into(),
        )));
    }
    Ok(ContentFingerprint {
        algorithm: ContentHashAlgorithm::Fnv1a128NonCryptographic,
        id: ContentId(hash),
        bytes_hashed,
    })
}

fn read_source_snapshot(path: &Path, maximum_bytes: u64) -> Result<Vec<u8>, MediaDecodeError> {
    let file = File::open(path)
        .map_err(|error| MediaDecodeError::Io(format!("opening {}: {error}", path.display())))?;
    let declared_bytes = file.metadata().ok().map(|metadata| metadata.len());
    if declared_bytes.is_some_and(|bytes| bytes > maximum_bytes) {
        return Err(MediaDecodeError::LimitExceeded(format!(
            "{} exceeds the configured {maximum_bytes}-byte source limit",
            path.display()
        )));
    }
    let capacity = declared_bytes
        .unwrap_or(0)
        .min(maximum_bytes)
        .min(usize::MAX as u64) as usize;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| MediaDecodeError::Io(format!("reading {}: {error}", path.display())))?;
    if bytes.len() as u64 > maximum_bytes {
        return Err(MediaDecodeError::LimitExceeded(format!(
            "{} exceeds the configured {maximum_bytes}-byte source limit",
            path.display()
        )));
    }
    if bytes.is_empty() {
        return Err(MediaDecodeError::Corrupt("source file is empty".into()));
    }
    Ok(bytes)
}

fn identify_container(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"fLaC") {
        Some("flac")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        Some("wav")
    } else if bytes.starts_with(b"OggS") {
        Some("ogg")
    } else if bytes.starts_with(b"ID3")
        || bytes
            .windows(2)
            .take(4_096)
            .any(|window| window[0] == 0xff && window[1] & 0xe0 == 0xe0)
    {
        Some("mp3")
    } else {
        None
    }
}

fn codec_name(codec: CodecType) -> String {
    symphonia::default::get_codecs()
        .get_codec(codec)
        .map(|descriptor| descriptor.short_name.to_owned())
        .unwrap_or_else(|| format!("unknown-{codec}"))
}

fn map_symphonia_error(stage: &str, error: SymphoniaError) -> MediaDecodeError {
    match error {
        SymphoniaError::Unsupported(message) => {
            MediaDecodeError::UnsupportedFormat(format!("{stage}: {message}"))
        }
        SymphoniaError::LimitError(message) => {
            MediaDecodeError::LimitExceeded(format!("{stage}: {message}"))
        }
        SymphoniaError::IoError(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            MediaDecodeError::Corrupt(format!("{stage}: unexpected end of stream"))
        }
        SymphoniaError::IoError(error) => MediaDecodeError::Io(format!("{stage}: {error}")),
        SymphoniaError::DecodeError(message) => {
            MediaDecodeError::Corrupt(format!("{stage}: {message}"))
        }
        SymphoniaError::SeekError(error) => {
            MediaDecodeError::Corrupt(format!("{stage}: {error:?}"))
        }
        SymphoniaError::ResetRequired => {
            MediaDecodeError::Corrupt(format!("{stage}: stream requires an unsupported reset"))
        }
    }
}

/// Explicit, async-agnostic conversion seam. Implementations convert sample
/// rate only; channel layout and ordering must remain unchanged.
pub trait SampleRateConverter {
    fn convert(
        &self,
        source: &PcmAsset,
        output_sample_rate_hz: u32,
    ) -> Result<ConvertedPcm, SampleRateConversionError>;
}

#[derive(Clone, Debug)]
pub struct ConvertedPcm {
    pub pcm: PcmAsset,
    pub provenance: SampleRateConversionProvenance,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleRateConversionProvenance {
    pub backend: &'static str,
    pub backend_version: &'static str,
    pub algorithm: &'static str,
    pub input_sample_rate_hz: u32,
    pub output_sample_rate_hz: u32,
    pub channels: u16,
    pub input_frames: u64,
    pub output_frames: u64,
    pub chunk_frames: usize,
    pub sinc_length: usize,
    pub cutoff_bits: Option<u32>,
    pub oversampling_factor: usize,
    pub interpolation: &'static str,
    pub window: &'static str,
    pub delay_removed: bool,
    pub trimmed_output_frames: u64,
}

#[derive(Clone, Debug)]
pub struct RubatoSampleRateConverter {
    chunk_frames: usize,
    parameters: SincInterpolationParameters,
}

impl RubatoSampleRateConverter {
    pub fn new(chunk_frames: usize) -> Result<Self, SampleRateConversionError> {
        if chunk_frames == 0 {
            return Err(SampleRateConversionError::ZeroChunkFrames);
        }
        Ok(Self {
            chunk_frames,
            parameters: SincInterpolationParameters::default(),
        })
    }

    /// Recreate the exact supported conversion backend from durable asset
    /// provenance. Unknown or newer recipes are refused instead of silently
    /// substituting the current defaults.
    pub fn from_materialization_recipe(
        recipe: &SampleRateMaterializationRecipe,
    ) -> Result<Self, SampleRateConversionError> {
        if recipe.backend != "rubato"
            || recipe.backend_version != RUBATO_CONVERTER_VERSION
            || recipe.algorithm != "asynchronous-windowed-sinc"
            || !recipe.delay_removed
        {
            return Err(SampleRateConversionError::UnsupportedRecipe(format!(
                "{} {} {}",
                recipe.backend, recipe.backend_version, recipe.algorithm
            )));
        }
        let interpolation = match recipe.interpolation.as_str() {
            "cubic" => SincInterpolationType::Cubic,
            "quadratic" => SincInterpolationType::Quadratic,
            "linear" => SincInterpolationType::Linear,
            "nearest" => SincInterpolationType::Nearest,
            other => {
                return Err(SampleRateConversionError::UnsupportedRecipe(format!(
                    "unknown sinc interpolation {other}"
                )))
            }
        };
        let window = match recipe.window.as_str() {
            "blackman" => WindowFunction::Blackman,
            "blackman2" => WindowFunction::Blackman2,
            "blackman-harris" => WindowFunction::BlackmanHarris,
            "blackman-harris2" => WindowFunction::BlackmanHarris2,
            "hann" => WindowFunction::Hann,
            "hann2" => WindowFunction::Hann2,
            other => {
                return Err(SampleRateConversionError::UnsupportedRecipe(format!(
                    "unknown sinc window {other}"
                )))
            }
        };
        if recipe.chunk_frames == 0 || recipe.sinc_length == 0 || recipe.oversampling_factor == 0 {
            return Err(SampleRateConversionError::UnsupportedRecipe(
                "zero-sized Rubato parameter".into(),
            ));
        }
        Ok(Self {
            chunk_frames: recipe.chunk_frames,
            parameters: SincInterpolationParameters {
                sinc_len: recipe.sinc_length,
                f_cutoff: recipe.cutoff_bits.map(f32::from_bits),
                oversampling_factor: recipe.oversampling_factor,
                interpolation,
                window,
            },
        })
    }
}

impl Default for RubatoSampleRateConverter {
    fn default() -> Self {
        Self::new(DEFAULT_RESAMPLE_CHUNK_FRAMES).expect("the default Rubato chunk size is non-zero")
    }
}

/// Number of destination frames needed to cover the source's exact rational
/// duration. Keep this calculation independent of Rubato's floating-point
/// ratio: an exactly integral conversion (for example 960 frames at 48 kHz to
/// 24 kHz) must not gain an endpoint frame because the backend rounded its
/// internal ratio slightly upward.
fn exact_resampled_frame_count(
    input_frames: u64,
    input_sample_rate_hz: u32,
    output_sample_rate_hz: u32,
) -> Result<usize, SampleRateConversionError> {
    let numerator = u128::from(input_frames) * u128::from(output_sample_rate_hz);
    let denominator = u128::from(input_sample_rate_hz);
    let frames = (numerator + denominator - 1) / denominator;
    usize::try_from(frames).map_err(|_| SampleRateConversionError::OutputTooLarge)
}

impl SampleRateConverter for RubatoSampleRateConverter {
    fn convert(
        &self,
        source: &PcmAsset,
        output_sample_rate_hz: u32,
    ) -> Result<ConvertedPcm, SampleRateConversionError> {
        if output_sample_rate_hz == 0 {
            return Err(SampleRateConversionError::ZeroOutputRate);
        }
        let input_sample_rate_hz = source.format.sample_rate.get();
        if output_sample_rate_hz == input_sample_rate_hz {
            return Err(SampleRateConversionError::RatesEqual(input_sample_rate_hz));
        }
        if source.frame_count() == 0 {
            return Err(SampleRateConversionError::EmptyInput);
        }
        if let Some(index) = source.samples.iter().position(|sample| !sample.is_finite()) {
            return Err(SampleRateConversionError::NonFiniteInput { index });
        }

        let channels = usize::from(source.format.channels.get());
        let input_frames = usize::try_from(source.frame_count())
            .map_err(|_| SampleRateConversionError::OutputTooLarge)?;
        let input = InterleavedOwned::new_from(source.samples.to_vec(), channels, input_frames)
            .map_err(|error| SampleRateConversionError::Adapter(error.to_string()))?;
        let ratio = f64::from(output_sample_rate_hz) / f64::from(input_sample_rate_hz);
        let mut resampler = Async::<f32>::new_sinc(
            ratio,
            1.0,
            &self.parameters,
            self.chunk_frames,
            channels,
            FixedAsync::Input,
        )
        .map_err(|error| SampleRateConversionError::Construction(error.to_string()))?;
        let output = resampler
            .process_all(&input, input_frames, None)
            .map_err(|error| SampleRateConversionError::Processing(error.to_string()))?;
        let mut samples = output.take_data();
        if samples.len() % channels != 0 {
            return Err(SampleRateConversionError::InvalidOutput(format!(
                "Rubato returned {} samples that do not form complete {channels}-channel frames",
                samples.len()
            )));
        }
        let produced_frames = samples.len() / channels;
        let output_frames = exact_resampled_frame_count(
            source.frame_count(),
            input_sample_rate_hz,
            output_sample_rate_hz,
        )?;
        if produced_frames < output_frames {
            return Err(SampleRateConversionError::InvalidOutput(format!(
                "Rubato returned {produced_frames} frames, fewer than the exact-duration requirement of {output_frames}"
            )));
        }
        let output_samples = output_frames
            .checked_mul(channels)
            .ok_or(SampleRateConversionError::OutputTooLarge)?;
        // `process_all` already removes the sinc startup delay and filter
        // padding. Its remaining length is calculated from a floating-point
        // ratio, though, so it can include one surplus endpoint frame when an
        // exact rational duration is integral. The project-rate contract owns
        // the duration and deterministically removes that numerical surplus.
        let trimmed_output_frames = produced_frames.saturating_sub(output_frames) as u64;
        samples.truncate(output_samples);
        if let Some(index) = samples.iter().position(|sample| !sample.is_finite()) {
            return Err(SampleRateConversionError::NonFiniteOutput { index });
        }
        let format = AudioFormat::new(output_sample_rate_hz, source.format.channels.get())
            .map_err(|error| SampleRateConversionError::InvalidOutput(error.to_string()))?;
        let pcm = PcmAsset::new(format, Arc::from(samples))
            .map_err(|error| SampleRateConversionError::InvalidOutput(error.to_string()))?;
        if pcm.frame_count() != output_frames as u64 {
            return Err(SampleRateConversionError::InvalidOutput(
                "Rubato frame count disagrees with its output buffer".into(),
            ));
        }

        Ok(ConvertedPcm {
            provenance: SampleRateConversionProvenance {
                backend: "rubato",
                backend_version: RUBATO_CONVERTER_VERSION,
                algorithm: "asynchronous-windowed-sinc",
                input_sample_rate_hz,
                output_sample_rate_hz,
                channels: source.format.channels.get(),
                input_frames: source.frame_count(),
                output_frames: pcm.frame_count(),
                chunk_frames: self.chunk_frames,
                sinc_length: self.parameters.sinc_len,
                cutoff_bits: self.parameters.f_cutoff.map(f32::to_bits),
                oversampling_factor: self.parameters.oversampling_factor,
                interpolation: interpolation_label(self.parameters.interpolation),
                window: window_label(self.parameters.window),
                delay_removed: true,
                trimmed_output_frames,
            },
            pcm,
        })
    }
}

impl SampleRateConversionProvenance {
    pub(crate) fn to_durable(&self) -> SampleRateMaterializationRecipe {
        SampleRateMaterializationRecipe {
            backend: self.backend.into(),
            backend_version: self.backend_version.into(),
            algorithm: self.algorithm.into(),
            input_sample_rate_hz: self.input_sample_rate_hz,
            output_sample_rate_hz: self.output_sample_rate_hz,
            channels: self.channels,
            input_frames: self.input_frames,
            output_frames: self.output_frames,
            chunk_frames: self.chunk_frames,
            sinc_length: self.sinc_length,
            cutoff_bits: self.cutoff_bits,
            oversampling_factor: self.oversampling_factor,
            interpolation: self.interpolation.into(),
            window: self.window.into(),
            delay_removed: self.delay_removed,
            trimmed_output_frames: self.trimmed_output_frames,
        }
    }
}

fn interpolation_label(interpolation: SincInterpolationType) -> &'static str {
    match interpolation {
        SincInterpolationType::Cubic => "cubic",
        SincInterpolationType::Quadratic => "quadratic",
        SincInterpolationType::Linear => "linear",
        SincInterpolationType::Nearest => "nearest",
    }
}

fn window_label(window: WindowFunction) -> &'static str {
    match window {
        WindowFunction::Blackman => "blackman",
        WindowFunction::Blackman2 => "blackman2",
        WindowFunction::BlackmanHarris => "blackman-harris",
        WindowFunction::BlackmanHarris2 => "blackman-harris2",
        WindowFunction::Hann => "hann",
        WindowFunction::Hann2 => "hann2",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SampleRateConversionError {
    ZeroOutputRate,
    ZeroChunkFrames,
    RatesEqual(u32),
    EmptyInput,
    NonFiniteInput { index: usize },
    NonFiniteOutput { index: usize },
    OutputTooLarge,
    Adapter(String),
    Construction(String),
    Processing(String),
    UnsupportedRecipe(String),
    InvalidOutput(String),
}

impl fmt::Display for SampleRateConversionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroOutputRate => write!(formatter, "output sample rate must not be zero"),
            Self::ZeroChunkFrames => write!(formatter, "resampler chunk size must not be zero"),
            Self::RatesEqual(rate) => write!(
                formatter,
                "explicit sample-rate conversion requires different rates; both are {rate} Hz"
            ),
            Self::EmptyInput => write!(formatter, "cannot resample empty PCM"),
            Self::NonFiniteInput { index } => {
                write!(formatter, "input PCM sample {index} is not finite")
            }
            Self::NonFiniteOutput { index } => {
                write!(formatter, "resampled PCM sample {index} is not finite")
            }
            Self::OutputTooLarge => {
                write!(formatter, "resampled PCM is too large for this process")
            }
            Self::Adapter(message) => write!(formatter, "invalid resampler buffer: {message}"),
            Self::Construction(message) => {
                write!(formatter, "resampler construction failed: {message}")
            }
            Self::Processing(message) => write!(formatter, "resampling failed: {message}"),
            Self::UnsupportedRecipe(message) => {
                write!(formatter, "unsupported resampling recipe: {message}")
            }
            Self::InvalidOutput(message) => {
                write!(formatter, "resampler produced invalid PCM: {message}")
            }
        }
    }
}

impl std::error::Error for SampleRateConversionError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelinkProposal {
    pub asset: AssetId,
    pub new_path: PathBuf,
    pub exact_fingerprint: bool,
    pub exact_metadata: bool,
    /// This is a presentation/confirmation requirement, not permission for a
    /// resolver to mutate the registry.
    pub requires_user_confirmation: bool,
}

#[derive(Clone, Debug)]
pub struct ResolvedMaterial {
    pub request: MaterialRequest,
    pub decoded: DecodedMaterial,
    pub relink: Option<RelinkProposal>,
    pub diagnostics: Vec<ResolutionDiagnostic>,
}

#[derive(Clone, Debug)]
pub struct UnresolvedMaterial {
    pub request: MaterialRequest,
    pub diagnostics: Vec<ResolutionDiagnostic>,
    /// Candidates that decoded but did not establish the registered identity.
    /// They remain visible repair leads, never an implicit relink.
    pub repair_candidates: Vec<RelinkProposal>,
}

#[derive(Clone, Debug)]
pub enum MaterialResolution {
    Resolved(ResolvedMaterial),
    Unresolved(UnresolvedMaterial),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolutionDiagnostic {
    pub path: Option<PathBuf>,
    pub code: &'static str,
    pub message: String,
}

/// Resolve one asset in deterministic candidate order: project-relative route
/// first, original absolute route second.  The only successful result has PCM
/// whose immutable metadata and fingerprint agree with the saved asset facts.
pub fn resolve_material(
    decoder: &impl MediaDecoder,
    project_manifest: &Path,
    request: MaterialRequest,
) -> MaterialResolution {
    let mut diagnostics = Vec::new();
    let mut repair_candidates = Vec::new();
    for path in request.candidates(project_manifest) {
        if !path.is_file() {
            diagnostics.push(ResolutionDiagnostic {
                path: Some(path),
                code: "route-missing",
                message: "the saved media route is not a file".into(),
            });
            continue;
        }
        let decoded = match decoder.decode(&path) {
            Ok(decoded) => decoded,
            Err(error) => {
                diagnostics.push(ResolutionDiagnostic {
                    path: Some(path),
                    code: "decode-failed",
                    message: error.to_string(),
                });
                continue;
            }
        };
        let metadata_matches = decoded.metadata == request.expected_metadata;
        let fingerprint_matches = decoded.fingerprint == request.expected_fingerprint;
        let pcm_matches = pcm_matches_metadata(&decoded.pcm, &decoded.metadata);
        if !pcm_matches {
            diagnostics.push(ResolutionDiagnostic {
                path: Some(decoded.path.clone()),
                code: "decoder-metadata-mismatch",
                message: "decoder PCM disagrees with its decoded metadata".into(),
            });
            continue;
        }
        let proposal = RelinkProposal {
            asset: request.asset,
            new_path: decoded.path.clone(),
            exact_fingerprint: fingerprint_matches,
            exact_metadata: metadata_matches,
            requires_user_confirmation: true,
        };
        if metadata_matches && fingerprint_matches {
            let relink = (!request
                .candidates(project_manifest)
                .iter()
                .any(|candidate| candidate == &decoded.path))
            .then_some(proposal);
            return MaterialResolution::Resolved(ResolvedMaterial {
                request,
                decoded,
                relink,
                diagnostics,
            });
        }
        let reason = match (fingerprint_matches, metadata_matches) {
            (false, false) => "content fingerprint and decoded metadata differ",
            (false, true) => "content fingerprint differs despite matching decoded metadata",
            (true, false) => "decoded metadata differs despite matching content fingerprint",
            (true, true) => unreachable!("the matching case returned above"),
        };
        diagnostics.push(ResolutionDiagnostic {
            path: Some(decoded.path.clone()),
            code: "candidate-identity-mismatch",
            message: reason.into(),
        });
        repair_candidates.push(proposal);
    }
    MaterialResolution::Unresolved(UnresolvedMaterial {
        request,
        diagnostics,
        repair_candidates,
    })
}

fn pcm_matches_metadata(pcm: &PcmAsset, metadata: &DecodedAudioMetadata) -> bool {
    pcm.format.sample_rate.get() == metadata.sample_rate_hz
        && pcm.format.channels.get() == metadata.channels
        && pcm.frame_count() == metadata.frame_count.0
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaDecodeError {
    UnsupportedFormat(String),
    Corrupt(String),
    Io(String),
    LimitExceeded(String),
    InvalidOutput(String),
}

impl fmt::Display for MediaDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFormat(message) => write!(formatter, "unsupported media: {message}"),
            Self::Corrupt(message) => write!(formatter, "corrupt media: {message}"),
            Self::Io(message) => write!(formatter, "media I/O failed: {message}"),
            Self::LimitExceeded(message) => write!(formatter, "media limit exceeded: {message}"),
            Self::InvalidOutput(message) => {
                write!(formatter, "decoder produced invalid PCM: {message}")
            }
        }
    }
}

impl std::error::Error for MediaDecodeError {}

/// Convert an explicit accepted repair into an `AssetLocation` input suitable
/// for `AssetRegistry::relink`.  Relative-route policy stays in the document
/// controller because it knows the package location; this helper refuses a
/// non-absolute proposed path rather than inventing one.
pub fn accepted_relink_location(
    proposal: &RelinkProposal,
) -> Result<AssetLocation, MediaDecodeError> {
    let absolute =
        crate::assets::AbsolutePath::parse(proposal.new_path.to_string_lossy().into_owned())
            .map_err(|error| MediaDecodeError::InvalidOutput(error.to_string()))?;
    AssetLocation::new(Some(absolute), None)
        .map_err(|error| MediaDecodeError::InvalidOutput(error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    fn temp_path(extension: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "audec-media-resolver-{}-{}.{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            extension
        ))
    }

    fn pcm16_wav(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        assert_eq!(samples.len() % usize::from(channels), 0);
        let data_bytes = u32::try_from(samples.len() * 2).unwrap();
        let block_align = channels * 2;
        let byte_rate = sample_rate * u32::from(block_align);
        let mut bytes = Vec::with_capacity(44 + data_bytes as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&byte_rate.to_le_bytes());
        bytes.extend_from_slice(&block_align.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_bytes.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn symphonia_wav_decode_retains_source_identity_and_recipe() {
        let path = temp_path("wav");
        let encoded = pcm16_wav(48_000, 2, &[i16::MIN, 0, i16::MAX, -16_384]);
        fs::write(&path, &encoded).unwrap();

        let decoded = SymphoniaMediaDecoder::default()
            .decode_provenanced(&path)
            .unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(decoded.decoded.path, path);
        assert_eq!(
            decoded.decoded.fingerprint,
            ContentFingerprint::from_bytes(&encoded)
        );
        assert_eq!(decoded.decoded.metadata.sample_rate_hz, 48_000);
        assert_eq!(decoded.decoded.metadata.channels, 2);
        assert_eq!(decoded.decoded.metadata.frame_count, SampleFrames(2));
        assert_eq!(decoded.decoded.metadata.container.as_deref(), Some("wav"));
        assert_eq!(decoded.decoded.metadata.codec.as_deref(), Some("pcm_s16le"));
        assert_eq!(decoded.decoded.metadata.bit_depth, Some(16));
        assert_eq!(decoded.decoded.pcm.frame_count(), 2);
        assert_eq!(decoded.decoded.pcm.samples[0], -1.0);
        assert_eq!(decoded.decoded.pcm.samples[1], 0.0);
        assert!((decoded.decoded.pcm.samples[2] - 32_767.0 / 32_768.0).abs() < 1.0e-6);
        assert_eq!(decoded.decoded.pcm.samples[3], -0.5);
        assert_eq!(decoded.provenance.backend, "symphonia");
        assert_eq!(
            decoded.provenance.backend_version,
            SYMPHONIA_DECODER_VERSION
        );
        assert_eq!(decoded.provenance.source_bytes, encoded.len() as u64);
        assert_eq!(decoded.provenance.stream_count, 1);
        assert_eq!(decoded.provenance.container.as_deref(), Some("wav"));
        assert_eq!(decoded.provenance.codec, "pcm_s16le");
        assert!(decoded.provenance.gapless);
    }

    #[test]
    fn symphonia_decoder_refuses_source_limit_and_truncated_stream() {
        let path = temp_path("wav");
        let encoded = pcm16_wav(8_000, 1, &[0, 1, 2, 3]);
        fs::write(&path, &encoded).unwrap();
        let limited = SymphoniaMediaDecoder::new(16, 1_000).unwrap();
        assert!(matches!(
            limited.decode(&path),
            Err(MediaDecodeError::LimitExceeded(_))
        ));

        fs::write(&path, &encoded[..42]).unwrap();
        let error = SymphoniaMediaDecoder::default().decode(&path).unwrap_err();
        let _ = fs::remove_file(&path);
        assert!(matches!(
            &error,
            MediaDecodeError::Corrupt(_) | MediaDecodeError::UnsupportedFormat(_)
        ));
        assert!(error.to_string().contains("media"));
    }

    fn stereo_pcm(sample_rate: u32, frames: usize) -> PcmAsset {
        let mut samples = Vec::with_capacity(frames * 2);
        for frame in 0..frames {
            let sample = (TAU * 440.0 * frame as f32 / sample_rate as f32).sin() * 0.5;
            samples.extend([sample, -sample]);
        }
        PcmAsset::new(
            AudioFormat::new(sample_rate, 2).unwrap(),
            Arc::from(samples),
        )
        .unwrap()
    }

    #[test]
    fn rubato_conversion_is_explicit_deterministic_and_preserves_channels() {
        let source = stereo_pcm(44_100, 441);
        let converter = RubatoSampleRateConverter::default();
        let first = converter.convert(&source, 48_000).unwrap();
        let second = converter.convert(&source, 48_000).unwrap();

        assert_eq!(first.pcm.format.sample_rate.get(), 48_000);
        assert_eq!(first.pcm.format.channels.get(), 2);
        assert_eq!(first.pcm.frame_count(), 480);
        assert_eq!(first.pcm.samples.as_ref(), second.pcm.samples.as_ref());
        assert!(first.pcm.samples.iter().all(|sample| sample.is_finite()));
        assert!(first
            .pcm
            .samples
            .chunks_exact(2)
            .all(|frame| (frame[0] + frame[1]).abs() < 1.0e-6));
        assert_eq!(first.provenance.backend, "rubato");
        assert_eq!(first.provenance.backend_version, RUBATO_CONVERTER_VERSION);
        assert_eq!(first.provenance.algorithm, "asynchronous-windowed-sinc");
        assert_eq!(first.provenance.input_frames, 441);
        assert_eq!(first.provenance.output_frames, 480);
        assert_eq!(first.provenance.channels, 2);
    }

    #[test]
    fn rubato_conversion_uses_exact_rational_duration_for_downsampling() {
        let converter = RubatoSampleRateConverter::default();

        let integral = converter.convert(&stereo_pcm(48_000, 960), 24_000).unwrap();
        assert_eq!(integral.pcm.frame_count(), 480);
        assert_eq!(integral.pcm.samples.len(), 480 * 2);
        assert_eq!(integral.provenance.output_frames, 480);

        // A partial destination-frame duration is retained, but the frame
        // count is still derived using integer rational arithmetic.
        let fractional = converter.convert(&stereo_pcm(48_000, 2), 32_000).unwrap();
        assert_eq!(fractional.pcm.frame_count(), 2);
        assert_eq!(fractional.pcm.samples.len(), 2 * 2);
        assert_eq!(fractional.provenance.output_frames, 2);
    }

    #[test]
    fn project_rate_material_keeps_original_provenance_and_skips_equal_rate() {
        let source = stereo_pcm(44_100, 441);
        let source_samples = Arc::clone(&source.samples);
        let encoded = b"original encoded source";
        let decoded = ProvenancedDecodedMaterial {
            decoded: DecodedMaterial {
                path: PathBuf::from("/original/source.wav"),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: 44_100,
                    channels: 2,
                    frame_count: SampleFrames(441),
                    container: Some("wav".into()),
                    codec: Some("pcm".into()),
                    bit_depth: Some(16),
                },
                fingerprint: ContentFingerprint::from_bytes(encoded),
                pcm: source,
            },
            provenance: MediaDecodeProvenance {
                backend: "symphonia",
                backend_version: SYMPHONIA_DECODER_VERSION,
                source_bytes: encoded.len() as u64,
                stream_count: 1,
                selected_track_id: 0,
                container: Some("wav".into()),
                codec: "pcm".into(),
                declared_frames: Some(441),
                gapless: true,
                verification: DecodeVerification::Unavailable,
            },
        };
        let converter = RubatoSampleRateConverter::default();

        let native = decoded.pcm_for_project_rate(44_100, &converter).unwrap();
        assert!(native.conversion.is_none());
        assert!(Arc::ptr_eq(&native.pcm.samples, &source_samples));
        assert_eq!(native.source_metadata.sample_rate_hz, 44_100);
        assert_eq!(
            native.source_fingerprint,
            ContentFingerprint::from_bytes(encoded)
        );

        let converted = decoded.pcm_for_project_rate(48_000, &converter).unwrap();
        assert_eq!(converted.pcm.format.sample_rate.get(), 48_000);
        assert_eq!(converted.source_metadata.sample_rate_hz, 44_100);
        assert_eq!(converted.decode_provenance.backend, "symphonia");
        assert_eq!(converted.conversion.unwrap().input_sample_rate_hz, 44_100);
    }

    #[test]
    fn imported_asset_identifies_project_pcm_and_retains_reopen_recipe() {
        let source = stereo_pcm(44_100, 441);
        let encoded = b"encoded source snapshot";
        let decoded = ProvenancedDecodedMaterial {
            decoded: DecodedMaterial {
                path: PathBuf::from("/original/source.wav"),
                metadata: DecodedAudioMetadata {
                    sample_rate_hz: 44_100,
                    channels: 2,
                    frame_count: SampleFrames(441),
                    container: Some("wav".into()),
                    codec: Some("pcm_s16le".into()),
                    bit_depth: Some(16),
                },
                fingerprint: ContentFingerprint::from_bytes(encoded),
                pcm: source,
            },
            provenance: MediaDecodeProvenance {
                backend: "symphonia",
                backend_version: SYMPHONIA_DECODER_VERSION,
                source_bytes: encoded.len() as u64,
                stream_count: 1,
                selected_track_id: 0,
                container: Some("wav".into()),
                codec: "pcm_s16le".into(),
                declared_frames: Some(441),
                gapless: true,
                verification: DecodeVerification::Unavailable,
            },
        };
        let imported = decoded
            .materialize_import(
                "Source",
                48_000,
                &RubatoSampleRateConverter::default(),
                17,
                "audec test",
                BTreeSet::from(["imported".into()]),
                false,
            )
            .unwrap();

        assert_eq!(imported.registration.metadata.sample_rate_hz, 48_000);
        assert_eq!(
            imported.registration.metadata.frame_count,
            SampleFrames(480)
        );
        assert_eq!(imported.pcm.frame_count(), 480);
        let provenance = imported
            .registration
            .provenance
            .materialization()
            .expect("filesystem imports retain source identity");
        assert_eq!(provenance.source_metadata.sample_rate_hz, 44_100);
        assert_eq!(
            provenance.source_content,
            ContentFingerprint::from_bytes(encoded)
        );
        let recipe = provenance.sample_rate.as_ref().unwrap();
        let recreated = RubatoSampleRateConverter::from_materialization_recipe(recipe).unwrap();
        let output = recreated
            .convert(&decoded.decoded.pcm, recipe.output_sample_rate_hz)
            .unwrap();
        assert_eq!(output.provenance.to_durable(), *recipe);
        assert_eq!(output.pcm.samples.as_ref(), imported.pcm.samples.as_ref());
        assert_eq!(
            canonical_pcm_identity(DecodedPcmView::from_pcm_asset(&output.pcm))
                .unwrap()
                .fingerprint,
            imported.registration.content
        );
    }

    #[test]
    fn packet_stream_hydrates_only_demanded_chunks_and_preserves_slice_boundaries() {
        use crate::pyramid::StreamingWaveformIndex;
        use crate::streaming_media::{
            BoundedMediaStore, CacheBudgets, DecodeRequest, FrameSpan, RequestPriority,
            VirtualSliceRef,
        };

        let path = temp_path("wav");
        let source_values = [
            -30_000, -20_000, -10_000, -1_000, 0, 1_000, 10_000, 20_000, 30_000, 12_345,
        ];
        let encoded = pcm16_wav(8_000, 1, &source_values);
        fs::write(&path, &encoded).unwrap();
        let decoder = SymphoniaMediaDecoder::default();
        let whole = decoder.decode_provenanced(&path).unwrap();
        let project_identity =
            canonical_pcm_identity(DecodedPcmView::from_pcm_asset(&whole.decoded.pcm)).unwrap();
        let request = StreamingMaterialRequest {
            asset: AssetId(9),
            path: AssetPathIntent {
                project_relative: None,
                original_absolute: Some(path.clone()),
            },
            source_metadata: whole.decoded.metadata.clone(),
            source_fingerprint: whole.decoded.fingerprint,
            project_metadata: whole.decoded.metadata.clone(),
            project_pcm_fingerprint: project_identity.fingerprint,
        };
        let source = decoder
            .open_project_rate_chunk_source(
                &path.with_extension("audec"),
                &request,
                8_000,
                4,
                2,
                &RubatoSampleRateConverter::default(),
            )
            .unwrap();
        assert_eq!(source.mode(), ProjectRateChunkMode::PacketStream);
        assert!(source.diagnostic().is_none());

        let descriptor = source.descriptor();
        let mut store = BoundedMediaStore::new(CacheBudgets {
            memory_bytes: 1_000_000,
            disk_bytes: 1_000_000,
        })
        .unwrap();
        let mut waveforms = StreamingWaveformIndex::new(descriptor);
        let demand = [0, 1].map(|index| DecodeRequest {
            key: descriptor.chunk_key(PcmChunkIndex(index)).unwrap(),
            priority: RequestPriority::Visible,
            distance_chunks: 0,
            demand_epoch: 3,
        });
        assert_eq!(
            source
                .hydrate_requests(demand, &mut store, &mut waveforms)
                .unwrap(),
            ChunkHydrationReport {
                decoded: 2,
                reused: 0
            }
        );
        assert!(!store.contains_resident(descriptor.chunk_key(PcmChunkIndex(2)).unwrap()));
        assert_eq!(waveforms.product_count(), 2);

        let slice = VirtualSliceRef::new(descriptor, FrameSpan::new(3, 7).unwrap()).unwrap();
        let actual = store.read_slice(slice, 0, 4).unwrap();
        assert_eq!(actual, whole.decoded.pcm.samples[3..7]);

        let query = waveforms
            .query_exact(1, 7, 2, |start, end| {
                let slice =
                    VirtualSliceRef::new(descriptor, FrameSpan::new(start, end).unwrap()).unwrap();
                store
                    .read_slice(slice, 0, end - start)
                    .map_err(StreamingWaveformError::Streaming)
            })
            .unwrap();
        assert_eq!(
            query
                .bins
                .iter()
                .map(|bin| (bin.start_frame, bin.end_frame))
                .collect::<Vec<_>>(),
            vec![(1, 4), (4, 7)]
        );

        let graph_asset = ArrangementAssetId::from_raw(77);
        let mut graph_media = StreamingGraphMediaSource::new(CacheBudgets {
            memory_bytes: 1_000_000,
            disk_bytes: 1_000_000,
        })
        .unwrap();
        graph_media.register(graph_asset, source.clone()).unwrap();
        let first_snapshot = graph_media
            .prepare(&[MediaBlockDemand::new(graph_asset, 3, 7).unwrap()])
            .unwrap();
        assert_eq!(
            first_snapshot.descriptor(graph_asset),
            Some(MediaAssetDescriptor {
                format: AudioFormat::new(8_000, 1).unwrap(),
                frame_count: 10,
            })
        );
        for frame in 3..7 {
            assert_eq!(
                first_snapshot.sample(graph_asset, frame, 0).unwrap(),
                whole.decoded.pcm.samples[frame as usize]
            );
        }
        assert_eq!(
            first_snapshot
                .sample(graph_asset, 8, 0)
                .unwrap_err()
                .failure,
            MediaReadFailure::FrameUnavailable
        );
        assert_eq!(
            graph_media.waveform(graph_asset).unwrap().product_count(),
            2
        );

        // A new playback window publishes a new bounded view without
        // invalidating an already-published callback snapshot.
        let second_snapshot = graph_media
            .prepare(&[MediaBlockDemand::new(graph_asset, 8, 10).unwrap()])
            .unwrap();
        assert_eq!(
            second_snapshot.sample(graph_asset, 8, 0).unwrap(),
            whole.decoded.pcm.samples[8]
        );
        assert_eq!(
            second_snapshot
                .sample(graph_asset, 7, 0)
                .unwrap_err()
                .failure,
            MediaReadFailure::FrameUnavailable
        );
        assert_eq!(
            first_snapshot.sample(graph_asset, 3, 0).unwrap(),
            whole.decoded.pcm.samples[3]
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn locating_a_path_does_not_accept_an_identity_mismatched_relink() {
        let path = temp_path("bin");
        fs::write(&path, b"observed bytes").unwrap();
        let located = locate_material_route(
            &path.with_extension("audec"),
            &AssetPathIntent {
                project_relative: None,
                original_absolute: Some(path.clone()),
            },
        )
        .unwrap();
        let error = verify_material_route(
            located.clone(),
            ContentFingerprint::from_bytes(b"different bytes"),
            1_024,
        )
        .unwrap_err();
        let StreamingOpenError::IdentityMismatch(mismatch) = error else {
            panic!("identity mismatch must remain explicit");
        };
        assert_eq!(mismatch.located, located);
        assert!(path.is_file());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn resampler_refuses_nonfinite_input_and_equal_rate_calls() {
        let converter = RubatoSampleRateConverter::default();
        let invalid =
            PcmAsset::new(AudioFormat::new(44_100, 1).unwrap(), Arc::from([f32::NAN])).unwrap();
        assert_eq!(
            converter.convert(&invalid, 48_000).unwrap_err(),
            SampleRateConversionError::NonFiniteInput { index: 0 }
        );

        let valid = stereo_pcm(44_100, 16);
        assert_eq!(
            converter.convert(&valid, 44_100).unwrap_err(),
            SampleRateConversionError::RatesEqual(44_100)
        );
    }
}
