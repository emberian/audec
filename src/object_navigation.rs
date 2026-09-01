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
use crate::artifact_catalog::{ArtifactId, ContentDigest, DigestAlgorithm};
use crate::assets::{AssetFrameRange, AssetId, SampleFrames};
use crate::automation::AutomationLaneId;
use crate::comparison::ComparisonId;
use crate::explanation::ExplanationId;
use crate::mixer::BusId;
use crate::reading::ReadingId;
use crate::reconstruction::ReconstructionProposalId;
use crate::reconstruction_apply::ReconstructionApplicationReceipt;
use crate::sample_actions::{
    SampleAuditionIntent, SamplePublishedResult, SampleResultFocus, SampleResultProvenance,
    SamplerTarget, SamplerViewDisposition,
};
use crate::sample_kit::{KitId, PadId, ZoneId};
use crate::sample_material::{DerivationScope, SourceMaterialRef, VirtualSliceRef};
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
pub struct AutomationOccurrenceRef {
    pub arrangement_clip: ClipId,
    pub lane: AutomationLaneId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FindingKind {
    Rhythm,
    Components,
    Separation,
    Loom,
    ModelClaim,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FindingLocalId {
    ReconstructionProposal(ReconstructionProposalId),
    Claim(u64),
}

/// Artifact-qualified finding identity with a stable lexicographic order.
///
/// The order includes the kind, scope, and analyzer-local ID, so ordered
/// collections preserve the same distinction as equality and hashing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    AutomationOccurrence(AutomationOccurrenceRef),
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
    AutomationOccurrence,
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
            Self::AutomationOccurrence(_) => ObjectKind::AutomationOccurrence,
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
            Self::AutomationOccurrence(occurrence) => format!(
                "automation-occurrence:{}:lane:{}",
                occurrence.arrangement_clip.get(),
                occurrence.lane.get()
            ),
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

/// Product verbs which may originate in Explorer, Inspector, a completion
/// receipt, or an editor.  Keeping the verb beside the exact [`ObjectRef`]
/// prevents callers from translating "open" into a pane-specific guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectAction {
    Reveal,
    Inspect,
    Edit,
    Audition(ObjectAuditionSignal),
}

/// Signal choice for an audible action. `Natural` means the object's ordinary
/// sound (a sample preview, pad gate, pattern cycle, or clip occurrence).
/// Interpretive objects require an explicit layer, except that `Natural`
/// defaults to their construction rather than pretending the claim itself is
/// PCM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectAuditionSignal {
    Natural,
    Source,
    Construction,
    Residual,
}

/// One action request, retaining the existing reveal receipt vocabulary as
/// its navigation/selection payload.  Promotion and extraction adapters can
/// therefore become editable or audible without copying their related-object
/// and publication guards into another outcome type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectActionRequest {
    pub action: ObjectAction,
    pub navigation: RevealRequest,
}

impl ObjectActionRequest {
    pub fn new(object: ObjectRef, action: ObjectAction) -> Self {
        Self {
            action,
            navigation: RevealRequest::new(object, RevealIntent::ActivateExisting),
        }
    }

    pub fn from_reveal(navigation: RevealRequest, action: ObjectAction) -> Self {
        Self { action, navigation }
    }

    pub const fn at_revision(mut self, revision: u64) -> Self {
        self.navigation.expected_project_revision = Some(revision);
        self
    }

    pub fn with_current_view(mut self, view: WorkspaceViewId) -> Self {
        self.navigation.current_view = Some(view);
        self
    }

    pub fn with_related(mut self, related: impl IntoIterator<Item = ObjectRef>) -> Self {
        self.navigation.related = deduplicate_related(&self.navigation.object, related);
        self
    }
}

impl RevealRecommendation {
    /// Reuse a typed completion receipt for Reveal, Inspect, Edit, or Audition
    /// while retaining its exact publication pin and related breadcrumbs.
    pub fn action_request(&self, action: ObjectAction) -> ObjectActionRequest {
        ObjectActionRequest::from_reveal(self.request.clone(), action)
    }

    pub fn into_action(self, action: ObjectAction) -> ObjectActionRequest {
        ObjectActionRequest::from_reveal(self.request, action)
    }
}

/// Exact editor focus handed to the destination presenter after its workspace
/// descriptor has been activated or created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObjectEditRoute {
    Material(SourceMaterialRef),
    Instrument {
        kit: KitId,
        pad: Option<PadId>,
        zone: Option<ZoneId>,
    },
    Pattern(PatternId),
    Arrangement(ObjectRef),
    Mixer(BusId),
    Automation(AutomationLaneId),
    ExplanationConstruction(ExplanationId),
}

