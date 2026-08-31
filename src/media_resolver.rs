//! Explicit source and derived-media resolution for an opened project.
//!
//! A project package stores route *intent*, not an assertion that its media is
//! present.  This module turns that intent into decoder requests and detailed
//! repair diagnostics.  It never mutates `AssetRegistry`: a UI/controller must
//! explicitly accept a [`RelinkProposal`] before calling `AssetRegistry::relink`.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::assets::{
    AssetFrameRange, AssetId, AssetLocation, ContentFingerprint, DecodedAudioMetadata,
};
use crate::daw_render::PcmAsset;
use crate::project_io::AssetPathIntent;

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

/// Deliberately small async-agnostic seam.  The application can invoke this
/// on a worker, and test decoders can remain deterministic without a window or
/// a particular file-format dependency.
pub trait MediaDecoder {
    fn decode(&self, path: &Path) -> Result<DecodedMaterial, MediaDecodeError>;
}

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
    InvalidOutput(String),
}

impl fmt::Display for MediaDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFormat(message) => write!(formatter, "unsupported media: {message}"),
            Self::Corrupt(message) => write!(formatter, "corrupt media: {message}"),
            Self::Io(message) => write!(formatter, "media I/O failed: {message}"),
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
