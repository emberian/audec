//! Product-object navigation over the durable workspace document.
//!
//! This service translates typed musical, material, and explanation objects
//! into workspace actions. It does not own GPUI entities, mutate project
//! truth, infer identities from labels, or silently discard objects whose
//! working surface has not landed yet. Receipt adapters retain every related
//! object so a reveal can populate selection, Inspector, and breadcrumbs from
//! the same result.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::constructive_controller::{ConstructivePublication, ConstructivePublishedFocus};
use crate::arrangement::{ClipId, TrackId};
use crate::artifact_catalog::{ArtifactId, DigestAlgorithm};
use crate::assets::AssetId;
use crate::automation::AutomationLaneId;
use crate::comparison::ComparisonId;
use crate::explanation::ExplanationId;
use crate::mixer::BusId;
use crate::reading::ReadingId;
use crate::reconstruction::ReconstructionProposalId;
use crate::reconstruction_apply::ReconstructionApplicationReceipt;
use crate::sample_actions::{
    SamplePublishedResult, SampleResultFocus, SampleResultProvenance, SamplerTarget,
    SamplerViewDisposition,
};
use crate::sample_kit::{KitId, PadId, ZoneId};
use crate::sample_material::{DerivationScope, SourceMaterialRef};
use crate::sequencer::{PatternClipId, PatternId, PPQ};
use crate::workspace_document::{
    AnalysisLensKind, BeatViewport, EditorTarget, EditorViewState, FrameViewport, LinkFacets,
    LinkGroupId, NewWorkspaceView, PatternEditorMode, ViewLinkMembership, ViewLocation,
    WorkspaceDocument, WorkspaceItemKind, WorkspaceViewDescriptor, WorkspaceViewId,
};

const NAVIGATION_SCOPE: &str = "audec.navigation_scope";
const NAVIGATION_OBJECT: &str = "audec.navigation_object";
const EXTENSION_NAMESPACE: &str = "audec";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InstrumentRef {
    SampleKit(KitId),
}