/// Typed handoff to an already-existing audio authority.  This does not own
/// playback: sample intents go through `SamplePaneBridge`, pattern occurrences
/// through the shared pattern audition adapter, arrangement clips through the
/// project transport, and interpretive layers through the reverse presenter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuditionPatternOccurrence {
    pub arrangement_clip: ClipId,
    pub sequencer_clip: PatternClipId,
    pub pattern: PatternId,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ObjectAuditionRoute {
    Sample(SampleAuditionIntent),
    PatternOccurrence(AuditionPatternOccurrence),
    ArrangementClip(ClipId),
    Investigation {
        object: ObjectRef,
        signal: ObjectAuditionSignal,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum ObjectActionDispatch {
    Reveal,
    Inspect,
    Edit(ObjectEditRoute),
    Audition(ObjectAuditionRoute),
}

/// One presenter-ready transition. All verbs update the same exact semantic
/// selection and Inspector target. Only the workspace and typed dispatch vary.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectActionPlan {
    pub action: ObjectAction,
    pub reveal: RevealPlan,
    pub dispatch: ObjectActionDispatch,
}

/// Availability is supplied by the current authoritative publication. The
/// navigator intentionally cannot infer it from a visible pane or raw ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectAvailability {
    Present,
    Missing,
    /// The caller does not own the store that could prove this identity. This
    /// is different from deletion and must not be rendered as "not found".
    AuthorityUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectActionRefusalReason {
    InvalidObject,
    StalePublication { expected: u64, actual: u64 },
    MissingObject,
    AuthorityUnavailable,
    ReadOnly,
    NoAudibleSignal,
    NeedsAudibleOccurrence,
    UnsupportedSignal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectActionRefusal {
    pub action: ObjectAction,
    pub object: ObjectRef,
    pub reason: ObjectActionRefusalReason,
    pub message: String,
}

/// Checked routing distinguishes a current target, an honest predecessor, and
/// a refusal. A deleted identity is never left in the returned action plan.
#[derive(Clone, Debug, PartialEq)]
pub enum ObjectActionResolution {
    Ready(ObjectActionPlan),
    Predecessor {
        deleted: ObjectRef,
        target: ObjectRef,
        plan: ObjectActionPlan,
    },
    Refused(ObjectActionRefusal),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ObjectNavigator;

impl ObjectNavigator {
    pub fn plan(document: &WorkspaceDocument, request: RevealRequest) -> RevealPlan {
        Self::plan_inner(document, request)
    }

    /// Plan a product verb without consulting object existence. This is the
    /// synchronous path for an interaction already derived from the current
    /// publication. Delayed callbacks and restored selections should use
    /// [`Self::plan_action_checked`] instead.
    pub fn plan_action(
        document: &WorkspaceDocument,
        request: ObjectActionRequest,
    ) -> ObjectActionResolution {
        Self::plan_action_inner(document, request)
    }

    /// Plan against an explicit project revision. This preserves the existing
    /// receipt rule for callers which have already resolved object existence
    /// but still need a final short publication guard.
    pub fn plan_action_at_revision(
        document: &WorkspaceDocument,
        current_project_revision: u64,
        request: ObjectActionRequest,
    ) -> ObjectActionResolution {
        if let Some(expected) = request.navigation.expected_project_revision {
            if expected != current_project_revision {
                return ObjectActionResolution::Refused(action_refusal(
                    &request,
                    ObjectActionRefusalReason::StalePublication {
                        expected,
                        actual: current_project_revision,
                    },
                    format!(
                        "{} was published at project revision {expected}, but the current revision is {current_project_revision}",
                        request.navigation.object.address()
                    ),
                ));
            }
        }
        Self::plan_action_inner(document, request)
    }

    /// Resolve an action against the current object authority. Missing
    /// primaries try typed parents and receipt-related objects in stable order;
    /// unavailable authorities and exhausted predecessor chains are explicit
    /// refusals. Missing related objects are removed before selection is
    /// published, so promotion evidence deleted after undo cannot remain as a
    /// false breadcrumb.
    pub fn plan_action_checked(
        document: &WorkspaceDocument,
        current_project_revision: u64,
        mut request: ObjectActionRequest,
        mut availability: impl FnMut(&ObjectRef) -> ObjectAvailability,
    ) -> ObjectActionResolution {
        if !valid_object(&request.navigation.object) {
            return ObjectActionResolution::Refused(action_refusal(
                &request,
                ObjectActionRefusalReason::InvalidObject,
                format!(
                    "{} has an invalid {:?} identity",
                    request.navigation.object.address(),
                    request.navigation.object.kind()
                ),
            ));
        }
        if let Some(expected) = request.navigation.expected_project_revision {
            if expected != current_project_revision {
                return ObjectActionResolution::Refused(action_refusal(
                    &request,
                    ObjectActionRefusalReason::StalePublication {
                        expected,
                        actual: current_project_revision,
                    },
                    format!(
                        "{} was published at project revision {expected}, but the current revision is {current_project_revision}",
                        request.navigation.object.address()
                    ),
                ));
            }
        }

        match availability(&request.navigation.object) {
            ObjectAvailability::Present => {
                request.navigation.related.retain(|object| {
                    valid_object(object) && availability(object) == ObjectAvailability::Present
                });
                Self::plan_action_inner(document, request)
            }
            ObjectAvailability::AuthorityUnavailable => {
                ObjectActionResolution::Refused(action_refusal(
                    &request,
                    ObjectActionRefusalReason::AuthorityUnavailable,
                    format!(
                        "the current publication cannot prove whether {} still exists",
                        request.navigation.object.address()
                    ),
                ))
            }
            ObjectAvailability::Missing => {
                let deleted = request.navigation.object.clone();
                let candidates = action_predecessors(&request.navigation);
                for candidate in candidates {
                    if availability(&candidate) != ObjectAvailability::Present {
                        continue;
                    }
                    let mut fallback = request.clone();
                    fallback.navigation.object = candidate.clone();
                    fallback.navigation.intent = RevealIntent::ActivateExisting;
                    fallback.navigation.expected_project_revision = Some(current_project_revision);
                    fallback.navigation.related = request
                        .navigation
                        .related
                        .iter()
                        .filter(|object| {
                            **object != candidate
                                && valid_object(object)
                                && availability(object) == ObjectAvailability::Present
                        })
                        .cloned()
                        .collect();
                    if let ObjectActionResolution::Ready(plan) =
                        Self::plan_action_inner(document, fallback)
                    {
                        return ObjectActionResolution::Predecessor {
                            deleted,
                            target: candidate,
                            plan,
                        };
                    }
                }
                ObjectActionResolution::Refused(action_refusal(
                    &request,
                    ObjectActionRefusalReason::MissingObject,
                    format!(
                        "{} no longer exists and no compatible predecessor is current",
                        deleted.address()
                    ),
                ))
            }
        }
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

    fn plan_action_inner(
        document: &WorkspaceDocument,
        request: ObjectActionRequest,
    ) -> ObjectActionResolution {
        if !valid_object(&request.navigation.object) {
            return ObjectActionResolution::Refused(action_refusal(
                &request,
                ObjectActionRefusalReason::InvalidObject,
                format!(
                    "{} has an invalid {:?} identity",
                    request.navigation.object.address(),
                    request.navigation.object.kind()
                ),
            ));
        }

        let dispatch = match request.action {
            ObjectAction::Reveal => ObjectActionDispatch::Reveal,
            ObjectAction::Inspect => ObjectActionDispatch::Inspect,
            ObjectAction::Edit => match edit_route(&request.navigation.object) {
                Some(route) => ObjectActionDispatch::Edit(route),
                None => {
                    return ObjectActionResolution::Refused(action_refusal(
                        &request,
                        ObjectActionRefusalReason::ReadOnly,
                        format!(
                            "{} is read-only; reveal or inspect it instead",
                            request.navigation.object.address()
                        ),
                    ));
                }
            },
            ObjectAction::Audition(signal) => {
                match audition_route(
                    &request.navigation.object,
                    &request.navigation.related,
                    signal,
                ) {
                    Ok(route) => ObjectActionDispatch::Audition(route),
                    Err((reason, message)) => {
                        return ObjectActionResolution::Refused(action_refusal(
                            &request, reason, message,
                        ));
                    }
                }
            }
        };

        let mut navigation = request.navigation.clone();
        navigation.intent = match request.action {
            ObjectAction::Reveal => navigation.intent,
            ObjectAction::Inspect => RevealIntent::ShowInspector,
            ObjectAction::Edit => match navigation.intent {
                RevealIntent::OpenNew | RevealIntent::RetargetCurrent => navigation.intent,
                RevealIntent::ActivateExisting
                | RevealIntent::ShowInspector
                | RevealIntent::SelectOnly => RevealIntent::ActivateExisting,
            },
            // Audition changes semantic attention but must not silently locate
            // or replace the user's working surface.
            ObjectAction::Audition(_) => RevealIntent::SelectOnly,
        };
        let reveal = Self::plan_inner(document, navigation);
        ObjectActionResolution::Ready(ObjectActionPlan {
            action: request.action,
            reveal,
            dispatch,
        })
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
            sequencer_clip: publication.sequencer_clip,
            pattern: publication.pattern,
        })
    });
    let created_zones = publication.created_zones.iter().map(|target| {
        ObjectRef::Pad(PadRef {
            kit: target.kit,
            pad: target.pad,
            zone: Some(target.zone),
        })
    });
    let created_pads = publication.created_pads.iter().copied().map(|pad| {
        ObjectRef::Pad(PadRef {
            kit: publication.kit,
            pad,
            zone: None,
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
                sequencer_clip: publication.sequencer_clip,
                pattern: publication.pattern,
            }),
            RevealIntent::ActivateExisting,
        ),
        ConstructivePublishedFocus::Sampler { kit, disposition } => (
            ObjectRef::Instrument(InstrumentRef::SampleKit(kit)),
            disposition_intent(disposition),
        ),
    };
    let related = [Some(kit), pad, pattern, occurrence]
        .into_iter()
        .flatten()
        .chain(created_pads)
        .chain(created_zones)
        .chain(publication.arrangement_track.map(ObjectRef::Track))
        .chain(publication.output_bus.map(ObjectRef::Bus));
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
        SampleResultFocus::Arrangement {
            arrangement_clip,
            sequencer_clip,
            pattern,
        } => (
            ObjectRef::PatternOccurrence(PatternOccurrenceRef {
                arrangement_clip,
                sequencer_clip,
                pattern,
            }),
            RevealIntent::ActivateExisting,
        ),
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
    recommendation
        .request
        .related
        .extend(result.created_pads.iter().copied().map(|pad| {
            ObjectRef::Pad(PadRef {
                kit: result.kit,
                pad,
                zone: None,
            })
        }));
    recommendation
        .request
        .related
        .extend(result.created_zones.iter().map(|target| {
            ObjectRef::Pad(PadRef {
                kit: target.kit,
                pad: target.pad,
                zone: Some(target.zone),
            })
        }));
    if let Some(arrangement_clip) = result.arrangement_clip {
        recommendation
            .request
            .related
            .push(ObjectRef::PatternOccurrence(PatternOccurrenceRef {
                arrangement_clip,
                sequencer_clip: result.sequencer_clip,
                pattern: result.pattern,
            }));
    }
    recommendation
        .request
        .related
        .extend(result.arrangement_track.map(ObjectRef::Track));
    recommendation
        .request
        .related
        .extend(result.output_bus.map(ObjectRef::Bus));
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

/// Select the most directly editable construction in a reconstruction
/// receipt. Multiple candidates are all retained as related objects and the
/// deterministic choice is diagnosed rather than hidden.
pub fn recommend_reconstruction(
    receipt: &ReconstructionApplicationReceipt,
) -> RevealRecommendation {
    let finding = ObjectRef::Finding(FindingRef {
        kind: FindingKind::Other,
        scope: FindingScope::Derivation(receipt.derivation_scope),
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
    related.extend(
        receipt
            .bindings
            .patterns
            .values()
            .map(|binding| ObjectRef::Pattern(binding.sequencer_pattern)),
    );
    related.extend(receipt.bindings.patterns.values().filter_map(|binding| {
        binding.occurrence.map(|occurrence| {
            ObjectRef::PatternOccurrence(PatternOccurrenceRef {
                arrangement_clip: occurrence.arrangement_clip,
                sequencer_clip: Some(occurrence.sequencer_clip),
                pattern: Some(binding.sequencer_pattern),
            })
        })
    }));
    related.extend(receipt.bindings.sample_kits.values().flat_map(|binding| {
        let kit = ObjectRef::Instrument(InstrumentRef::SampleKit(binding.kit));
        std::iter::once(kit)
            .chain(binding.targets.values().map(|target| {
                ObjectRef::Pad(PadRef {
                    kit: target.kit,
                    pad: target.pad,
                    zone: Some(target.zone),
                })
            }))
            .chain(std::iter::once(ObjectRef::Bus(binding.output_bus)))
    }));
    let mut diagnostics = Vec::new();
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

fn action_refusal(
    request: &ObjectActionRequest,
    reason: ObjectActionRefusalReason,
    message: impl Into<String>,
) -> ObjectActionRefusal {
    ObjectActionRefusal {
        action: request.action,
        object: request.navigation.object.clone(),
        reason,
        message: message.into(),
    }
}

fn edit_route(object: &ObjectRef) -> Option<ObjectEditRoute> {
    match object {
        ObjectRef::Material(asset) => {
            Some(ObjectEditRoute::Material(SourceMaterialRef::Asset(*asset)))
        }
        ObjectRef::Sample(material) => Some(ObjectEditRoute::Material(*material)),
        ObjectRef::Instrument(instrument) => Some(ObjectEditRoute::Instrument {
            kit: instrument.kit(),
            pad: None,
            zone: None,
        }),
        ObjectRef::Pad(pad) => Some(ObjectEditRoute::Instrument {
            kit: pad.kit,
            pad: Some(pad.pad),
            zone: pad.zone,
        }),
        ObjectRef::Pattern(pattern) => Some(ObjectEditRoute::Pattern(*pattern)),
        ObjectRef::PatternOccurrence(_)
        | ObjectRef::AudioClip(_)
        | ObjectRef::Track(_)
        | ObjectRef::AutomationOccurrence(_) => Some(ObjectEditRoute::Arrangement(object.clone())),
        ObjectRef::Bus(bus) => Some(ObjectEditRoute::Mixer(*bus)),
        ObjectRef::Automation(lane) => Some(ObjectEditRoute::Automation(*lane)),
        ObjectRef::Explanation(explanation) => {
            Some(ObjectEditRoute::ExplanationConstruction(*explanation))
        }
        ObjectRef::Finding(_) | ObjectRef::Comparison(_) | ObjectRef::Reading(_) => None,
    }
}

fn audition_route(
    object: &ObjectRef,
    related: &[ObjectRef],
    signal: ObjectAuditionSignal,
) -> Result<ObjectAuditionRoute, (ObjectActionRefusalReason, String)> {
    let sample_signal = matches!(
        signal,
        ObjectAuditionSignal::Natural | ObjectAuditionSignal::Source
    );
    match object {
        ObjectRef::Material(asset) if sample_signal => Ok(ObjectAuditionRoute::Sample(
            SampleAuditionIntent::MaterialOneShot {
                material: SourceMaterialRef::Asset(*asset),
                velocity: 1.0,
            },
        )),
        ObjectRef::Sample(material) if sample_signal => Ok(ObjectAuditionRoute::Sample(
            SampleAuditionIntent::MaterialOneShot {
                material: *material,
                velocity: 1.0,
            },
        )),
        ObjectRef::Pad(pad) if sample_signal => Ok(ObjectAuditionRoute::Sample(
            SampleAuditionIntent::PadGate {
                kit: pad.kit,
                pad: pad.pad,
                velocity: 1.0,
                pressed: true,
            },
        )),
        ObjectRef::Instrument(instrument) if sample_signal => {
            let pad = related.iter().find_map(|related| match related {
                ObjectRef::Pad(pad) if pad.kit == instrument.kit() => Some(*pad),
                _ => None,
            });
            pad.map(|pad| {
                ObjectAuditionRoute::Sample(SampleAuditionIntent::PadGate {
                    kit: pad.kit,
                    pad: pad.pad,
                    velocity: 1.0,
                    pressed: true,
                })
            })
            .ok_or_else(|| {
                (
                    ObjectActionRefusalReason::NeedsAudibleOccurrence,
                    format!(
                        "{} is playable, but audition needs a current pad target",
                        object.address()
                    ),
                )
            })
        }
        ObjectRef::PatternOccurrence(occurrence)
            if matches!(signal, ObjectAuditionSignal::Natural | ObjectAuditionSignal::Construction) =>
        {
            complete_pattern_occurrence(*occurrence).map(ObjectAuditionRoute::PatternOccurrence)
        }
        ObjectRef::Pattern(pattern)
            if matches!(signal, ObjectAuditionSignal::Natural | ObjectAuditionSignal::Construction) =>
        {
            let occurrence = related.iter().find_map(|related| match related {
                ObjectRef::PatternOccurrence(occurrence)
                    if occurrence.pattern == Some(*pattern) =>
                {
                    Some(*occurrence)
                }
                _ => None,
            });
            occurrence
                .ok_or_else(|| {
                    (
                        ObjectActionRefusalReason::NeedsAudibleOccurrence,
                        format!(
                            "{} needs a current arrangement occurrence before it can use the shared pattern audition path",
                            object.address()
                        ),
                    )
                })
                .and_then(complete_pattern_occurrence)
                .map(ObjectAuditionRoute::PatternOccurrence)
        }
        ObjectRef::AudioClip(clip)
            if matches!(signal, ObjectAuditionSignal::Natural | ObjectAuditionSignal::Source) =>
        {
            Ok(ObjectAuditionRoute::ArrangementClip(*clip))
        }
        ObjectRef::Finding(_) | ObjectRef::Explanation(_) | ObjectRef::Comparison(_) => {
            let signal = match signal {
                ObjectAuditionSignal::Natural => ObjectAuditionSignal::Construction,
                signal => signal,
            };
            Ok(ObjectAuditionRoute::Investigation {
                object: object.clone(),
                signal,
            })
        }
        ObjectRef::Material(_)
        | ObjectRef::Sample(_)
        | ObjectRef::Instrument(_)
        | ObjectRef::Pad(_)
        | ObjectRef::Pattern(_)
        | ObjectRef::PatternOccurrence(_)
        | ObjectRef::AudioClip(_) => Err((
            ObjectActionRefusalReason::UnsupportedSignal,
            format!(
                "{} does not provide a {signal:?} audition layer",
                object.address()
            ),
        )),
        ObjectRef::Track(_)
        | ObjectRef::Bus(_)
        | ObjectRef::Automation(_)
        | ObjectRef::AutomationOccurrence(_)
        | ObjectRef::Reading(_) => Err((
            ObjectActionRefusalReason::NoAudibleSignal,
            format!(
                "{} has no direct audible signal; reveal a clip, pad, pattern occurrence, explanation, or comparison instead",
                object.address()
            ),
        )),
    }
}

fn complete_pattern_occurrence(
    occurrence: PatternOccurrenceRef,
) -> Result<AuditionPatternOccurrence, (ObjectActionRefusalReason, String)> {
    match (occurrence.sequencer_clip, occurrence.pattern) {
        (Some(sequencer_clip), Some(pattern)) => Ok(AuditionPatternOccurrence {
            arrangement_clip: occurrence.arrangement_clip,
            sequencer_clip,
            pattern,
        }),
        _ => Err((
            ObjectActionRefusalReason::NeedsAudibleOccurrence,
            format!(
                "{} does not retain the complete arrangement/sequencer/pattern binding required for audition",
                ObjectRef::PatternOccurrence(occurrence).address()
            ),
        )),
    }
}

/// Ordered structural parents followed by receipt-related objects. This is a
/// product relationship, not an existence claim; checked routing still asks
/// the caller's current authority about every candidate.
pub(crate) fn action_predecessors(request: &RevealRequest) -> Vec<ObjectRef> {
    let mut candidates = Vec::new();
    match &request.object {
        ObjectRef::Pad(pad) => {
            candidates.push(ObjectRef::Instrument(InstrumentRef::SampleKit(pad.kit)))
        }
        ObjectRef::PatternOccurrence(occurrence) => {
            if let Some(pattern) = occurrence.pattern {
                candidates.push(ObjectRef::Pattern(pattern));
            }
        }
        ObjectRef::AutomationOccurrence(occurrence) => {
            candidates.push(ObjectRef::Automation(occurrence.lane));
        }
        ObjectRef::Sample(SourceMaterialRef::Asset(asset)) => {
            candidates.push(ObjectRef::Material(*asset));
        }
        ObjectRef::Sample(SourceMaterialRef::VirtualSlice(slice)) => {
            candidates.push(ObjectRef::Material(slice.source_asset));
        }
        _ => {}
    }
    candidates.extend(request.related.iter().cloned());
    deduplicate_related(&request.object, candidates)
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObjectAddressError {
    NonStringExtension,
    Malformed(String),
    Invalid(ObjectKind),
}

impl std::fmt::Display for ObjectAddressError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonStringExtension => write!(
                formatter,
                "workspace navigation object extension is not a string"
            ),
            Self::Malformed(address) => {
                write!(formatter, "malformed workspace product address `{address}`")
            }
            Self::Invalid(kind) => write!(formatter, "invalid {kind:?} workspace product identity"),
        }
    }
}

impl std::error::Error for ObjectAddressError {}

/// Recover the product identity retained by a durable workspace descriptor.
///
/// The private extension remains the authoritative round-trip path. Typed
/// builtin target/state fallbacks cover descriptors created before product
/// navigation landed. A generic Arrangement/Analysis/Inspector target returns
/// `Ok(None)` because it cannot honestly name one product object.
pub fn object_from_descriptor(
    descriptor: &WorkspaceViewDescriptor,
) -> Result<Option<ObjectRef>, ObjectAddressError> {
    if let Some(value) = descriptor.extensions.get(NAVIGATION_OBJECT) {
        let address = value
            .as_str()
            .ok_or(ObjectAddressError::NonStringExtension)?;
        return parse_object_address(address).map(Some);
    }
    let object = match &descriptor.target {
        EditorTarget::PatternDefinition { id } => {
            Some(ObjectRef::Pattern(PatternId::from_raw(*id)))
        }
        EditorTarget::AutomationLane { id } => {
            Some(ObjectRef::Automation(AutomationLaneId::from_raw(*id)))
        }
        EditorTarget::Mixer { bus_id: Some(id) } => Some(ObjectRef::Bus(BusId::from_raw(*id))),
        EditorTarget::Render {
            comparison_id: Some(id),
        } => Some(ObjectRef::Comparison(ComparisonId(*id))),
        EditorTarget::Extension { namespace, key } if namespace == EXTENSION_NAMESPACE => {
            parse_extension_target(key)?
        }
        EditorTarget::Assets => match &descriptor.state {
            EditorViewState::Browser {
                selected_asset_id: Some(id),
                ..
            } => Some(ObjectRef::Material(AssetId(*id))),
            _ => None,
        },
        _ => None,
    };
    match object {
        Some(object) if valid_object(&object) => Ok(Some(object)),
        Some(object) => Err(ObjectAddressError::Invalid(object.kind())),
        None => Ok(None),
    }
}

fn parse_extension_target(key: &str) -> Result<Option<ObjectRef>, ObjectAddressError> {
    if key == "active-kit" {
        return Ok(None);
    }
    if let Some(raw) = key.strip_prefix("kit:") {
        return parse_u64(raw, key).map(|id| {
            Some(ObjectRef::Instrument(InstrumentRef::SampleKit(
                KitId::from_raw(id),
            )))
        });
    }
    if key.starts_with("explanation:") || key.starts_with("reading:") {
        return parse_object_address(key).map(Some);
    }
    Ok(None)
}

fn parse_object_address(address: &str) -> Result<ObjectRef, ObjectAddressError> {
    let parts: Vec<_> = address.split(':').collect();
    let object = match parts.as_slice() {
        ["material", asset] => ObjectRef::Material(AssetId(parse_u64(asset, address)?)),
        ["sample", "asset", asset] => ObjectRef::Sample(SourceMaterialRef::Asset(AssetId(
            parse_u64(asset, address)?,
        ))),
        ["sample", "slice", asset, start, end] => {
            let source_asset = AssetId(parse_u64(asset, address)?);
            let source_range = AssetFrameRange::new(
                SampleFrames(parse_u64(start, address)?),
                SampleFrames(parse_u64(end, address)?),
            )
            .map_err(|_| ObjectAddressError::Malformed(address.into()))?;
            let slice = VirtualSliceRef::new(source_asset, source_range)
                .map_err(|_| ObjectAddressError::Malformed(address.into()))?;
            ObjectRef::Sample(SourceMaterialRef::VirtualSlice(slice))
        }
        ["instrument", "kit", kit] => ObjectRef::Instrument(InstrumentRef::SampleKit(
            KitId::from_raw(parse_u64(kit, address)?),
        )),
        ["pad", "kit", kit, "pad", pad, "zone", zone] => {
            let zone = parse_u64(zone, address)?;
            ObjectRef::Pad(PadRef {
                kit: KitId::from_raw(parse_u64(kit, address)?),
                pad: PadId::from_raw(parse_u64(pad, address)?),
                zone: (zone != 0).then(|| ZoneId::from_raw(zone)),
            })
        }
        ["pattern", pattern] => {
            ObjectRef::Pattern(PatternId::from_raw(parse_u64(pattern, address)?))
        }
        ["pattern-occurrence", arrangement_clip, "sequencer", sequencer_clip, "pattern", pattern] =>
        {
            let sequencer_clip = parse_u64(sequencer_clip, address)?;
            let pattern = parse_u64(pattern, address)?;
            ObjectRef::PatternOccurrence(PatternOccurrenceRef {
                arrangement_clip: ClipId::from_raw(parse_u64(arrangement_clip, address)?),
                sequencer_clip: (sequencer_clip != 0)
                    .then(|| PatternClipId::from_raw(sequencer_clip)),
                pattern: (pattern != 0).then(|| PatternId::from_raw(pattern)),
            })
        }
        ["audio-clip", clip] => ObjectRef::AudioClip(ClipId::from_raw(parse_u64(clip, address)?)),
        ["track", track] => ObjectRef::Track(TrackId::from_raw(parse_u64(track, address)?)),
        ["bus", bus] => ObjectRef::Bus(BusId::from_raw(parse_u64(bus, address)?)),
        ["automation", lane] => {
            ObjectRef::Automation(AutomationLaneId::from_raw(parse_u64(lane, address)?))
        }
        ["automation-occurrence", arrangement_clip, "lane", lane] => {
            ObjectRef::AutomationOccurrence(AutomationOccurrenceRef {
                arrangement_clip: ClipId::from_raw(parse_u64(arrangement_clip, address)?),
                lane: AutomationLaneId::from_raw(parse_u64(lane, address)?),
            })
        }
        ["explanation", explanation] => {
            ObjectRef::Explanation(ExplanationId(parse_u64(explanation, address)?))
        }
        ["comparison", comparison] => {
            ObjectRef::Comparison(ComparisonId(parse_u64(comparison, address)?))
        }
        ["reading", reading] => ObjectRef::Reading(
            reading
                .parse()
                .map_err(|_| ObjectAddressError::Malformed(address.into()))?,
        ),
        parts if parts.first() == Some(&"finding") => parse_finding_address(parts, address)?,
        _ => return Err(ObjectAddressError::Malformed(address.into())),
    };
    if valid_object(&object) {
        Ok(object)
    } else {
        Err(ObjectAddressError::Invalid(object.kind()))
    }
}

fn parse_finding_address(parts: &[&str], address: &str) -> Result<ObjectRef, ObjectAddressError> {
    let kind = match parts.get(1).copied() {
        Some("rhythm") => FindingKind::Rhythm,
        Some("components") => FindingKind::Components,
        Some("separation") => FindingKind::Separation,
        Some("loom") => FindingKind::Loom,
        Some("model-claim") => FindingKind::ModelClaim,
        Some("other") => FindingKind::Other,
        _ => return Err(ObjectAddressError::Malformed(address.into())),
    };
    let (scope, local_offset) = match parts.get(2).copied() {
        Some("artifact") if parts.len() == 7 => {
            let algorithm = match parts[3] {
                "sha256" => DigestAlgorithm::Sha256,
                "blake3" => DigestAlgorithm::Blake3,
                "stable" => DigestAlgorithm::StableNonCryptographic,
                _ => return Err(ObjectAddressError::Malformed(address.into())),
            };
            let bytes = decode_hex_32(parts[4])
                .ok_or_else(|| ObjectAddressError::Malformed(address.into()))?;
            (
                FindingScope::Artifact(ArtifactId(ContentDigest::new(algorithm, bytes))),
                5,
            )
        }
        Some("derivation") if parts.len() == 6 => (
            FindingScope::Derivation(DerivationScope(
                u128::from_str_radix(parts[3], 16)
                    .map_err(|_| ObjectAddressError::Malformed(address.into()))?,
            )),
            4,
        ),
        Some("publication") if parts.len() == 8 && parts.get(4) == Some(&"asset") => (
            FindingScope::ProjectPublication {
                revision: parse_u64(parts[3], address)?,
                source: AssetId(parse_u64(parts[5], address)?),
            },
            6,
        ),
        _ => return Err(ObjectAddressError::Malformed(address.into())),
    };
    let local = match (parts.get(local_offset), parts.get(local_offset + 1)) {
        (Some(&"proposal"), Some(value)) => FindingLocalId::ReconstructionProposal(
            ReconstructionProposalId::from_raw(parse_u64(value, address)?),
        ),
        (Some(&"claim"), Some(value)) => FindingLocalId::Claim(parse_u64(value, address)?),
        _ => return Err(ObjectAddressError::Malformed(address.into())),
    };
    Ok(ObjectRef::Finding(FindingRef { kind, scope, local }))
}

fn parse_u64(value: &str, address: &str) -> Result<u64, ObjectAddressError> {
    value
        .parse()
        .map_err(|_| ObjectAddressError::Malformed(address.into()))
}

fn decode_hex_32(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(bytes)
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
    let retained_scope = descriptor
        .extensions
        .get(NAVIGATION_SCOPE)
        .and_then(Value::as_str);
    if let Some(scope) = retained_scope {
        return scope == surface.scope;
    }
    // Generic legacy targets such as Analysis(None) and active-kit are homes,
    // not identities. They must be retargeted so the exact product address is
    // persisted before an activation can count as a reveal.
    !target_is_untargeted(&descriptor.target) && descriptor.target == surface.target
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
        ObjectRef::PatternOccurrence(_)
        | ObjectRef::AutomationOccurrence(_)
        | ObjectRef::AudioClip(_)
        | ObjectRef::Track(_) => SurfaceSpec {
            kind: WorkspaceItemKind::Arrangement,
            target: EditorTarget::Arrangement,
            state: default_arrangement_state(),
            scope: "arrangement".into(),
            object: object_address,
            multiplicity: TargetMultiplicity::SingletonBySurface,
        },
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
        ObjectRef::AutomationOccurrence(occurrence) => {
            occurrence.arrangement_clip.get() != 0 && occurrence.lane.get() != 0
        }
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
    use crate::reconstruction_apply::{
        AppliedPatternBinding, AppliedPatternOccurrence, ReconstructionApplicationBindings,
    };

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
            created_pads: vec![pad(3)],
            created_zones: Vec::new(),
            pad: Some(pad(3)),
            pattern: None,
            sequencer_clip: None,
            arrangement_clip: None,
            arrangement_track: None,
            output_bus: None,
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
            created_pads: vec![pad(5)],
            created_zones: vec![crate::sample_kit::SampleTargetRef {
                kit: kit(2),
                pad: pad(5),
                zone: ZoneId::from_raw(6),
            }],
            pad: Some(pad(5)),
            pattern: None,
            sequencer_clip: None,
            arrangement_clip: None,
            arrangement_track: None,
            output_bus: None,
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
        assert!(recommendation
            .request
            .related
            .contains(&ObjectRef::Pad(PadRef {
                kit: kit(2),
                pad: pad(5),
                zone: Some(ZoneId::from_raw(6)),
            })));
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
            created_pads: vec![pad(1)],
            created_zones: Vec::new(),
            pad: Some(pad(1)),
            pattern: Some(pattern(8)),
            sequencer_clip: Some(crate::sequencer::PatternClipId::from_raw(17)),
            arrangement_clip: Some(occurrence),
            arrangement_track: None,
            output_bus: None,
            focus: ConstructivePublishedFocus::Arrangement(occurrence),
        };
        let recommendation = recommend_constructive(&publication);
        assert_eq!(
            recommendation.request.object,
            ObjectRef::PatternOccurrence(PatternOccurrenceRef {
                arrangement_clip: occurrence,
                sequencer_clip: Some(crate::sequencer::PatternClipId::from_raw(17)),
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
            derivation_scope: crate::sample_material::DerivationScope(77),
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
                        occurrence: Some(AppliedPatternOccurrence {
                            sequencer_clip: crate::sequencer::PatternClipId::from_raw(10),
                            arrangement_clip: crate::arrangement::ClipId::from_raw(11),
                            arrangement_track: crate::arrangement::TrackId::from_raw(12),
                        }),
                        source_origin_tick: 0,
                    },
                )]),
                sample_kits: BTreeMap::new(),
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
        assert!(recommendation
            .request
            .related
            .contains(&ObjectRef::PatternOccurrence(PatternOccurrenceRef {
                arrangement_clip: crate::arrangement::ClipId::from_raw(11),
                sequencer_clip: Some(crate::sequencer::PatternClipId::from_raw(10)),
                pattern: Some(promoted_pattern),
            })));
        assert!(recommendation.request.related.iter().any(|object| matches!(
            object,
            ObjectRef::Finding(FindingRef {
                scope: FindingScope::Derivation(crate::sample_material::DerivationScope(77)),
                local: FindingLocalId::ReconstructionProposal(actual),
                ..
            }) if *actual == proposal
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

    #[test]
    fn one_action_contract_routes_inspect_edit_and_audition_without_pane_guesses() {
        let document = WorkspaceDocument::default();
        let target = ObjectRef::Pad(PadRef {
            kit: kit(4),
            pad: pad(7),
            zone: Some(ZoneId::from_raw(9)),
        });

        let ObjectActionResolution::Ready(inspect) = ObjectNavigator::plan_action(
            &document,
            ObjectActionRequest::new(target.clone(), ObjectAction::Inspect),
        ) else {
            panic!("pad inspection should be routable")
        };
        assert!(matches!(
            inspect.reveal.workspace,
            WorkspaceReveal::Create(NewWorkspaceView {
                kind: WorkspaceItemKind::Inspector,
                ..
            })
        ));
        assert_eq!(inspect.reveal.selection.primary, target);
        assert_eq!(
            inspect.reveal.inspector.visibility,
            InspectorVisibility::Reveal
        );
        assert_eq!(inspect.dispatch, ObjectActionDispatch::Inspect);

        let ObjectActionResolution::Ready(edit) = ObjectNavigator::plan_action(
            &document,
            ObjectActionRequest::new(target.clone(), ObjectAction::Edit),
        ) else {
            panic!("pad edit should be routable")
        };
        assert_eq!(
            edit.dispatch,
            ObjectActionDispatch::Edit(ObjectEditRoute::Instrument {
                kit: kit(4),
                pad: Some(pad(7)),
                zone: Some(ZoneId::from_raw(9)),
            })
        );

        let ObjectActionResolution::Ready(audition) = ObjectNavigator::plan_action(
            &document,
            ObjectActionRequest::new(
                target,
                ObjectAction::Audition(ObjectAuditionSignal::Natural),
            ),
        ) else {
            panic!("pad audition should be routable")
        };
        assert_eq!(audition.reveal.workspace, WorkspaceReveal::None);
        assert_eq!(
            audition.dispatch,
            ObjectActionDispatch::Audition(ObjectAuditionRoute::Sample(
                SampleAuditionIntent::PadGate {
                    kit: kit(4),
                    pad: pad(7),
                    velocity: 1.0,
                    pressed: true,
                }
            ))
        );
    }

    #[test]
    fn checked_action_reports_stale_and_deleted_ids_without_leaking_them_into_selection() {
        let document = WorkspaceDocument::default();
        let deleted = ObjectRef::PatternOccurrence(PatternOccurrenceRef {
            arrangement_clip: ClipId::from_raw(41),
            sequencer_clip: Some(PatternClipId::from_raw(42)),
            pattern: Some(pattern(43)),
        });
        let predecessor = ObjectRef::Pattern(pattern(43));
        let request = ObjectActionRequest::new(deleted.clone(), ObjectAction::Edit)
            .at_revision(12)
            .with_related([predecessor.clone()]);

        let stale = ObjectNavigator::plan_action_checked(&document, 13, request.clone(), |_| {
            ObjectAvailability::Present
        });
        assert!(matches!(
            stale,
            ObjectActionResolution::Refused(ObjectActionRefusal {
                reason: ObjectActionRefusalReason::StalePublication {
                    expected: 12,
                    actual: 13,
                },
                ..
            })
        ));

        let resolved = ObjectNavigator::plan_action_checked(&document, 12, request, |object| {
            if object == &deleted {
                ObjectAvailability::Missing
            } else if object == &predecessor {
                ObjectAvailability::Present
            } else {
                ObjectAvailability::AuthorityUnavailable
            }
        });
        let ObjectActionResolution::Predecessor {
            deleted: actual_deleted,
            target,
            plan,
        } = resolved
        else {
            panic!("deleted occurrence should fall back to its live definition")
        };
        assert_eq!(actual_deleted, deleted);
        assert_eq!(target, predecessor);
        assert_eq!(plan.reveal.selection.primary, predecessor);
        assert!(!plan.reveal.selection.related.contains(&deleted));
        assert_eq!(
            plan.dispatch,
            ObjectActionDispatch::Edit(ObjectEditRoute::Pattern(pattern(43)))
        );
    }

    #[test]
    fn extraction_and_promotion_receipts_keep_audible_and_evidence_context() {
        let occurrence = PatternOccurrenceRef {
            arrangement_clip: ClipId::from_raw(51),
            sequencer_clip: Some(PatternClipId::from_raw(52)),
            pattern: Some(pattern(53)),
        };
        let finding = ObjectRef::Finding(FindingRef {
            kind: FindingKind::Rhythm,
            scope: FindingScope::Derivation(DerivationScope(54)),
            local: FindingLocalId::ReconstructionProposal(ReconstructionProposalId::from_raw(55)),
        });
        let recommendation = RevealRecommendation {
            request: RevealRequest::new(
                ObjectRef::Pattern(pattern(53)),
                RevealIntent::ActivateExisting,
            )
            .at_revision(56)
            .with_related([ObjectRef::PatternOccurrence(occurrence), finding.clone()]),
            diagnostics: Vec::new(),
        };

        let action = recommendation
            .action_request(ObjectAction::Audition(ObjectAuditionSignal::Construction));
        let ObjectActionResolution::Ready(plan) =
            ObjectNavigator::plan_action_checked(&WorkspaceDocument::default(), 56, action, |_| {
                ObjectAvailability::Present
            })
        else {
            panic!("promotion result should reach shared pattern audition")
        };
        assert_eq!(
            plan.dispatch,
            ObjectActionDispatch::Audition(ObjectAuditionRoute::PatternOccurrence(
                AuditionPatternOccurrence {
                    arrangement_clip: occurrence.arrangement_clip,
                    sequencer_clip: occurrence.sequencer_clip.unwrap(),
                    pattern: occurrence.pattern.unwrap(),
                }
            ))
        );
        assert!(plan.reveal.selection.related.contains(&finding));
    }

    #[test]
    fn read_only_and_non_audible_objects_are_refused_with_distinct_reasons() {
        let document = WorkspaceDocument::default();
        let finding = ObjectRef::Finding(FindingRef {
            kind: FindingKind::Components,
            scope: FindingScope::Derivation(DerivationScope(61)),
            local: FindingLocalId::Claim(62),
        });
        assert!(matches!(
            ObjectNavigator::plan_action(
                &document,
                ObjectActionRequest::new(finding, ObjectAction::Edit)
            ),
            ObjectActionResolution::Refused(ObjectActionRefusal {
                reason: ObjectActionRefusalReason::ReadOnly,
                ..
            })
        ));
        assert!(matches!(
            ObjectNavigator::plan_action(
                &document,
                ObjectActionRequest::new(
                    ObjectRef::Automation(AutomationLaneId::from_raw(63)),
                    ObjectAction::Audition(ObjectAuditionSignal::Natural),
                )
            ),
            ObjectActionResolution::Refused(ObjectActionRefusal {
                reason: ObjectActionRefusalReason::NoAudibleSignal,
                ..
            })
        ));
    }

    #[test]
    fn every_product_address_round_trips_without_erasing_typed_scope() {
        let slice = VirtualSliceRef::new(
            AssetId(2),
            AssetFrameRange::new(SampleFrames(30), SampleFrames(50)).unwrap(),
        )
        .unwrap();
        let reading = ReadingId::new([7; 16]).unwrap();
        let objects = vec![
            ObjectRef::Material(AssetId(1)),
            ObjectRef::Sample(SourceMaterialRef::Asset(AssetId(1))),
            ObjectRef::Sample(SourceMaterialRef::VirtualSlice(slice)),
            ObjectRef::Instrument(InstrumentRef::SampleKit(kit(4))),
            ObjectRef::Pad(PadRef {
                kit: kit(4),
                pad: pad(5),
                zone: Some(ZoneId::from_raw(6)),
            }),
            ObjectRef::Pattern(pattern(7)),
            ObjectRef::PatternOccurrence(PatternOccurrenceRef {
                arrangement_clip: ClipId::from_raw(8),
                sequencer_clip: Some(PatternClipId::from_raw(9)),
                pattern: Some(pattern(7)),
            }),
            ObjectRef::AudioClip(ClipId::from_raw(10)),
            ObjectRef::Track(TrackId::from_raw(11)),
            ObjectRef::Bus(BusId::from_raw(12)),
            ObjectRef::Automation(AutomationLaneId::from_raw(13)),
            ObjectRef::AutomationOccurrence(AutomationOccurrenceRef {
                arrangement_clip: ClipId::from_raw(14),
                lane: AutomationLaneId::from_raw(13),
            }),
            ObjectRef::Finding(FindingRef {
                kind: FindingKind::Rhythm,
                scope: FindingScope::Artifact(ArtifactId(ContentDigest::new(
                    DigestAlgorithm::Blake3,
                    [9; 32],
                ))),
                local: FindingLocalId::Claim(14),
            }),
            ObjectRef::Finding(FindingRef {
                kind: FindingKind::Loom,
                scope: FindingScope::Derivation(DerivationScope(15)),
                local: FindingLocalId::ReconstructionProposal(ReconstructionProposalId::from_raw(
                    16,
                )),
            }),
            ObjectRef::Finding(FindingRef {
                kind: FindingKind::Other,
                scope: FindingScope::ProjectPublication {
                    revision: 17,
                    source: AssetId(18),
                },
                local: FindingLocalId::Claim(19),
            }),
            ObjectRef::Explanation(ExplanationId(20)),
            ObjectRef::Comparison(ComparisonId(21)),
            ObjectRef::Reading(reading),
        ];
        for object in objects {
            assert_eq!(parse_object_address(&object.address()).unwrap(), object);
        }
    }

    #[test]
    fn untargeted_analysis_home_is_retargeted_before_finding_counts_as_revealed() {
        let finding = ObjectRef::Finding(FindingRef {
            kind: FindingKind::ModelClaim,
            scope: FindingScope::Derivation(DerivationScope(81)),
            local: FindingLocalId::Claim(3),
        });
        let document = WorkspaceDocument::default();
        let plan = ObjectNavigator::plan(
            &document,
            RevealRequest::new(finding.clone(), RevealIntent::ActivateExisting),
        );
        let WorkspaceReveal::Retarget { descriptor, .. } = plan.workspace else {
            panic!("generic analysis home must retain exact finding before activation")
        };
        assert_eq!(object_from_descriptor(&descriptor).unwrap(), Some(finding));
    }

    #[test]
    fn builtin_descriptor_fallbacks_recover_only_honest_product_targets() {
        let document = WorkspaceDocument::default();
        let browser = document
            .views
            .values()
            .find(|descriptor| descriptor.kind == WorkspaceItemKind::Browser);
        assert!(browser.is_none(), "default document has no browser surface");

        let mut descriptor = WorkspaceViewDescriptor {
            id: WorkspaceViewId(99),
            kind: WorkspaceItemKind::Browser,
            target: EditorTarget::Assets,
            title_override: None,
            links: ViewLinkMembership {
                group: LinkGroupId::UNLINKED,
                facets: LinkFacets::NONE,
            },
            state: EditorViewState::Browser {
                search: String::new(),
                selected_asset_id: Some(44),
            },
            extensions: BTreeMap::new(),
        };
        assert_eq!(
            object_from_descriptor(&descriptor).unwrap(),
            Some(ObjectRef::Material(AssetId(44)))
        );
        descriptor.state = EditorViewState::Browser {
            search: String::new(),
            selected_asset_id: None,
        };
        assert_eq!(object_from_descriptor(&descriptor).unwrap(), None);
    }
}