impl InstrumentRef {
    pub const fn kit(self) -> KitId {
        match self {
            Self::SampleKit(kit) => kit,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PadRef {
    pub kit: KitId,
    pub pad: PadId,
    pub zone: Option<ZoneId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PatternOccurrenceRef {
    pub arrangement_clip: ClipId,
    pub sequencer_clip: Option<PatternClipId>,
    pub pattern: Option<PatternId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FindingKind {
    Rhythm,
    Components,
    Separation,
    Loom,
    ModelClaim,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FindingScope {
    Artifact(ArtifactId),
    Derivation(DerivationScope),
    /// Some legacy application receipts retain a project publication and
    /// source asset but not the analysis content scope. The pair remains an
    /// honest scope; it must not be rewritten as a content-addressed finding.
    ProjectPublication {
        revision: u64,
        source: AssetId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FindingLocalId {
    ReconstructionProposal(ReconstructionProposalId),
    Claim(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FindingRef {
    pub kind: FindingKind,
    pub scope: FindingScope,
    pub local: FindingLocalId,
}

/// A product-level identity. Equal integers in different variants remain
/// different objects, and analyzer-local IDs always carry an explicit scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ObjectRef {
    Material(AssetId),
    Sample(SourceMaterialRef),
    Instrument(InstrumentRef),
    Pad(PadRef),
    Pattern(PatternId),
    PatternOccurrence(PatternOccurrenceRef),
    AudioClip(ClipId),
    Track(TrackId),
    Bus(BusId),
    Automation(AutomationLaneId),
    Finding(FindingRef),
    Explanation(ExplanationId),
    Comparison(ComparisonId),
    Reading(ReadingId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    Material,
    Sample,
    Instrument,
    Pad,
    Pattern,
    PatternOccurrence,
    AudioClip,
    Track,
    Bus,
    Automation,
    Finding,
    Explanation,
    Comparison,
    Reading,
}

impl ObjectRef {
    pub const fn kind(&self) -> ObjectKind {
        match self {
            Self::Material(_) => ObjectKind::Material,
            Self::Sample(_) => ObjectKind::Sample,
            Self::Instrument(_) => ObjectKind::Instrument,
            Self::Pad(_) => ObjectKind::Pad,
            Self::Pattern(_) => ObjectKind::Pattern,
            Self::PatternOccurrence(_) => ObjectKind::PatternOccurrence,
            Self::AudioClip(_) => ObjectKind::AudioClip,
            Self::Track(_) => ObjectKind::Track,
            Self::Bus(_) => ObjectKind::Bus,
            Self::Automation(_) => ObjectKind::Automation,
            Self::Finding(_) => ObjectKind::Finding,
            Self::Explanation(_) => ObjectKind::Explanation,
            Self::Comparison(_) => ObjectKind::Comparison,
            Self::Reading(_) => ObjectKind::Reading,
        }
    }

    /// Stable address used only at the durable workspace boundary. Parsing
    /// this string never substitutes for the typed value retained in plans.
    pub fn address(&self) -> String {
        match self {
            Self::Material(asset) => format!("material:{}", asset.0),
            Self::Sample(SourceMaterialRef::Asset(asset)) => {
                format!("sample:asset:{}", asset.0)
            }
            Self::Sample(SourceMaterialRef::VirtualSlice(slice)) => format!(
                "sample:slice:{}:{}:{}",
                slice.source_asset.0, slice.source_range.start.0, slice.source_range.end.0
            ),
            Self::Instrument(instrument) => format!("instrument:kit:{}", instrument.kit().get()),
            Self::Pad(pad) => format!(
                "pad:kit:{}:pad:{}:zone:{}",
                pad.kit.get(),
                pad.pad.get(),
                pad.zone.map_or(0, ZoneId::get)
            ),
            Self::Pattern(pattern) => format!("pattern:{}", pattern.get()),
            Self::PatternOccurrence(occurrence) => format!(
                "pattern-occurrence:{}:sequencer:{}:pattern:{}",
                occurrence.arrangement_clip.get(),
                occurrence.sequencer_clip.map_or(0, PatternClipId::get),
                occurrence.pattern.map_or(0, PatternId::get)
            ),
            Self::AudioClip(clip) => format!("audio-clip:{}", clip.get()),
            Self::Track(track) => format!("track:{}", track.get()),
            Self::Bus(bus) => format!("bus:{}", bus.get()),
            Self::Automation(lane) => format!("automation:{}", lane.get()),
            Self::Finding(finding) => finding_address(*finding),
            Self::Explanation(explanation) => format!("explanation:{}", explanation.0),
            Self::Comparison(comparison) => format!("comparison:{}", comparison.0),
            Self::Reading(reading) => format!("reading:{reading}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevealIntent {
    ActivateExisting,
    OpenNew,
    RetargetCurrent,
    ShowInspector,
    SelectOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevealRequest {
    pub object: ObjectRef,
    pub intent: RevealIntent,
    /// Revision that proved this object existed when the reveal originated in
    /// a publication receipt. Apply those requests with
    /// [`ObjectNavigator::plan_at_revision`] after later undo/import activity.
    pub expected_project_revision: Option<u64>,
    pub current_view: Option<WorkspaceViewId>,
    pub related: Vec<ObjectRef>,
}

impl RevealRequest {
    pub fn new(object: ObjectRef, intent: RevealIntent) -> Self {
        Self {
            object,
            intent,
            expected_project_revision: None,
            current_view: None,
            related: Vec::new(),
        }
    }

    pub const fn at_revision(mut self, revision: u64) -> Self {
        self.expected_project_revision = Some(revision);
        self
    }

    pub fn with_current_view(mut self, view: WorkspaceViewId) -> Self {
        self.current_view = Some(view);
        self
    }

    pub fn with_related(mut self, related: impl IntoIterator<Item = ObjectRef>) -> Self {
        self.related = deduplicate_related(&self.object, related);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetMultiplicity {
    SingletonBySurface,
    SingletonByTarget,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceReveal {
    Activate {
        view: WorkspaceViewId,
        location: ViewLocation,
    },
    Create(NewWorkspaceView),
    Retarget {
        descriptor: WorkspaceViewDescriptor,
        location: ViewLocation,
    },
    None,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionConsequence {
    pub primary: ObjectRef,
    pub related: Vec<ObjectRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectorVisibility {
    UpdateIfVisible,
    Reveal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectorConsequence {
    pub target: ObjectRef,
    pub visibility: InspectorVisibility,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevealDiagnosticCode {
    InvalidObject,
    StalePublication,
    UnsupportedMapping,
    MissingCurrentView,
    CurrentViewMissing,
    IncompatibleRetarget,
    SingletonReused,
    UntargetedViewRetargeted,
    ReceiptRequestedStay,
    ReceiptHadMultipleCandidates,
    ReceiptUsedPublicationScope,
    ReceiptHadNoConstruction,
    EphemeralSamplerTarget,
    ProvenanceHasNoMaterialIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevealDiagnostic {
    pub code: RevealDiagnosticCode,
    pub message: String,
}

impl RevealDiagnostic {
    fn new(code: RevealDiagnosticCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RevealPlan {
    pub workspace: WorkspaceReveal,
    pub selection: SelectionConsequence,
    pub inspector: InspectorConsequence,
    pub diagnostics: Vec<RevealDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevealRecommendation {
    pub request: RevealRequest,
    pub diagnostics: Vec<RevealDiagnostic>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ObjectNavigator;

impl ObjectNavigator {
    pub fn plan(document: &WorkspaceDocument, request: RevealRequest) -> RevealPlan {
        Self::plan_inner(document, request)
    }

    /// Reject a receipt-backed reveal after its creating publication has been
    /// undone or superseded. Direct UI selections have no revision pin and
    /// continue to plan normally.
    pub fn plan_at_revision(
        document: &WorkspaceDocument,
        current_project_revision: u64,
        request: RevealRequest,
    ) -> RevealPlan {
        if let Some(expected) = request.expected_project_revision {
            if expected != current_project_revision {
                return RevealPlan {
                    workspace: WorkspaceReveal::Unsupported,
                    selection: SelectionConsequence {
                        primary: request.object.clone(),
                        related: deduplicate_related(&request.object, request.related.clone()),
                    },
                    inspector: InspectorConsequence {
                        target: request.object,
                        visibility: InspectorVisibility::UpdateIfVisible,
                    },
                    diagnostics: vec![RevealDiagnostic::new(
                        RevealDiagnosticCode::StalePublication,
                        format!(
                            "object was published at project revision {expected}, but the current revision is {current_project_revision}"
                        ),
                    )],
                };
            }
        }
        Self::plan_inner(document, request)
    }

    fn plan_inner(document: &WorkspaceDocument, request: RevealRequest) -> RevealPlan {
        let mut diagnostics = Vec::new();
        let selection = SelectionConsequence {
            primary: request.object.clone(),
            related: deduplicate_related(&request.object, request.related.clone()),
        };
        let inspector = InspectorConsequence {
            target: request.object.clone(),
            visibility: if request.intent == RevealIntent::ShowInspector {
                InspectorVisibility::Reveal
            } else {
                InspectorVisibility::UpdateIfVisible
            },
        };
        if !valid_object(&request.object) {
            diagnostics.push(RevealDiagnostic::new(
                RevealDiagnosticCode::InvalidObject,
                format!("invalid {:?} product identity", request.object.kind()),
            ));
            return RevealPlan {
                workspace: WorkspaceReveal::Unsupported,
                selection,
                inspector,
                diagnostics,
            };
        }
        if request.intent == RevealIntent::SelectOnly {
            return RevealPlan {
                workspace: WorkspaceReveal::None,
                selection,
                inspector,
                diagnostics,
            };
        }
        let surface = if request.intent == RevealIntent::ShowInspector {
            inspector_surface()
        } else if let Some(surface) = surface_for(&request.object) {
            surface
        } else {
            diagnostics.push(RevealDiagnostic::new(
                RevealDiagnosticCode::UnsupportedMapping,
                format!(
                    "no workspace surface is registered for {:?}",
                    request.object.kind()
                ),
            ));
            return RevealPlan {
                workspace: WorkspaceReveal::Unsupported,
                selection,
                inspector,
                diagnostics,
            };
        };

        let workspace = match request.intent {
            RevealIntent::RetargetCurrent => {
                plan_retarget_or_fallback(document, &request, &surface, &mut diagnostics)
            }
            RevealIntent::OpenNew => {
                if surface.multiplicity == TargetMultiplicity::SingletonBySurface {
                    if let Some(view) = find_surface(document, &surface) {
                        diagnostics.push(RevealDiagnostic::new(
                            RevealDiagnosticCode::SingletonReused,
                            "this workspace surface is a project singleton; activated its existing view",
                        ));
                        activate(document, view)
                    } else {
                        WorkspaceReveal::Create(surface.new_view())
                    }
                } else {
                    WorkspaceReveal::Create(surface.new_view())
                }
            }
            RevealIntent::ActivateExisting | RevealIntent::ShowInspector => {
                plan_activate_or_create(document, &surface, &mut diagnostics)
            }
            RevealIntent::SelectOnly => WorkspaceReveal::None,
        };
        RevealPlan {
            workspace,
            selection,
            inspector,
            diagnostics,
        }
    }
}

/// Translate an aggregate constructive receipt before any view-specific
/// result type discards arrangement focus.
pub fn recommend_constructive(publication: &ConstructivePublication) -> RevealRecommendation {
    let kit = ObjectRef::Instrument(InstrumentRef::SampleKit(publication.kit));
    let pad = publication.pad.map(|pad| {
        ObjectRef::Pad(PadRef {
            kit: publication.kit,
            pad,
            zone: None,
        })
    });
    let pattern = publication.pattern.map(ObjectRef::Pattern);
    let occurrence = publication.arrangement_clip.map(|arrangement_clip| {
        ObjectRef::PatternOccurrence(PatternOccurrenceRef {
            arrangement_clip,
            sequencer_clip: None,
            pattern: publication.pattern,
        })
    });
    let mut diagnostics = Vec::new();
    let (object, intent) = match publication.focus {
        ConstructivePublishedFocus::Stay => {
            diagnostics.push(RevealDiagnostic::new(
                RevealDiagnosticCode::ReceiptRequestedStay,
                "constructive result requested no editor activation; selected its most specific durable object",
            ));
            (
                occurrence
                    .clone()
                    .or_else(|| pattern.clone())
                    .or_else(|| pad.clone())
                    .unwrap_or_else(|| kit.clone()),
                RevealIntent::SelectOnly,
            )
        }
        ConstructivePublishedFocus::Kit(kit) => (
            ObjectRef::Instrument(InstrumentRef::SampleKit(kit)),
            RevealIntent::ActivateExisting,
        ),
        ConstructivePublishedFocus::Pad { kit, pad } => (
            ObjectRef::Pad(PadRef {
                kit,
                pad,
                zone: None,
            }),
            RevealIntent::ActivateExisting,
        ),
        ConstructivePublishedFocus::Pattern(pattern) => {
            (ObjectRef::Pattern(pattern), RevealIntent::ActivateExisting)
        }
        ConstructivePublishedFocus::Arrangement(arrangement_clip) => (
            ObjectRef::PatternOccurrence(PatternOccurrenceRef {
                arrangement_clip,
                sequencer_clip: None,
                pattern: publication.pattern,
            }),
            RevealIntent::ActivateExisting,
        ),
        ConstructivePublishedFocus::Sampler { kit, disposition } => (
            ObjectRef::Instrument(InstrumentRef::SampleKit(kit)),
            disposition_intent(disposition),
        ),
    };
    let related = [Some(kit), pad, pattern, occurrence].into_iter().flatten();
    RevealRecommendation {
        request: RevealRequest::new(object, intent)
            .at_revision(publication.revision)
            .with_related(related),
        diagnostics,
    }
}

pub fn request_from_sample_focus(
    focus: SampleResultFocus,
    published_kit: KitId,
    published_pad: Option<PadId>,
    published_pattern: Option<PatternId>,
) -> RevealRecommendation {
    let mut diagnostics = Vec::new();
    let fallback = || {
        published_pad.map_or(
            ObjectRef::Instrument(InstrumentRef::SampleKit(published_kit)),
            |pad| {
                ObjectRef::Pad(PadRef {
                    kit: published_kit,
                    pad,
                    zone: None,
                })
            },
        )
    };
    let (object, intent) = match focus {
        SampleResultFocus::Stay => {
            diagnostics.push(RevealDiagnostic::new(
                RevealDiagnosticCode::ReceiptRequestedStay,
                "sample result requested no editor activation",
            ));
            (
                published_pattern.map_or_else(fallback, ObjectRef::Pattern),
                RevealIntent::SelectOnly,
            )
        }
        SampleResultFocus::Kit(kit) => (
            ObjectRef::Instrument(InstrumentRef::SampleKit(kit)),
            RevealIntent::ActivateExisting,
        ),
        SampleResultFocus::Pad { kit, pad } => (
            ObjectRef::Pad(PadRef {
                kit,
                pad,
                zone: None,
            }),
            RevealIntent::ActivateExisting,
        ),
        SampleResultFocus::Pattern(pattern) => {
            (ObjectRef::Pattern(pattern), RevealIntent::ActivateExisting)
        }
        SampleResultFocus::Sampler {
            target,
            disposition,
        } => match sampler_object(target) {
            Some(object) => (object, disposition_intent(disposition)),
            None => {
                diagnostics.push(RevealDiagnostic::new(
                    RevealDiagnosticCode::EphemeralSamplerTarget,
                    "a published result cannot reveal a still-unallocated NewKit/NewPad target; used the receipt identity",
                ));
                (fallback(), disposition_intent(disposition))
            }
        },
    };
    let related = [
        Some(ObjectRef::Instrument(InstrumentRef::SampleKit(
            published_kit,
        ))),
        published_pad.map(|pad| {
            ObjectRef::Pad(PadRef {
                kit: published_kit,
                pad,
                zone: None,
            })
        }),
        published_pattern.map(ObjectRef::Pattern),
    ]
    .into_iter()
    .flatten();
    RevealRecommendation {
        request: RevealRequest::new(object, intent).with_related(related),
        diagnostics,
    }
}

pub fn recommend_sample_result(result: &SamplePublishedResult) -> RevealRecommendation {
    let mut recommendation =
        request_from_sample_focus(result.focus, result.kit, result.pad, result.pattern);
    recommendation.request.expected_project_revision = Some(result.revision);
    if let Some(provenance) = &result.provenance {
        match provenance {
            SampleResultProvenance::Material(material) => {
                recommendation
                    .request
                    .related
                    .push(ObjectRef::Sample(*material));
            }
            SampleResultProvenance::Selection { source, .. } => {
                recommendation
                    .request
                    .related
                    .push(ObjectRef::Sample(source.material()));
            }
            SampleResultProvenance::Authored(_) => recommendation.diagnostics.push(
                RevealDiagnostic::new(
                    RevealDiagnosticCode::ProvenanceHasNoMaterialIdentity,
                    "authored provenance was retained, but it does not itself identify a source material",
                ),
            ),
        }
    }
    recommendation.request.related = deduplicate_related(
        &recommendation.request.object,
        recommendation.request.related,
    );
    recommendation
}

/// Select the most directly editable construction in an older reconstruction
/// receipt. Multiple candidates are all retained as related objects and the
/// deterministic choice is diagnosed rather than hidden.
pub fn recommend_reconstruction(
    receipt: &ReconstructionApplicationReceipt,
) -> RevealRecommendation {
    let finding = ObjectRef::Finding(FindingRef {
        kind: FindingKind::Other,
        scope: FindingScope::ProjectPublication {
            revision: receipt.project_revision,
            source: receipt.bindings.source_asset,
        },
        local: FindingLocalId::ReconstructionProposal(receipt.bindings.proposal),
    });
    let mut candidates = Vec::new();
    candidates.extend(
        receipt
            .bindings
            .patterns
            .values()
            .map(|binding| ObjectRef::Pattern(binding.sequencer_pattern)),
    );
    candidates.extend(
        receipt
            .bindings
            .automations
            .values()
            .map(|binding| ObjectRef::Automation(binding.lane)),
    );
    candidates.extend(
        receipt
            .bindings
            .triggers
            .values()
            .map(|binding| ObjectRef::AudioClip(binding.audio_clip)),
    );
    if let Some(residual) = &receipt.bindings.residual {
        candidates.push(ObjectRef::AudioClip(residual.audio_clip));
    }
    candidates.extend(
        receipt
            .bindings
            .tracks
            .values()
            .map(|binding| ObjectRef::Track(binding.arrangement_track)),
    );
    let mut related = vec![finding, ObjectRef::Material(receipt.bindings.source_asset)];
    related.extend(candidates.iter().cloned());
    related.extend(
        receipt
            .bindings
            .tracks
            .values()
            .map(|binding| ObjectRef::Bus(binding.mixer_bus)),
    );
    let mut diagnostics = vec![RevealDiagnostic::new(
        RevealDiagnosticCode::ReceiptUsedPublicationScope,
        "legacy reconstruction receipt lacks a content scope; navigation retains its project revision and source asset as the finding scope",
    )];
    let object = if let Some(object) = candidates.first().cloned() {
        if candidates.len() > 1 {
            diagnostics.push(RevealDiagnostic::new(
                RevealDiagnosticCode::ReceiptHadMultipleCandidates,
                format!(
                    "reconstruction authored {} revealable objects; selected the first editable construction and retained every sibling",
                    candidates.len()
                ),
            ));
        }
        object
    } else {
        diagnostics.push(RevealDiagnostic::new(
            RevealDiagnosticCode::ReceiptHadNoConstruction,
            "reconstruction receipt contains no currently revealable construction; selected its source material",
        ));
        ObjectRef::Material(receipt.bindings.source_asset)
    };
    RevealRecommendation {
        request: RevealRequest::new(object, RevealIntent::ActivateExisting)
            .at_revision(receipt.project_revision)
            .with_related(related),
        diagnostics,
    }
}

#[derive(Clone, Debug)]
struct SurfaceSpec {
    kind: WorkspaceItemKind,
    target: EditorTarget,
    state: EditorViewState,
    scope: String,
    object: String,
    multiplicity: TargetMultiplicity,
}

impl SurfaceSpec {
    fn new_view(&self) -> NewWorkspaceView {
        NewWorkspaceView {
            kind: self.kind.clone(),
            target: self.target.clone(),
            title_override: None,
            links: ViewLinkMembership {
                group: LinkGroupId::UNLINKED,
                facets: LinkFacets::NONE,
            },
            state: self.state.clone(),
            extensions: navigation_extensions(&self.scope, &self.object),
        }
    }
}

fn plan_retarget_or_fallback(
    document: &WorkspaceDocument,
    request: &RevealRequest,
    surface: &SurfaceSpec,
    diagnostics: &mut Vec<RevealDiagnostic>,
) -> WorkspaceReveal {
    let Some(current) = request.current_view else {
        diagnostics.push(RevealDiagnostic::new(
            RevealDiagnosticCode::MissingCurrentView,
            "RetargetCurrent requires the originating workspace view",
        ));
        return plan_activate_or_create(document, surface, diagnostics);
    };
    let Some(descriptor) = document.views.get(&current) else {
        diagnostics.push(RevealDiagnostic::new(
            RevealDiagnosticCode::CurrentViewMissing,
            format!("current workspace view {} no longer exists", current.0),
        ));
        return plan_activate_or_create(document, surface, diagnostics);
    };
    if !same_surface_family(&descriptor.kind, &surface.kind) {
        diagnostics.push(RevealDiagnostic::new(
            RevealDiagnosticCode::IncompatibleRetarget,
            format!(
                "current {:?} view cannot retarget to {:?}",
                descriptor.kind, surface.kind
            ),
        ));
        return plan_activate_or_create(document, surface, diagnostics);
    }
    let mut next = descriptor.clone();
    next.target = surface.target.clone();
    next.extensions.insert(
        NAVIGATION_SCOPE.into(),
        Value::String(surface.scope.clone()),
    );
    next.extensions.insert(
        NAVIGATION_OBJECT.into(),
        Value::String(surface.object.clone()),
    );
    WorkspaceReveal::Retarget {
        descriptor: next,
        location: document.location(current).unwrap_or(ViewLocation::Hidden),
    }
}

fn plan_activate_or_create(
    document: &WorkspaceDocument,
    surface: &SurfaceSpec,
    diagnostics: &mut Vec<RevealDiagnostic>,
) -> WorkspaceReveal {
    if let Some(view) = find_exact(document, surface) {
        return activate(document, view);
    }
    if let Some(view) = find_untargeted_compatible(document, surface) {
        let descriptor = &document.views[&view];
        let mut next = descriptor.clone();
        next.target = surface.target.clone();
        next.extensions.insert(
            NAVIGATION_SCOPE.into(),
            Value::String(surface.scope.clone()),
        );
        next.extensions.insert(
            NAVIGATION_OBJECT.into(),
            Value::String(surface.object.clone()),
        );
        diagnostics.push(RevealDiagnostic::new(
            RevealDiagnosticCode::UntargetedViewRetargeted,
            "retargeted an existing untargeted workspace surface instead of creating a duplicate",
        ));
        return WorkspaceReveal::Retarget {
            descriptor: next,
            location: document.location(view).unwrap_or(ViewLocation::Hidden),
        };
    }
    if surface.multiplicity == TargetMultiplicity::SingletonBySurface {
        if let Some(view) = find_surface(document, surface) {
            return activate(document, view);
        }
    }
    WorkspaceReveal::Create(surface.new_view())
}

fn activate(document: &WorkspaceDocument, view: WorkspaceViewId) -> WorkspaceReveal {
    WorkspaceReveal::Activate {
        view,
        location: document.location(view).unwrap_or(ViewLocation::Hidden),
    }
}

fn find_exact(document: &WorkspaceDocument, surface: &SurfaceSpec) -> Option<WorkspaceViewId> {
    document
        .views
        .values()
        .find(|descriptor| descriptor_matches_surface(descriptor, surface))
        .map(|descriptor| descriptor.id)
}

fn find_surface(document: &WorkspaceDocument, surface: &SurfaceSpec) -> Option<WorkspaceViewId> {
    document
        .views
        .values()
        .find(|descriptor| same_surface_family(&descriptor.kind, &surface.kind))
        .map(|descriptor| descriptor.id)
}

fn find_untargeted_compatible(
    document: &WorkspaceDocument,
    surface: &SurfaceSpec,
) -> Option<WorkspaceViewId> {
    document
        .views
        .values()
        .find(|descriptor| {
            same_surface_family(&descriptor.kind, &surface.kind)
                && !descriptor.extensions.contains_key(NAVIGATION_SCOPE)
                && target_is_untargeted(&descriptor.target)
        })
        .map(|descriptor| descriptor.id)
}

pub fn descriptor_matches_object(descriptor: &WorkspaceViewDescriptor, object: &ObjectRef) -> bool {
    surface_for(object).is_some_and(|surface| descriptor_matches_surface(descriptor, &surface))
}

fn descriptor_matches_surface(descriptor: &WorkspaceViewDescriptor, surface: &SurfaceSpec) -> bool {
    if !same_surface_family(&descriptor.kind, &surface.kind) {
        return false;
    }
    if surface.multiplicity == TargetMultiplicity::SingletonBySurface {
        return true;
    }
    descriptor
        .extensions
        .get(NAVIGATION_SCOPE)
        .and_then(Value::as_str)
        .is_some_and(|scope| scope == surface.scope)
        || descriptor.target == surface.target
}

fn same_surface_family(left: &WorkspaceItemKind, right: &WorkspaceItemKind) -> bool {
    match (left, right) {
        (WorkspaceItemKind::PatternEditor { .. }, WorkspaceItemKind::PatternEditor { .. })
        | (WorkspaceItemKind::AnalysisLens { .. }, WorkspaceItemKind::AnalysisLens { .. }) => true,
        (
            WorkspaceItemKind::Extension {
                namespace: left_namespace,
                name: left_name,
            },
            WorkspaceItemKind::Extension {
                namespace: right_namespace,
                name: right_name,
            },
        ) => left_namespace == right_namespace && left_name == right_name,
        _ => left == right,
    }
}

fn target_is_untargeted(target: &EditorTarget) -> bool {
    match target {
        EditorTarget::Mixer { bus_id: None }
        | EditorTarget::Analysis { source_id: None }
        | EditorTarget::Render {
            comparison_id: None,
        } => true,
        EditorTarget::Extension { namespace, key } => {
            namespace == EXTENSION_NAMESPACE && key == "active-kit"
        }
        _ => false,
    }
}

fn inspector_surface() -> SurfaceSpec {
    SurfaceSpec {
        kind: WorkspaceItemKind::Inspector,
        target: EditorTarget::Inspector,
        state: EditorViewState::Inspector,
        scope: "inspector".into(),
        object: "inspector".into(),
        multiplicity: TargetMultiplicity::SingletonBySurface,
    }
}

fn surface_for(object: &ObjectRef) -> Option<SurfaceSpec> {
    let object_address = object.address();
    let spec = match object {
        ObjectRef::Material(asset) => browser_surface(*asset, object_address),
        ObjectRef::Sample(material) => {
            let asset = match material {
                SourceMaterialRef::Asset(asset) => *asset,
                SourceMaterialRef::VirtualSlice(slice) => slice.source_asset,
            };
            browser_surface(asset, object_address)
        }
        ObjectRef::Instrument(instrument) => sampler_surface(instrument.kit(), object_address),
        ObjectRef::Pad(pad) => sampler_surface(pad.kit, object_address),
        ObjectRef::Pattern(pattern) => SurfaceSpec {
            kind: WorkspaceItemKind::PatternEditor {
                mode: PatternEditorMode::Steps,
            },
            target: EditorTarget::PatternDefinition { id: pattern.get() },
            state: default_pattern_state(),
            scope: format!("pattern:{}", pattern.get()),
            object: object_address,
            multiplicity: TargetMultiplicity::SingletonByTarget,
        },
        ObjectRef::PatternOccurrence(_) | ObjectRef::AudioClip(_) | ObjectRef::Track(_) => {
            SurfaceSpec {
                kind: WorkspaceItemKind::Arrangement,
                target: EditorTarget::Arrangement,
                state: default_arrangement_state(),
                scope: "arrangement".into(),
                object: object_address,
                multiplicity: TargetMultiplicity::SingletonBySurface,
            }
        }
        ObjectRef::Bus(bus) => SurfaceSpec {
            kind: WorkspaceItemKind::Mixer,
            target: EditorTarget::Mixer {
                bus_id: Some(bus.get()),
            },
            state: EditorViewState::Mixer,
            scope: "mixer".into(),
            object: object_address,
            multiplicity: TargetMultiplicity::SingletonBySurface,
        },
        ObjectRef::Automation(lane) => SurfaceSpec {
            kind: WorkspaceItemKind::AutomationEditor,
            target: EditorTarget::AutomationLane { id: lane.get() },
            state: EditorViewState::Automation {
                viewport: BeatViewport {
                    start_tick: 0,
                    end_tick: PPQ * 16,
                },
            },
            scope: format!("automation:{}", lane.get()),
            object: object_address,
            multiplicity: TargetMultiplicity::SingletonByTarget,
        },
        ObjectRef::Finding(finding) => finding_surface(*finding, object_address),
        ObjectRef::Explanation(explanation) => extension_surface(
            "explanation",
            format!("explanation:{}", explanation.0),
            object_address,
        ),
        ObjectRef::Comparison(comparison) => SurfaceSpec {
            kind: WorkspaceItemKind::Render,
            target: EditorTarget::Render {
                comparison_id: Some(comparison.0),
            },
            state: EditorViewState::Render,
            scope: format!("comparison:{}", comparison.0),
            object: object_address,
            multiplicity: TargetMultiplicity::SingletonByTarget,
        },
        ObjectRef::Reading(reading) => {
            extension_surface("reading", format!("reading:{reading}"), object_address)
        }
    };
    Some(spec)
}

fn browser_surface(asset: AssetId, object: String) -> SurfaceSpec {
    SurfaceSpec {
        kind: WorkspaceItemKind::Browser,
        target: EditorTarget::Assets,
        state: EditorViewState::Browser {
            search: String::new(),
            selected_asset_id: Some(asset.0),
        },
        scope: "library:materials".into(),
        object,
        multiplicity: TargetMultiplicity::SingletonBySurface,
    }
}

fn sampler_surface(kit: KitId, object: String) -> SurfaceSpec {
    SurfaceSpec {
        kind: WorkspaceItemKind::Extension {
            namespace: EXTENSION_NAMESPACE.into(),
            name: "sampler".into(),
        },
        target: EditorTarget::Extension {
            namespace: EXTENSION_NAMESPACE.into(),
            key: format!("kit:{}", kit.get()),
        },
        state: EditorViewState::Extension { data: Value::Null },
        scope: format!("instrument:kit:{}", kit.get()),
        object,
        multiplicity: TargetMultiplicity::SingletonByTarget,
    }
}

fn finding_surface(finding: FindingRef, object: String) -> SurfaceSpec {
    let lens = match finding.kind {
        FindingKind::Rhythm => AnalysisLensKind::Rhythm,
        FindingKind::Separation => AnalysisLensKind::Separation,
        FindingKind::Loom => AnalysisLensKind::Loom,
        FindingKind::Components | FindingKind::ModelClaim | FindingKind::Other => {
            AnalysisLensKind::Components
        }
    };
    let target = match finding.local {
        FindingLocalId::ReconstructionProposal(proposal) => EditorTarget::Explanation {
            proposal_id: proposal.get(),
        },
        FindingLocalId::Claim(_) => EditorTarget::Analysis { source_id: None },
    };
    SurfaceSpec {
        kind: WorkspaceItemKind::AnalysisLens { lens },
        target,
        state: default_analysis_state(),
        scope: finding_address(finding),
        object,
        multiplicity: TargetMultiplicity::SingletonByTarget,
    }
}

fn extension_surface(name: &str, key: String, object: String) -> SurfaceSpec {
    SurfaceSpec {
        kind: WorkspaceItemKind::Extension {
            namespace: EXTENSION_NAMESPACE.into(),
            name: name.into(),
        },
        target: EditorTarget::Extension {
            namespace: EXTENSION_NAMESPACE.into(),
            key: key.clone(),
        },
        state: EditorViewState::Extension { data: Value::Null },
        scope: key,
        object,
        multiplicity: TargetMultiplicity::SingletonByTarget,
    }
}

fn default_arrangement_state() -> EditorViewState {
    EditorViewState::Arrangement {
        viewport: FrameViewport { start: 0, end: 1 },
        follow: true,
        header_width: Some(190.0),
    }
}

fn default_pattern_state() -> EditorViewState {
    EditorViewState::Pattern {
        viewport: BeatViewport {
            start_tick: 0,
            end_tick: PPQ * 16,
        },
        vertical_origin: None,
    }
}

fn default_analysis_state() -> EditorViewState {
    EditorViewState::Analysis {
        viewport: FrameViewport { start: 0, end: 1 },
        follow: true,
        min_frequency_hz: None,
        max_frequency_hz: None,
        recipe_fingerprint: None,
    }
}

fn navigation_extensions(scope: &str, object: &str) -> BTreeMap<String, Value> {
    BTreeMap::from([
        (NAVIGATION_SCOPE.into(), Value::String(scope.into())),
        (NAVIGATION_OBJECT.into(), Value::String(object.into())),
    ])
}

fn sampler_object(target: SamplerTarget) -> Option<ObjectRef> {
    match target {
        SamplerTarget::Kit(kit) => Some(ObjectRef::Instrument(InstrumentRef::SampleKit(kit))),
        SamplerTarget::Pad { kit, pad } => Some(ObjectRef::Pad(PadRef {
            kit,
            pad,
            zone: None,
        })),
        SamplerTarget::NewKit | SamplerTarget::NewPad { .. } => None,
    }
}

fn disposition_intent(disposition: SamplerViewDisposition) -> RevealIntent {
    match disposition {
        SamplerViewDisposition::RetargetCurrent => RevealIntent::RetargetCurrent,
        SamplerViewDisposition::OpenNew => RevealIntent::OpenNew,
    }
}

fn deduplicate_related(
    primary: &ObjectRef,
    related: impl IntoIterator<Item = ObjectRef>,
) -> Vec<ObjectRef> {
    let mut seen = BTreeSet::new();
    related
        .into_iter()
        .filter(|object| object != primary)
        .filter(|object| seen.insert(object.address()))
        .collect()
}

fn valid_object(object: &ObjectRef) -> bool {
    match object {
        ObjectRef::Material(asset) => asset.0 != 0,
        ObjectRef::Sample(SourceMaterialRef::Asset(asset)) => asset.0 != 0,
        ObjectRef::Sample(SourceMaterialRef::VirtualSlice(slice)) => {
            slice.source_asset.0 != 0 && slice.source_range.start < slice.source_range.end
        }
        ObjectRef::Instrument(instrument) => instrument.kit().get() != 0,
        ObjectRef::Pad(pad) => {
            pad.kit.get() != 0 && pad.pad.get() != 0 && pad.zone.is_none_or(|zone| zone.get() != 0)
        }
        ObjectRef::Pattern(pattern) => pattern.get() != 0,
        ObjectRef::PatternOccurrence(occurrence) => occurrence.arrangement_clip.get() != 0,
        ObjectRef::AudioClip(clip) => clip.get() != 0,
        ObjectRef::Track(track) => track.get() != 0,
        ObjectRef::Bus(bus) => bus.get() != 0,
        ObjectRef::Automation(lane) => lane.get() != 0,
        ObjectRef::Finding(finding) => valid_finding(*finding),
        ObjectRef::Explanation(explanation) => explanation.0 != 0,
        ObjectRef::Comparison(comparison) => comparison.0 != 0,
        ObjectRef::Reading(_) => true,
    }
}

fn valid_finding(finding: FindingRef) -> bool {
    let scope = match finding.scope {
        FindingScope::Artifact(_) => true,
        FindingScope::Derivation(scope) => scope.0 != 0,
        FindingScope::ProjectPublication { revision, source } => revision != 0 && source.0 != 0,
    };
    let local = match finding.local {
        FindingLocalId::ReconstructionProposal(proposal) => proposal.get() != 0,
        FindingLocalId::Claim(claim) => claim != 0,
    };
    scope && local
}

fn finding_address(finding: FindingRef) -> String {
    let kind = match finding.kind {
        FindingKind::Rhythm => "rhythm",
        FindingKind::Components => "components",
        FindingKind::Separation => "separation",
        FindingKind::Loom => "loom",
        FindingKind::ModelClaim => "model-claim",
        FindingKind::Other => "other",
    };
    let scope = match finding.scope {
        FindingScope::Artifact(artifact) => artifact_address(artifact),
        FindingScope::Derivation(scope) => format!("derivation:{:032x}", scope.0),
        FindingScope::ProjectPublication { revision, source } => {
            format!("publication:{revision}:asset:{}", source.0)
        }
    };
    let local = match finding.local {
        FindingLocalId::ReconstructionProposal(proposal) => {
            format!("proposal:{}", proposal.get())
        }
        FindingLocalId::Claim(claim) => format!("claim:{claim}"),
    };
    format!("finding:{kind}:{scope}:{local}")
}

fn artifact_address(artifact: ArtifactId) -> String {
    let algorithm = match artifact.0.algorithm {
        DigestAlgorithm::Sha256 => "sha256",
        DigestAlgorithm::Blake3 => "blake3",
        DigestAlgorithm::StableNonCryptographic => "stable",
    };
    let mut hex = String::with_capacity(64);
    for byte in artifact.0.bytes {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    format!("artifact:{algorithm}:{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::reconstruction::ReconstructionTrackId;
    use crate::reconstruction_apply::{AppliedPatternBinding, ReconstructionApplicationBindings};

    fn kit(raw: u64) -> KitId {
        KitId::from_raw(raw)
    }

    fn pad(raw: u64) -> PadId {
        PadId::from_raw(raw)
    }

    fn pattern(raw: u64) -> PatternId {
        PatternId::from_raw(raw)
    }

    fn apply_create(document: &mut WorkspaceDocument, reveal: &WorkspaceReveal) -> WorkspaceViewId {
        let WorkspaceReveal::Create(new) = reveal else {
            panic!("expected a new workspace descriptor")
        };
        let id = document.create_view(new.clone()).unwrap();
        document.show_view(id).unwrap();
        id
    }

    #[test]
    fn one_shot_reveals_exact_pad_in_kit_scoped_instrument_view() {
        let publication = ConstructivePublication {
            revision: 4,
            kit: kit(7),
            pad: Some(pad(3)),
            pattern: None,
            arrangement_clip: None,
            focus: ConstructivePublishedFocus::Pad {
                kit: kit(7),
                pad: pad(3),
            },
        };
        let recommendation = recommend_constructive(&publication);
        let mut document = WorkspaceDocument::default();
        let plan = ObjectNavigator::plan(&document, recommendation.request.clone());
        assert_eq!(
            plan.selection.primary,
            ObjectRef::Pad(PadRef {
                kit: kit(7),
                pad: pad(3),
                zone: None,
            })
        );
        let view = apply_create(&mut document, &plan.workspace);
        let second = ObjectNavigator::plan(&document, recommendation.request);
        assert!(matches!(
            second.workspace,
            WorkspaceReveal::Activate { view: actual, .. } if actual == view
        ));
        assert!(descriptor_matches_object(
            &document.views[&view],
            &ObjectRef::Instrument(InstrumentRef::SampleKit(kit(7)))
        ));
    }

    #[test]
    fn chop_result_keeps_exact_source_slice_as_inspector_breadcrumb() {
        let source_range = crate::assets::AssetFrameRange::new(
            crate::assets::SampleFrames(120),
            crate::assets::SampleFrames(480),
        )
        .unwrap();
        let result = SamplePublishedResult {
            revision: 9,
            kit: kit(2),
            pad: Some(pad(5)),
            pattern: None,
            focus: SampleResultFocus::Kit(kit(2)),
            provenance: Some(SampleResultProvenance::Selection {
                source: crate::sample_actions::SampleSelection {
                    asset: AssetId(11),
                    source_range: Some(source_range),
                },
                chop: Some(crate::sample_actions::SampleChopIntent::EqualSlices { count: 8 }),
            }),
        };
        let recommendation = recommend_sample_result(&result);
        assert!(recommendation.request.related.contains(&ObjectRef::Sample(
            SourceMaterialRef::VirtualSlice(crate::sample_material::VirtualSliceRef {
                source_asset: AssetId(11),
                source_range,
            })
        )));
        let plan = ObjectNavigator::plan(&WorkspaceDocument::default(), recommendation.request);
        assert_eq!(
            plan.inspector.target,
            ObjectRef::Instrument(InstrumentRef::SampleKit(kit(2)))
        );
        assert!(matches!(plan.workspace, WorkspaceReveal::Create(_)));
    }

    #[test]
    fn make_beat_reveals_occurrence_and_retains_pattern_and_instrument() {
        let occurrence = ClipId::from_raw(31);
        let publication = ConstructivePublication {
            revision: 12,
            kit: kit(4),
            pad: Some(pad(1)),
            pattern: Some(pattern(8)),
            arrangement_clip: Some(occurrence),
            focus: ConstructivePublishedFocus::Arrangement(occurrence),
        };
        let recommendation = recommend_constructive(&publication);
        assert_eq!(
            recommendation.request.object,
            ObjectRef::PatternOccurrence(PatternOccurrenceRef {
                arrangement_clip: occurrence,
                sequencer_clip: None,
                pattern: Some(pattern(8)),
            })
        );
        assert!(recommendation
            .request
            .related
            .contains(&ObjectRef::Pattern(pattern(8))));
        assert!(recommendation
            .request
            .related
            .contains(&ObjectRef::Instrument(InstrumentRef::SampleKit(kit(4)))));
        let plan = ObjectNavigator::plan(&WorkspaceDocument::default(), recommendation.request);
        let WorkspaceReveal::Create(view) = plan.workspace else {
            panic!("default workspace has no arrangement editor")
        };
        assert_eq!(view.kind, WorkspaceItemKind::Arrangement);
        assert_eq!(view.target, EditorTarget::Arrangement);
    }

    #[test]
    fn reverse_promotion_reveals_editable_pattern_without_losing_finding_scope() {
        let source = AssetId(6);
        let proposal = ReconstructionProposalId::from_raw(3);
        let promoted_pattern = pattern(19);
        let receipt = ReconstructionApplicationReceipt {
            project_revision: 22,
            bindings: ReconstructionApplicationBindings {
                proposal,
                source_asset: source,
                tracks: BTreeMap::new(),
                slices: BTreeMap::new(),
                triggers: BTreeMap::new(),
                patterns: BTreeMap::from([(
                    ReconstructionTrackId::from_raw(4),
                    AppliedPatternBinding {
                        sequencer_pattern: promoted_pattern,
                        arrangement_pattern: crate::arrangement::PatternId::from_raw(9),
                        source_origin_tick: 0,
                    },
                )]),
                pitched_events: BTreeMap::new(),
                automations: BTreeMap::new(),
                unresolved_modulations: BTreeMap::new(),
                unresolved_effects: BTreeMap::new(),
                unresolved_latent_components: BTreeMap::new(),
                residual: None,
                evidence: BTreeMap::new(),
            },
            diagnostics: Vec::new(),
        };
        let recommendation = recommend_reconstruction(&receipt);
        assert_eq!(
            recommendation.request.object,
            ObjectRef::Pattern(promoted_pattern)
        );
        assert!(recommendation.request.related.iter().any(|object| matches!(
            object,
            ObjectRef::Finding(FindingRef {
                scope: FindingScope::ProjectPublication {
                    revision: 22,
                    source: actual_source,
                },
                local: FindingLocalId::ReconstructionProposal(actual),
                ..
            }) if *actual_source == source && *actual == proposal
        )));
        let plan = ObjectNavigator::plan(&WorkspaceDocument::default(), recommendation.request);
        assert!(matches!(
            plan.workspace,
            WorkspaceReveal::Create(NewWorkspaceView {
                kind: WorkspaceItemKind::PatternEditor { .. },
                target: EditorTarget::PatternDefinition { id: 19 },
                ..
            })
        ));
    }

    #[test]
    fn retarget_current_is_explicit_and_preserves_descriptor_identity() {
        let mut document = WorkspaceDocument::default();
        let first = ObjectRef::Pad(PadRef {
            kit: kit(2),
            pad: pad(1),
            zone: None,
        });
        let created = ObjectNavigator::plan(
            &document,
            RevealRequest::new(first, RevealIntent::ActivateExisting),
        );
        let view = apply_create(&mut document, &created.workspace);
        let second = ObjectRef::Pad(PadRef {
            kit: kit(3),
            pad: pad(4),
            zone: None,
        });
        let plan = ObjectNavigator::plan(
            &document,
            RevealRequest::new(second.clone(), RevealIntent::RetargetCurrent)
                .with_current_view(view),
        );
        let WorkspaceReveal::Retarget { descriptor, .. } = plan.workspace else {
            panic!("compatible sampler view should retarget")
        };
        assert_eq!(descriptor.id, view);
        assert!(descriptor_matches_object(&descriptor, &second));
    }

    #[test]
    fn reading_and_explanation_never_fall_through_to_an_unrelated_lens() {
        let reading = ReadingId::new([7; 16]).unwrap();
        let reading_plan = ObjectNavigator::plan(
            &WorkspaceDocument::default(),
            RevealRequest::new(ObjectRef::Reading(reading), RevealIntent::ActivateExisting),
        );
        assert!(matches!(
            reading_plan.workspace,
            WorkspaceReveal::Create(NewWorkspaceView {
                kind: WorkspaceItemKind::Extension { ref namespace, ref name },
                ..
            }) if namespace == "audec" && name == "reading"
        ));

        let explanation = ObjectRef::Explanation(ExplanationId(5));
        let explanation_plan = ObjectNavigator::plan(
            &WorkspaceDocument::default(),
            RevealRequest::new(explanation, RevealIntent::ActivateExisting),
        );
        assert!(matches!(
            explanation_plan.workspace,
            WorkspaceReveal::Create(NewWorkspaceView {
                kind: WorkspaceItemKind::Extension { ref namespace, ref name },
                ..
            }) if namespace == "audec" && name == "explanation"
        ));
    }

    #[test]
    fn stale_receipt_never_creates_or_retargets_a_workspace_view() {
        let request = RevealRequest::new(
            ObjectRef::Pattern(pattern(17)),
            RevealIntent::ActivateExisting,
        )
        .at_revision(41);
        let plan = ObjectNavigator::plan_at_revision(&WorkspaceDocument::default(), 42, request);
        assert_eq!(plan.workspace, WorkspaceReveal::Unsupported);
        assert!(plan
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == RevealDiagnosticCode::StalePublication));
    }
}
