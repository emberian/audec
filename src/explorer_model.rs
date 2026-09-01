//! GPUI-neutral Explorer and Inspector contracts.
//!
//! The Explorer is the project's object map, not a second project model and
//! not a list of backend domains. It deliberately owns only presentation
//! hierarchy, deterministic filtering, and a typed selection. The Inspector
//! derives its sections from the same immutable [`DawProject`] publication.
//! A caller must submit edits through the command/receipt boundary; neither
//! model mutates a project nor turns an unavailable analytic identity into a
//! constructive object.

use std::collections::BTreeMap;

use crate::arrangement::ClipContent;
use crate::assets::AssetId;
use crate::comparison::ComparisonId;
use crate::daw_project::DawProject;
use crate::explanation::ExplanationId;
use crate::interpretation::InterpretationStore;
use crate::project_controller::{
    AutomationOccurrenceRef, FindingRef, InstrumentRef, ObjectAction, ObjectActionRequest,
    ObjectKind, ObjectRef, PadRef, PatternOccurrenceRef, RevealIntent, RevealRequest,
};
use crate::reading::ReadingId;
use crate::reverse_surface::{ReverseSurfaceBody, ReverseSurfaceDocument};
use crate::sample_actions::named_sample_library;
use crate::sample_material::SourceMaterialRef;
use crate::sequencer::PatternId;

/// Top-level product modes. These are intentionally user-facing nouns rather
/// than the current analysis-module taxonomy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExplorerMode {
    #[default]
    Project,
    Library,
    Investigate,
    Readings,
}

impl ExplorerMode {
    pub const ALL: [Self; 4] = [
        Self::Project,
        Self::Library,
        Self::Investigate,
        Self::Readings,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Project => "Project",
            Self::Library => "Library",
            Self::Investigate => "Investigate",
            Self::Readings => "Readings",
        }
    }
}

/// Stable, opaque presentation address. It is derived from typed identity and
/// hierarchy, never used to recreate an object from a raw integer.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExplorerNodeId(String);

impl ExplorerNodeId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn root(mode: ExplorerMode) -> Self {
        Self(format!("mode:{}", mode.label().to_ascii_lowercase()))
    }

    fn category(parent: &Self, name: &str) -> Self {
        Self(format!("{}/{}", parent.0, name))
    }

    fn object(parent: &Self, object: &ObjectRef) -> Self {
        Self(format!("{}/{}", parent.0, object.address()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExplorerCategory {
    Arrangement,
    Tracks,
    Instruments,
    Patterns,
    SignalFlow,
    Automation,
    Materials,
    Samples,
    Findings,
    Explanations,
    Comparisons,
    ImportedReadings,
    Unsupported,
}

impl ExplorerCategory {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Arrangement => "Arrange",
            Self::Tracks => "Tracks",
            Self::Instruments => "Instruments",
            Self::Patterns => "Patterns",
            Self::SignalFlow => "Signal flow",
            Self::Automation => "Automation",
            Self::Materials => "Materials",
            Self::Samples => "Samples",
            Self::Findings => "Findings",
            Self::Explanations => "Explanations",
            Self::Comparisons => "Compare",
            Self::ImportedReadings => "Imported readings",
            Self::Unsupported => "Needs a surface",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExplorerTarget {
    Mode(ExplorerMode),
    Category(ExplorerCategory),
    Object(ObjectRef),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplorerDiagnosticCode {
    Empty,
    MissingObject,
    StaleSelection,
    UnsupportedObject,
    FilterNoMatches,
    NotSelectable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplorerDiagnostic {
    pub code: ExplorerDiagnosticCode,
    pub message: String,
}

impl ExplorerDiagnostic {
    fn new(code: ExplorerDiagnosticCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// An imported reading stays rooted in its own identity. `verification` is a
/// label owned by the reading/import service; this model never upgrades it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplorerReading {
    pub id: ReadingId,
    pub title: String,
    pub verification: String,
}

#[derive(Clone, Copy)]
pub struct ExplorerInput<'a> {
    pub project: &'a DawProject,
    pub findings: &'a [FindingRef],
    pub explanations: &'a [ExplanationId],
    pub comparisons: &'a [ComparisonId],
    pub readings: &'a [ExplorerReading],
}

impl<'a> ExplorerInput<'a> {
    pub fn project(project: &'a DawProject) -> Self {
        Self {
            project,
            findings: &[],
            explanations: &[],
            comparisons: &[],
            readings: &[],
        }
    }

    pub fn from_collections(
        project: &'a DawProject,
        collections: &'a ExplorerSemanticCollections,
    ) -> Self {
        Self {
            project,
            findings: &collections.findings,
            explanations: &collections.explanations,
            comparisons: &collections.comparisons,
            readings: &collections.readings,
        }
    }
}

/// Investigate/Readings identities projected from reverse-surface documents.
///
/// Order is `ObjectRef::address`. Duplicate addresses collapse. Reading
/// verification labels are copied from the reading body and never upgraded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExplorerSemanticCollections {
    pub findings: Vec<FindingRef>,
    pub explanations: Vec<ExplanationId>,
    pub comparisons: Vec<ComparisonId>,
    pub readings: Vec<ExplorerReading>,
}

impl ExplorerSemanticCollections {
    pub fn from_reverse_documents<'a>(
        docs: impl IntoIterator<Item = &'a ReverseSurfaceDocument>,
    ) -> Self {
        let mut findings = BTreeMap::new();
        let mut explanations = BTreeMap::new();
        let mut comparisons = BTreeMap::new();
        let mut readings = BTreeMap::new();
        for document in docs {
            let address = document.object.address();
            match &document.object {
                ObjectRef::Finding(finding) => {
                    findings.entry(address).or_insert(*finding);
                }
                ObjectRef::Explanation(id) => {
                    explanations.entry(address).or_insert(*id);
                }
                ObjectRef::Comparison(id) => {
                    comparisons.entry(address).or_insert(*id);
                }
                ObjectRef::Reading(id) => {
                    readings.entry(address).or_insert_with(|| ExplorerReading {
                        id: *id,
                        title: document.title.clone(),
                        verification: reading_verification_label(document),
                    });
                }
                _ => {}
            }
        }
        Self {
            findings: findings.into_values().collect(),
            explanations: explanations.into_values().collect(),
            comparisons: comparisons.into_values().collect(),
            readings: readings.into_values().collect(),
        }
    }

    /// Union explanation and comparison identities from the interpretation store.
    /// Existing reverse-document entries keep their address and are not replaced.
    pub fn include_interpretations(self, interpretations: &InterpretationStore) -> Self {
        let mut explanations: BTreeMap<_, _> = self
            .explanations
            .into_iter()
            .map(|id| (ObjectRef::Explanation(id).address(), id))
            .collect();
        for id in interpretations.explanations().keys().copied() {
            explanations
                .entry(ObjectRef::Explanation(id).address())
                .or_insert(id);
        }
        let mut comparisons: BTreeMap<_, _> = self
            .comparisons
            .into_iter()
            .map(|id| (ObjectRef::Comparison(id).address(), id))
            .collect();
        for id in interpretations.comparisons().keys().copied() {
            comparisons
                .entry(ObjectRef::Comparison(id).address())
                .or_insert(id);
        }
        Self {
            findings: self.findings,
            explanations: explanations.into_values().collect(),
            comparisons: comparisons.into_values().collect(),
            readings: self.readings,
        }
    }
}

fn reading_verification_label(document: &ReverseSurfaceDocument) -> String {
    match &document.body {
        ReverseSurfaceBody::Reading(body) => match &body.verification {
            Ok(tier) => format!("{tier:?}"),
            Err(refusal) => format!("{refusal:?}"),
        },
        ReverseSurfaceBody::Finding(_)
        | ReverseSurfaceBody::Explanation(_)
        | ReverseSurfaceBody::Comparison(_) => String::new(),
    }
}

/// A node may be a virtual mode/category or a single typed project object.
/// A diagnostic node is intentionally non-selectable: it tells the musician
/// why an object cannot yet be opened instead of pretending it has a pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplorerNode {
    pub id: ExplorerNodeId,
    pub target: ExplorerTarget,
    pub label: String,
    pub detail: Option<String>,
    pub diagnostic: Option<ExplorerDiagnostic>,
    pub children: Vec<ExplorerNode>,
}

impl ExplorerNode {
    fn mode(mode: ExplorerMode) -> Self {
        Self {
            id: ExplorerNodeId::root(mode),
            target: ExplorerTarget::Mode(mode),
            label: mode.label().into(),
            detail: None,
            diagnostic: None,
            children: Vec::new(),
        }
    }

    fn category(parent: &ExplorerNodeId, category: ExplorerCategory) -> Self {
        Self {
            id: ExplorerNodeId::category(parent, category.label()),
            target: ExplorerTarget::Category(category),
            label: category.label().into(),
            detail: None,
            diagnostic: None,
            children: Vec::new(),
        }
    }

    fn object(parent: &ExplorerNodeId, object: ObjectRef, label: impl Into<String>) -> Self {
        Self {
            id: ExplorerNodeId::object(parent, &object),
            target: ExplorerTarget::Object(object),
            label: label.into(),
            detail: None,
            diagnostic: None,
            children: Vec::new(),
        }
    }

    fn unavailable(
        parent: &ExplorerNodeId,
        label: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id: ExplorerNodeId::category(parent, "unavailable"),
            target: ExplorerTarget::Category(ExplorerCategory::Unsupported),
            label: label.into(),
            detail: None,
            diagnostic: Some(ExplorerDiagnostic::new(
                ExplorerDiagnosticCode::UnsupportedObject,
                message,
            )),
            children: Vec::new(),
        }
    }

    pub fn as_object(&self) -> Option<&ObjectRef> {
        match &self.target {
            ExplorerTarget::Object(object) => Some(object),
            ExplorerTarget::Mode(_) | ExplorerTarget::Category(_) => None,
        }
    }
}

/// Serializable UI state may retain only this selection intent. It is checked
/// against each new immutable publication, so a restored selection cannot
/// silently become the first item in a different project.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExplorerSelection {
    pub mode: ExplorerMode,
    pub filter: String,
    pub selected: Option<ExplorerNodeId>,
    pub selected_revision: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplorerSelectionResult {
    pub selection: ExplorerSelection,
    pub breadcrumb: Vec<String>,
    pub diagnostic: Option<ExplorerDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplorerModel {
    revision: u64,
    roots: BTreeMap<ExplorerMode, ExplorerNode>,
    by_id: BTreeMap<ExplorerNodeId, ExplorerTarget>,
    parents: BTreeMap<ExplorerNodeId, ExplorerNodeId>,
}

impl ExplorerModel {
    pub fn build(input: ExplorerInput<'_>) -> Self {
        let mut model = Self {
            revision: input.project.revisions().aggregate,
            roots: BTreeMap::new(),
            by_id: BTreeMap::new(),
            parents: BTreeMap::new(),
        };
        model
            .roots
            .insert(ExplorerMode::Project, project_root(input));
        model
            .roots
            .insert(ExplorerMode::Library, library_root(input));
        model
            .roots
            .insert(ExplorerMode::Investigate, investigate_root(input));
        model
            .roots
            .insert(ExplorerMode::Readings, readings_root(input));
        let roots = model.roots.values().cloned().collect::<Vec<_>>();
        for root in &roots {
            model.index_node(None, root);
        }
        model
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub fn root(&self, mode: ExplorerMode) -> &ExplorerNode {
        // Every mode is created by `build`.
        &self.roots[&mode]
    }

    pub fn node(&self, id: &ExplorerNodeId) -> Option<&ExplorerTarget> {
        self.by_id.get(id)
    }

    pub fn object_node(&self, object: &ObjectRef) -> Option<&ExplorerNodeId> {
        self.by_id.iter().find_map(|(id, target)| {
            matches!(target, ExplorerTarget::Object(candidate) if candidate == object).then_some(id)
        })
    }

    pub fn filtered(&self, mode: ExplorerMode, query: &str) -> ExplorerNode {
        let normalized = query.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return self.root(mode).clone();
        }
        filter_node(self.root(mode), &normalized).unwrap_or_else(|| ExplorerNode {
            id: self.root(mode).id.clone(),
            target: ExplorerTarget::Mode(mode),
            label: self.root(mode).label.clone(),
            detail: None,
            diagnostic: Some(ExplorerDiagnostic::new(
                ExplorerDiagnosticCode::FilterNoMatches,
                format!(
                    "No {} items match \"{}\"",
                    mode.label().to_ascii_lowercase(),
                    query
                ),
            )),
            children: Vec::new(),
        })
    }

    pub fn select(
        &self,
        mut selection: ExplorerSelection,
        id: ExplorerNodeId,
    ) -> ExplorerSelectionResult {
        let diagnostic = match self.by_id.get(&id) {
            Some(ExplorerTarget::Category(ExplorerCategory::Unsupported)) => {
                Some(ExplorerDiagnostic::new(
                    ExplorerDiagnosticCode::NotSelectable,
                    "This item has no product surface yet; its diagnostic remains visible.",
                ))
            }
            Some(_) => None,
            None => Some(ExplorerDiagnostic::new(
                ExplorerDiagnosticCode::MissingObject,
                "That explorer target is not present in the current project publication.",
            )),
        };
        if diagnostic.is_none() {
            selection.selected = Some(id.clone());
            selection.selected_revision = Some(self.revision);
        }
        ExplorerSelectionResult {
            breadcrumb: self.breadcrumb(&id),
            selection,
            diagnostic,
        }
    }

    pub fn reconcile_selection(&self, mut selection: ExplorerSelection) -> ExplorerSelectionResult {
        let Some(id) = selection.selected.clone() else {
            return ExplorerSelectionResult {
                selection,
                breadcrumb: Vec::new(),
                diagnostic: None,
            };
        };
        if self.by_id.contains_key(&id) {
            selection.selected_revision = Some(self.revision);
            return ExplorerSelectionResult {
                breadcrumb: self.breadcrumb(&id),
                selection,
                diagnostic: None,
            };
        }
        selection.selected = None;
        selection.selected_revision = Some(self.revision);
        ExplorerSelectionResult {
            selection,
            breadcrumb: Vec::new(),
            diagnostic: Some(ExplorerDiagnostic::new(
                ExplorerDiagnosticCode::StaleSelection,
                "The selected object no longer exists in this project revision.",
            )),
        }
    }

    pub fn breadcrumb(&self, id: &ExplorerNodeId) -> Vec<String> {
        let mut path = Vec::new();
        let mut current = Some(id.clone());
        while let Some(node) = current {
            if let Some(label) = self.node_label(&node) {
                path.push(label);
            }
            current = self.parents.get(&node).cloned();
        }
        path.reverse();
        path
    }

    pub fn reveal_request(
        &self,
        id: &ExplorerNodeId,
        intent: RevealIntent,
    ) -> Result<RevealRequest, ExplorerDiagnostic> {
        self.action_request(id, ObjectAction::Reveal)
            .map(|mut request| {
                request.navigation.intent = intent;
                request.navigation
            })
    }

    /// Lower every Explorer row through the same product action contract used
    /// by Inspector links and creation/promotion receipts. Reverse objects are
    /// not rejected merely because their presenter lives outside `DawProject`;
    /// their presence in this model proves the caller supplied them in the
    /// current semantic publication.
    pub fn action_request(
        &self,
        id: &ExplorerNodeId,
        action: ObjectAction,
    ) -> Result<ObjectActionRequest, ExplorerDiagnostic> {
        match self.by_id.get(id) {
            Some(ExplorerTarget::Object(object)) => {
                Ok(ObjectActionRequest::new(object.clone(), action))
            }
            Some(_) => Err(ExplorerDiagnostic::new(
                ExplorerDiagnosticCode::NotSelectable,
                "Only a durable object can receive a product action.",
            )),
            None => Err(ExplorerDiagnostic::new(
                ExplorerDiagnosticCode::MissingObject,
                "This explorer selection was removed before its action could be routed.",
            )),
        }
    }

    fn index_node(&mut self, parent: Option<&ExplorerNodeId>, node: &ExplorerNode) {
        self.by_id.insert(node.id.clone(), node.target.clone());
        if let Some(parent) = parent {
            self.parents.insert(node.id.clone(), parent.clone());
        }
        for child in &node.children {
            self.index_node(Some(&node.id), child);
        }
    }

    fn node_label(&self, id: &ExplorerNodeId) -> Option<String> {
        self.roots
            .values()
            .find_map(|root| find_node(root, id).map(|node| node.label.clone()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InspectorSectionKind {
    Identity,
    Edit,
    SoundAndRoute,
    Uses,
    OriginAndEvidence,
    History,
}

impl InspectorSectionKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Identity => "Identity",
            Self::Edit => "Edit",
            Self::SoundAndRoute => "Sound + route",
            Self::Uses => "Uses",
            Self::OriginAndEvidence => "Origin + evidence",
            Self::History => "History",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectorField {
    pub label: String,
    pub value: String,
    pub reveal: Option<ObjectRef>,
}

impl InspectorField {
    /// Inspector relationship rows are action sources, not pane names. The
    /// same related object can consequently be revealed, inspected, edited,
    /// or auditioned through `ObjectNavigator` with no destination guess in
    /// the Inspector renderer.
    pub fn action_request(&self, action: ObjectAction) -> Option<ObjectActionRequest> {
        self.reveal
            .clone()
            .map(|object| ObjectActionRequest::new(object, action))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectorSection {
    pub kind: InspectorSectionKind,
    pub fields: Vec<InspectorField>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectorReport {
    pub object: ObjectRef,
    pub title: String,
    pub sections: Vec<InspectorSection>,
    pub diagnostics: Vec<ExplorerDiagnostic>,
}

pub struct InspectorModel;

impl InspectorModel {
    pub fn inspect(project: &DawProject, object: ObjectRef) -> InspectorReport {
        let state = project.state();
        let mut report = InspectorReport {
            title: object.address(),
            object: object.clone(),
            sections: vec![identity_section(&object)],
            diagnostics: Vec::new(),
        };
        match &object {
            ObjectRef::Material(asset_id) => {
                let Some(asset) = state.domains.assets.get(*asset_id) else {
                    return missing_report(report);
                };
                report.title = asset.name().into();
                report.sections.push(section(
                    InspectorSectionKind::Edit,
                    [
                        field(
                            "Tags",
                            join_or_none(asset.tags().iter().map(String::as_str)),
                        ),
                        field("Availability", format!("{:?}", asset.availability())),
                    ],
                ));
                report.sections.push(section(
                    InspectorSectionKind::Uses,
                    material_uses(project, *asset_id),
                ));
                report.sections.push(section(
                    InspectorSectionKind::OriginAndEvidence,
                    [
                        field("Origin", format!("{:?}", asset.provenance().origin())),
                        field("Frames", asset.metadata().frame_count.0.to_string()),
                    ],
                ));
            }
            ObjectRef::Instrument(InstrumentRef::SampleKit(kit_id)) => {
                let Some(kit) = state.domains.sample_kits.kits.get(kit_id) else {
                    return missing_report(report);
                };
                report.title = kit.name.clone();
                report.sections.push(section(
                    InspectorSectionKind::Edit,
                    [
                        field("Pads", kit.pads.len().to_string()),
                        field("Revision", kit.revision.to_string()),
                    ],
                ));
                report.sections.push(section(
                    InspectorSectionKind::SoundAndRoute,
                    [field("Output bus", kit.output.bus.get().to_string())],
                ));
                report.sections.push(section(
                    InspectorSectionKind::Uses,
                    kit_pattern_uses(project, *kit_id),
                ));
            }
            ObjectRef::Pad(pad) => {
                let Some(kit) = state.domains.sample_kits.kits.get(&pad.kit) else {
                    return missing_report(report);
                };
                let Some(value) = kit.pads.get(&pad.pad) else {
                    return missing_report(report);
                };
                report.title = value.name.clone();
                report.sections.push(section(
                    InspectorSectionKind::Edit,
                    [
                        field("Kit", kit.name.clone())
                            .with_reveal(ObjectRef::Instrument(InstrumentRef::SampleKit(pad.kit))),
                        field("Zones", value.zone_order.len().to_string()),
                    ],
                ));
                report.sections.push(section(
                    InspectorSectionKind::OriginAndEvidence,
                    pad_origins(kit, pad),
                ));
            }
            ObjectRef::Pattern(pattern_id) => {
                let Some(pattern) = state.domains.sequencer.patterns().get(*pattern_id) else {
                    return missing_report(report);
                };
                report.title = pattern.name.clone();
                report.sections.push(section(
                    InspectorSectionKind::Edit,
                    [
                        field("Length ticks", pattern.length.0.to_string()),
                        field("Origin", format!("{:?}", pattern.origin)),
                    ],
                ));
                report.sections.push(section(
                    InspectorSectionKind::Uses,
                    pattern_uses(project, *pattern_id),
                ));
                report.sections.push(section(
                    InspectorSectionKind::History,
                    [
                        field("Pattern revision", pattern.revision.to_string()),
                        field(
                            "Project revision",
                            project.revisions().aggregate.to_string(),
                        ),
                    ],
                ));
            }
            ObjectRef::PatternOccurrence(occurrence) => {
                let Some(clip) = state.domains.arrangement.clip(occurrence.arrangement_clip) else {
                    return missing_report(report);
                };
                report.title = clip.name.clone();
                report.sections.push(section(
                    InspectorSectionKind::Edit,
                    [
                        field("Track", clip.track_id.get().to_string())
                            .with_reveal(ObjectRef::Track(clip.track_id)),
                        field(
                            "Placement",
                            format!(
                                "{}..{}",
                                clip.placement.start.get(),
                                clip.placement.end.get()
                            ),
                        ),
                    ],
                ));
                if let Some(pattern) = occurrence.pattern {
                    report.sections.push(section(
                        InspectorSectionKind::Uses,
                        [field("Definition", pattern.get().to_string())
                            .with_reveal(ObjectRef::Pattern(pattern))],
                    ));
                }
            }
            ObjectRef::AudioClip(clip) => {
                let Some(clip) = state.domains.arrangement.clip(*clip) else {
                    return missing_report(report);
                };
                if !matches!(&clip.content, ClipContent::Audio(_)) {
                    report.diagnostics.push(ExplorerDiagnostic::new(
                        ExplorerDiagnosticCode::UnsupportedObject,
                        "This clip is not audio; a typed automation/pattern occurrence is required instead of an AudioClip identity.",
                    ));
                    complete_section_set(&mut report);
                    return report;
                }
                report.title = clip.name.clone();
                report.sections.push(section(
                    InspectorSectionKind::Edit,
                    [
                        field("Track", clip.track_id.get().to_string())
                            .with_reveal(ObjectRef::Track(clip.track_id)),
                        field(
                            "Placement",
                            format!(
                                "{}..{}",
                                clip.placement.start.get(),
                                clip.placement.end.get()
                            ),
                        ),
                    ],
                ));
                if let ClipContent::Audio(region) = &clip.content {
                    report.sections.push(section(
                        InspectorSectionKind::OriginAndEvidence,
                        [
                            arrangement_material_field(state, region.asset),
                            field(
                                "Source frames",
                                format!("{}..{}", region.source.start, region.source.end),
                            ),
                        ],
                    ));
                }
            }
            ObjectRef::Track(track) => {
                let Some(track) = state.domains.arrangement.track(*track) else {
                    return missing_report(report);
                };
                report.title = track.name.clone();
                report.sections.push(section(
                    InspectorSectionKind::Edit,
                    [
                        field("Kind", format!("{:?}", track.kind)),
                        field("Clips", track.clip_ids.len().to_string()),
                    ],
                ));
                if let Some(bus) = state.bindings.mixer.tracks.get(&track.id) {
                    report.sections.push(section(
                        InspectorSectionKind::SoundAndRoute,
                        [field("Output bus", bus.get().to_string())
                            .with_reveal(ObjectRef::Bus(*bus))],
                    ));
                }
            }
            ObjectRef::Bus(bus) => {
                let Some(bus) = state.domains.mixer.bus(*bus) else {
                    return missing_report(report);
                };
                report.title = bus.name().into();
                report.sections.push(section(
                    InspectorSectionKind::SoundAndRoute,
                    [
                        field("Kind", format!("{:?}", bus.kind())),
                        field(
                            "Output",
                            bus.output()
                                .map_or_else(|| "Master".into(), |bus| bus.get().to_string()),
                        ),
                    ],
                ));
            }
            ObjectRef::Automation(lane) => {
                let Some(lane) = state.domains.automation.lane(*lane) else {
                    return missing_report(report);
                };
                report.title = lane.name.clone();
                report.sections.push(section(
                    InspectorSectionKind::Edit,
                    [
                        field("Points", lane.points().len().to_string()),
                        field("Enabled", lane.enabled.to_string()),
                    ],
                ));
                report.sections.push(section(
                    InspectorSectionKind::SoundAndRoute,
                    [field("Target", format!("{:?}", lane.target))],
                ));
            }
            ObjectRef::AutomationOccurrence(occurrence) => {
                let Some(clip) = state.domains.arrangement.clip(occurrence.arrangement_clip) else {
                    return missing_report(report);
                };
                let Some(lane) = state.domains.automation.lane(occurrence.lane) else {
                    return missing_report(report);
                };
                report.title = clip.name.clone();
                report.sections.push(section(
                    InspectorSectionKind::Edit,
                    [
                        field("Automation lane", lane.name.clone())
                            .with_reveal(ObjectRef::Automation(occurrence.lane)),
                        field(
                            "Placement",
                            format!(
                                "{}..{}",
                                clip.placement.start.get(),
                                clip.placement.end.get()
                            ),
                        ),
                    ],
                ));
            }
            ObjectRef::Sample(material) => {
                let named = named_sample_library(&state.domains.sample_kits)
                    .into_iter()
                    .find(|sample| sample.material == *material);
                report.title = named
                    .as_ref()
                    .map_or_else(|| "Sample material".into(), |sample| sample.name.clone());
                if let Some(sample) = named {
                    report.sections.push(section(
                        InspectorSectionKind::Edit,
                        [
                            field("Instrument", sample.instrument_name).with_reveal(
                                ObjectRef::Instrument(InstrumentRef::SampleKit(sample.target.kit)),
                            ),
                            field("Pad", sample.target.pad.get().to_string()).with_reveal(
                                ObjectRef::Pad(PadRef {
                                    kit: sample.target.kit,
                                    pad: sample.target.pad,
                                    zone: Some(sample.target.zone),
                                }),
                            ),
                        ],
                    ));
                    report.sections.push(section(
                        InspectorSectionKind::SoundAndRoute,
                        [
                            field("Audition", "Playable from its mapped pad"),
                            field("Output bus", sample.output_bus.get().to_string()),
                        ],
                    ));
                    report.sections.push(section(
                        InspectorSectionKind::OriginAndEvidence,
                        sample_origin(*material)
                            .into_iter()
                            .chain([field("Provenance", format!("{:?}", sample.provenance))]),
                    ));
                } else {
                    report.sections.push(section(
                        InspectorSectionKind::OriginAndEvidence,
                        sample_origin(*material),
                    ));
                }
            }
            ObjectRef::Finding(_)
            | ObjectRef::Explanation(_)
            | ObjectRef::Comparison(_)
            | ObjectRef::Reading(_) => {
                report.diagnostics.push(ExplorerDiagnostic::new(
                    ExplorerDiagnosticCode::UnsupportedObject,
                    "This identity is preserved, but its dedicated Inspector adapter has not landed yet.",
                ));
            }
        }
        complete_section_set(&mut report);
        report
    }
}

impl InspectorField {
    fn with_reveal(mut self, object: ObjectRef) -> Self {
        self.reveal = Some(object);
        self
    }
}

fn project_root(input: ExplorerInput<'_>) -> ExplorerNode {
    let mut root = ExplorerNode::mode(ExplorerMode::Project);
    let state = input.project.state();
    let mut arrangement = ExplorerNode::category(&root.id, ExplorerCategory::Arrangement);
    let mut tracks = ExplorerNode::category(&arrangement.id, ExplorerCategory::Tracks);
    for track_id in &state.domains.arrangement.track_order {
        let Some(track) = state.domains.arrangement.track(*track_id) else {
            continue;
        };
        let mut node =
            ExplorerNode::object(&tracks.id, ObjectRef::Track(*track_id), track.name.clone());
        for clip_id in &track.clip_ids {
            let Some(clip) = state.domains.arrangement.clip(*clip_id) else {
                continue;
            };
            match &clip.content {
                ClipContent::Pattern(region) => {
                    if let Some(object) = pattern_occurrence(state, *clip_id, region.pattern) {
                        node.children.push(ExplorerNode::object(
                            &node.id,
                            object,
                            clip.name.clone(),
                        ));
                    } else {
                        node.children.push(ExplorerNode::unavailable(
                            &ExplorerNodeId::category(
                                &node.id,
                                &format!("clip-{}", clip.id.get()),
                            ),
                            clip.name.clone(),
                            "This pattern occurrence has an incomplete binding chain and cannot be revealed as a different object type.",
                        ));
                    }
                }
                ClipContent::Audio(_) => node.children.push(ExplorerNode::object(
                    &node.id,
                    ObjectRef::AudioClip(*clip_id),
                    clip.name.clone(),
                )),
                ClipContent::Automation(region) => {
                    if let Some(lane) = state.bindings.automation.lanes.get(&region.parameter) {
                        node.children.push(ExplorerNode::object(
                            &node.id,
                            ObjectRef::AutomationOccurrence(AutomationOccurrenceRef {
                                arrangement_clip: *clip_id,
                                lane: *lane,
                            }),
                            clip.name.clone(),
                        ));
                    } else {
                        node.children.push(ExplorerNode::unavailable(
                            &ExplorerNodeId::category(
                                &node.id,
                                &format!("clip-{}", clip.id.get()),
                            ),
                            clip.name.clone(),
                            "This automation occurrence has no live lane binding and cannot be revealed as a different object type.",
                        ));
                    }
                }
            }
        }
        tracks.children.push(node);
    }
    empty_diagnostic(&mut tracks, "No tracks or clips yet");
    arrangement.children.push(tracks);
    root.children.push(arrangement);

    let mut instruments = ExplorerNode::category(&root.id, ExplorerCategory::Instruments);
    for kit in state.domains.sample_kits.kits.values() {
        let kit_ref = ObjectRef::Instrument(InstrumentRef::SampleKit(kit.id));
        let mut kit_node = ExplorerNode::object(&instruments.id, kit_ref, kit.name.clone());
        for pad in kit.ordered_pads() {
            kit_node.children.push(ExplorerNode::object(
                &kit_node.id,
                ObjectRef::Pad(PadRef {
                    kit: kit.id,
                    pad: pad.id,
                    zone: None,
                }),
                pad.name.clone(),
            ));
        }
        instruments.children.push(kit_node);
    }
    empty_diagnostic(&mut instruments, "No instruments yet");
    root.children.push(instruments);

    let mut patterns = ExplorerNode::category(&root.id, ExplorerCategory::Patterns);
    for pattern in state.domains.sequencer.patterns().patterns() {
        patterns.children.push(ExplorerNode::object(
            &patterns.id,
            ObjectRef::Pattern(pattern.id),
            pattern.name.clone(),
        ));
    }
    empty_diagnostic(&mut patterns, "No patterns yet");
    root.children.push(patterns);

    let mut signal = ExplorerNode::category(&root.id, ExplorerCategory::SignalFlow);
    for bus in state.domains.mixer.buses() {
        signal.children.push(ExplorerNode::object(
            &signal.id,
            ObjectRef::Bus(bus.id()),
            bus.name().to_owned(),
        ));
    }
    root.children.push(signal);

    let mut automation = ExplorerNode::category(&root.id, ExplorerCategory::Automation);
    for lane in state.domains.automation.lanes() {
        automation.children.push(ExplorerNode::object(
            &automation.id,
            ObjectRef::Automation(lane.id),
            lane.name.clone(),
        ));
    }
    root.children.push(automation);
    root
}

fn library_root(input: ExplorerInput<'_>) -> ExplorerNode {
    let mut root = ExplorerNode::mode(ExplorerMode::Library);
    let mut materials = ExplorerNode::category(&root.id, ExplorerCategory::Materials);
    for asset in input.project.state().domains.assets.assets().values() {
        let mut node = ExplorerNode::object(
            &materials.id,
            ObjectRef::Material(asset.id()),
            asset.name().to_owned(),
        );
        node.detail = Some(join_or_none(asset.tags().iter().map(String::as_str)));
        materials.children.push(node);
    }
    empty_diagnostic(&mut materials, "No imported or derived material yet");
    root.children.push(materials);

    let mut samples = ExplorerNode::category(&root.id, ExplorerCategory::Samples);
    for sample in named_sample_library(&input.project.state().domains.sample_kits) {
        let mut node =
            ExplorerNode::object(&samples.id, ObjectRef::Sample(sample.material), sample.name);
        node.id = ExplorerNodeId::category(
            &samples.id,
            &format!(
                "sample-{}-{}-{}",
                sample.target.kit.get(),
                sample.target.pad.get(),
                sample.target.zone.get()
            ),
        );
        node.detail = Some(format!(
            "{} · route {} · {}",
            sample.instrument_name,
            sample.output_bus.get(),
            sample_material_label(sample.material)
        ));
        samples.children.push(node);
    }
    empty_diagnostic(&mut samples, "No named samples yet");
    root.children.push(samples);
    root
}

fn investigate_root(input: ExplorerInput<'_>) -> ExplorerNode {
    let mut root = ExplorerNode::mode(ExplorerMode::Investigate);
    let mut findings = ExplorerNode::category(&root.id, ExplorerCategory::Findings);
    for finding in input.findings {
        findings.children.push(ExplorerNode::object(
            &findings.id,
            ObjectRef::Finding(*finding),
            format!("Finding · {}", finding_address(*finding)),
        ));
    }
    empty_diagnostic(&mut findings, "No findings published for this project yet");
    root.children.push(findings);

    let mut explanations = ExplorerNode::category(&root.id, ExplorerCategory::Explanations);
    for id in input.explanations {
        explanations.children.push(ExplorerNode::object(
            &explanations.id,
            ObjectRef::Explanation(*id),
            format!("Explanation {}", id.0),
        ));
    }
    empty_diagnostic(
        &mut explanations,
        "No persistent explanations available yet",
    );
    root.children.push(explanations);

    let mut comparisons = ExplorerNode::category(&root.id, ExplorerCategory::Comparisons);
    for id in input.comparisons {
        comparisons.children.push(ExplorerNode::object(
            &comparisons.id,
            ObjectRef::Comparison(*id),
            format!("Comparison {}", id.0),
        ));
    }
    empty_diagnostic(&mut comparisons, "No persistent comparisons available yet");
    root.children.push(comparisons);
    root
}

fn readings_root(input: ExplorerInput<'_>) -> ExplorerNode {
    let mut root = ExplorerNode::mode(ExplorerMode::Readings);
    let mut readings = ExplorerNode::category(&root.id, ExplorerCategory::ImportedReadings);
    for reading in input.readings {
        let mut node = ExplorerNode::object(
            &readings.id,
            ObjectRef::Reading(reading.id),
            reading.title.clone(),
        );
        node.detail = Some(reading.verification.clone());
        readings.children.push(node);
    }
    empty_diagnostic(&mut readings, "No readings imported yet");
    root.children.push(readings);
    root
}

fn empty_diagnostic(node: &mut ExplorerNode, message: &str) {
    if node.children.is_empty() {
        node.diagnostic = Some(ExplorerDiagnostic::new(
            ExplorerDiagnosticCode::Empty,
            message,
        ));
    }
}

fn pattern_occurrence(
    state: &crate::daw_project::ProjectState,
    clip: crate::arrangement::ClipId,
    alias: crate::arrangement::PatternId,
) -> Option<ObjectRef> {
    let pattern = *state.bindings.patterns.definitions.get(&alias)?;
    Some(ObjectRef::PatternOccurrence(PatternOccurrenceRef {
        arrangement_clip: clip,
        sequencer_clip: state.bindings.patterns.placements.get(&clip).copied(),
        pattern: Some(pattern),
    }))
}

fn filter_node(node: &ExplorerNode, query: &str) -> Option<ExplorerNode> {
    let mut children = node
        .children
        .iter()
        .filter_map(|child| filter_node(child, query))
        .collect::<Vec<_>>();
    let matches = node.label.to_ascii_lowercase().contains(query)
        || node
            .detail
            .as_ref()
            .is_some_and(|detail| detail.to_ascii_lowercase().contains(query))
        || node
            .as_object()
            .is_some_and(|object| object.address().contains(query));
    if matches {
        return Some(node.clone());
    }
    if children.is_empty() {
        None
    } else {
        let mut retained = node.clone();
        retained.children = std::mem::take(&mut children);
        retained.diagnostic = None;
        Some(retained)
    }
}

fn find_node<'a>(node: &'a ExplorerNode, id: &ExplorerNodeId) -> Option<&'a ExplorerNode> {
    if &node.id == id {
        return Some(node);
    }
    node.children.iter().find_map(|child| find_node(child, id))
}

fn identity_section(object: &ObjectRef) -> InspectorSection {
    section(
        InspectorSectionKind::Identity,
        [
            field("Kind", object_kind_label(object.kind())),
            field("Address", object.address()),
        ],
    )
}

fn object_kind_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Material => "Material",
        ObjectKind::Sample => "Sample",
        ObjectKind::Instrument => "Instrument",
        ObjectKind::Pad => "Pad",
        ObjectKind::Pattern => "Pattern",
        ObjectKind::PatternOccurrence => "Pattern occurrence",
        ObjectKind::AudioClip => "Audio clip",
        ObjectKind::Track => "Track",
        ObjectKind::Bus => "Bus",
        ObjectKind::Automation => "Automation",
        ObjectKind::AutomationOccurrence => "Automation occurrence",
        ObjectKind::Finding => "Finding",
        ObjectKind::Explanation => "Explanation",
        ObjectKind::Comparison => "Comparison",
        ObjectKind::Reading => "Reading",
    }
}

fn complete_section_set(report: &mut InspectorReport) {
    for kind in [
        InspectorSectionKind::Identity,
        InspectorSectionKind::Edit,
        InspectorSectionKind::SoundAndRoute,
        InspectorSectionKind::Uses,
        InspectorSectionKind::OriginAndEvidence,
        InspectorSectionKind::History,
    ] {
        if report.sections.iter().all(|section| section.kind != kind) {
            report.sections.push(section(
                kind,
                [field("Status", "No facts of this kind for this object")],
            ));
        }
    }
}

fn section(
    kind: InspectorSectionKind,
    fields: impl IntoIterator<Item = InspectorField>,
) -> InspectorSection {
    InspectorSection {
        kind,
        fields: fields.into_iter().collect(),
    }
}

fn field(label: impl Into<String>, value: impl Into<String>) -> InspectorField {
    InspectorField {
        label: label.into(),
        value: value.into(),
        reveal: None,
    }
}

fn missing_report(mut report: InspectorReport) -> InspectorReport {
    report.diagnostics.push(ExplorerDiagnostic::new(
        ExplorerDiagnosticCode::MissingObject,
        "This object is not present in the current project publication.",
    ));
    complete_section_set(&mut report);
    report
}

fn material_uses(project: &DawProject, asset: AssetId) -> Vec<InspectorField> {
    let state = project.state();
    let mut uses = Vec::new();
    for kit in state.domains.sample_kits.kits.values() {
        for pad in kit.ordered_pads() {
            if kit
                .ordered_zones(pad.id)
                .any(|zone| zone.material.asset_id() == asset)
            {
                uses.push(
                    field(format!("Pad · {}", pad.name), kit.name.clone()).with_reveal(
                        ObjectRef::Pad(PadRef {
                            kit: kit.id,
                            pad: pad.id,
                            zone: None,
                        }),
                    ),
                );
            }
        }
    }
    for track_id in &state.domains.arrangement.track_order {
        let Some(track) = state.domains.arrangement.track(*track_id) else {
            continue;
        };
        for clip_id in &track.clip_ids {
            let Some(clip) = state.domains.arrangement.clip(*clip_id) else {
                continue;
            };
            if matches!(&clip.content, ClipContent::Audio(region) if state.bindings.assets.arrangement_assets.get(&region.asset) == Some(&asset))
            {
                uses.push(
                    field(format!("Clip · {}", clip.name), track.name.clone())
                        .with_reveal(ObjectRef::AudioClip(*clip_id)),
                );
            }
        }
    }
    if uses.is_empty() {
        vec![field("Used by", "No constructive use in this project")]
    } else {
        uses
    }
}

fn arrangement_material_field(
    state: &crate::daw_project::ProjectState,
    arrangement_asset: crate::arrangement::AssetId,
) -> InspectorField {
    match state
        .bindings
        .assets
        .arrangement_assets
        .get(&arrangement_asset)
        .copied()
    {
        Some(asset) => {
            field("Material", asset.0.to_string()).with_reveal(ObjectRef::Material(asset))
        }
        None => field(
            "Material",
            "Unbound arrangement asset (cannot claim a media-pool identity)",
        ),
    }
}

fn kit_pattern_uses(project: &DawProject, kit: crate::sample_kit::KitId) -> Vec<InspectorField> {
    let mut fields = Vec::new();
    let targets = &project.state().bindings.sample_targets.targets;
    for pattern in project.state().domains.sequencer.patterns().patterns() {
        let has_kit_target = matches!(&pattern.content, crate::sequencer::PatternContent::Steps(steps) if steps.lanes.values().any(|lane| {
            matches!(lane.target, crate::sequencer::TriggerTarget::Sample(sample) if targets.get(&sample).is_some_and(|target| target.kit == kit))
        }));
        if has_kit_target {
            fields.push(
                field("Pattern", pattern.name.clone()).with_reveal(ObjectRef::Pattern(pattern.id)),
            );
        }
    }
    if fields.is_empty() {
        vec![field("Triggered by", "No patterns target this instrument")]
    } else {
        fields
    }
}

fn pad_origins(kit: &crate::sample_kit::SampleKit, pad: &PadRef) -> Vec<InspectorField> {
    let mut fields = Vec::new();
    for zone in kit.ordered_zones(pad.pad) {
        fields.push(
            field("Material", sample_material_label(zone.material))
                .with_reveal(ObjectRef::Sample(zone.material)),
        );
        fields.push(field("Provenance", format!("{:?}", zone.provenance)));
    }
    if fields.is_empty() {
        vec![field("Material", "Empty pad")]
    } else {
        fields
    }
}

fn pattern_uses(project: &DawProject, pattern: PatternId) -> Vec<InspectorField> {
    let state = project.state();
    let mut fields = Vec::new();
    for (clip, sequencer_clip) in &state.bindings.patterns.placements {
        let Some(sequencer) = state.domains.sequencer.clip(*sequencer_clip) else {
            continue;
        };
        if sequencer.pattern != pattern {
            continue;
        }
        let Some(arrangement) = state.domains.arrangement.clip(*clip) else {
            continue;
        };
        fields.push(field("Occurrence", arrangement.name.clone()).with_reveal(
            ObjectRef::PatternOccurrence(PatternOccurrenceRef {
                arrangement_clip: *clip,
                sequencer_clip: Some(*sequencer_clip),
                pattern: Some(pattern),
            }),
        ));
    }
    if fields.is_empty() {
        vec![field("Occurrences", "Not placed in Arrange")]
    } else {
        fields
    }
}

fn sample_origin(material: SourceMaterialRef) -> Vec<InspectorField> {
    match material {
        SourceMaterialRef::Asset(asset) => {
            vec![field("Material", asset.0.to_string()).with_reveal(ObjectRef::Material(asset))]
        }
        SourceMaterialRef::VirtualSlice(slice) => vec![
            field("Source material", slice.source_asset.0.to_string())
                .with_reveal(ObjectRef::Material(slice.source_asset)),
            field(
                "Exact frames",
                format!(
                    "{}..{}",
                    slice.source_range.start.0, slice.source_range.end.0
                ),
            ),
        ],
    }
}

fn sample_material_label(material: SourceMaterialRef) -> String {
    match material {
        SourceMaterialRef::Asset(asset) => format!("Material {}", asset.0),
        SourceMaterialRef::VirtualSlice(slice) => format!(
            "Slice {}..{}",
            slice.source_range.start.0, slice.source_range.end.0
        ),
    }
}

fn join_or_none<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    let values = values.into_iter().collect::<Vec<_>>();
    if values.is_empty() {
        "None".into()
    } else {
        values.join(", ")
    }
}

fn finding_address(finding: FindingRef) -> String {
    ObjectRef::Finding(finding).address()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(label: &str, object: ObjectRef) -> ExplorerNode {
        let root = ExplorerNodeId::root(ExplorerMode::Project);
        ExplorerNode::object(&root, object, label)
    }

    fn model_with_roots(roots: BTreeMap<ExplorerMode, ExplorerNode>) -> ExplorerModel {
        let mut model = ExplorerModel {
            revision: 7,
            roots,
            by_id: BTreeMap::new(),
            parents: BTreeMap::new(),
        };
        let roots = model.roots.values().cloned().collect::<Vec<_>>();
        for root in &roots {
            model.index_node(None, root);
        }
        model
    }

    #[test]
    fn filter_keeps_ancestors_and_is_case_insensitive() {
        let mut root = ExplorerNode::mode(ExplorerMode::Project);
        let mut category = ExplorerNode::category(&root.id, ExplorerCategory::Patterns);
        category.children.push(leaf(
            "Cold Hats",
            ObjectRef::Pattern(PatternId::from_raw(8)),
        ));
        root.children.push(category);
        let model = model_with_roots(BTreeMap::from([(ExplorerMode::Project, root)]));
        let filtered = model.filtered(ExplorerMode::Project, "HATS");
        assert_eq!(filtered.children[0].label, "Patterns");
        assert_eq!(filtered.children[0].children[0].label, "Cold Hats");
    }

    #[test]
    fn selection_is_typed_and_missing_selection_becomes_stale() {
        let mut root = ExplorerNode::mode(ExplorerMode::Project);
        let node = leaf("Pattern", ObjectRef::Pattern(PatternId::from_raw(3)));
        let id = node.id.clone();
        root.children.push(node);
        let model = model_with_roots(BTreeMap::from([(ExplorerMode::Project, root)]));
        let selected = model.select(ExplorerSelection::default(), id.clone());
        assert!(selected.diagnostic.is_none());
        assert_eq!(selected.breadcrumb, vec!["Project", "Pattern"]);
        let empty = model_with_roots(BTreeMap::from([(
            ExplorerMode::Project,
            ExplorerNode::mode(ExplorerMode::Project),
        )]));
        let reconciled = empty.reconcile_selection(selected.selection);
        assert_eq!(
            reconciled.diagnostic.unwrap().code,
            ExplorerDiagnosticCode::StaleSelection
        );
    }

    #[test]
    fn reveal_round_trip_never_serializes_an_untyped_id() {
        let mut root = ExplorerNode::mode(ExplorerMode::Project);
        let node = leaf("Pattern", ObjectRef::Pattern(PatternId::from_raw(12)));
        let id = node.id.clone();
        root.children.push(node);
        let model = model_with_roots(BTreeMap::from([(ExplorerMode::Project, root)]));
        let request = model
            .reveal_request(&id, RevealIntent::ActivateExisting)
            .unwrap();
        assert_eq!(request.object, ObjectRef::Pattern(PatternId::from_raw(12)));
        assert_eq!(request.intent, RevealIntent::ActivateExisting);
    }

    #[test]
    fn reverse_root_uses_the_same_typed_action_contract_as_project_objects() {
        let mut root = ExplorerNode::mode(ExplorerMode::Investigate);
        let node = ExplorerNode::object(
            &root.id,
            ObjectRef::Explanation(ExplanationId(4)),
            "Explanation 4",
        );
        let id = node.id.clone();
        root.children.push(node);
        let model = model_with_roots(BTreeMap::from([(ExplorerMode::Investigate, root)]));
        assert_eq!(
            model.action_request(&id, ObjectAction::Inspect).unwrap(),
            ObjectActionRequest::new(
                ObjectRef::Explanation(ExplanationId(4)),
                ObjectAction::Inspect,
            )
        );
        let reveal = model
            .reveal_request(&id, RevealIntent::ActivateExisting)
            .unwrap();
        assert_eq!(reveal.object, ObjectRef::Explanation(ExplanationId(4)));
    }

    #[test]
    fn inspector_relationship_exposes_all_product_verbs_without_losing_identity() {
        let field = field("Source", "Material 8").with_reveal(ObjectRef::Material(AssetId(8)));
        let request = field
            .action_request(ObjectAction::Audition(
                crate::project_controller::ObjectAuditionSignal::Source,
            ))
            .unwrap();
        assert_eq!(request.navigation.object, ObjectRef::Material(AssetId(8)));
        assert_eq!(
            request.action,
            ObjectAction::Audition(crate::project_controller::ObjectAuditionSignal::Source)
        );
    }

    #[test]
    fn empty_filter_reports_a_diagnostic_without_inventing_a_selection() {
        let model = model_with_roots(BTreeMap::from([(
            ExplorerMode::Library,
            ExplorerNode::mode(ExplorerMode::Library),
        )]));
        let filtered = model.filtered(ExplorerMode::Library, "unfindable");
        assert_eq!(
            filtered.diagnostic.unwrap().code,
            ExplorerDiagnosticCode::FilterNoMatches
        );
        assert!(filtered.children.is_empty());
    }

    fn finding_ref(claim: u64) -> FindingRef {
        FindingRef {
            kind: crate::project_controller::FindingKind::Rhythm,
            scope: crate::project_controller::FindingScope::Derivation(
                crate::sample_material::DerivationScope(42),
            ),
            local: crate::project_controller::FindingLocalId::Claim(claim),
        }
    }

    fn surface_document(object: ObjectRef, title: &str) -> ReverseSurfaceDocument {
        let finding = match &object {
            ObjectRef::Finding(finding) => *finding,
            _ => finding_ref(1),
        };
        ReverseSurfaceDocument {
            object,
            title: title.into(),
            body: ReverseSurfaceBody::Finding(crate::reverse_surface::FindingSurfaceDocument {
                finding,
                label: title.into(),
                artifact: None,
                extent: None,
                statements: Vec::new(),
            }),
            evidence: Vec::new(),
            edit_consequences: Vec::new(),
            comparisons: Vec::new(),
        }
    }

    fn reading_document(
        reading_id: ReadingId,
        title: &str,
        verification: Result<
            crate::reading::VerificationTier,
            crate::reading::ReadingVerificationRefusal,
        >,
    ) -> ReverseSurfaceDocument {
        ReverseSurfaceDocument {
            object: ObjectRef::Reading(reading_id),
            title: title.into(),
            body: ReverseSurfaceBody::Reading(crate::reverse_surface::ReadingSurfaceDocument {
                reading: crate::reading::ReadingFile {
                    format: crate::reading::READING_FORMAT.into(),
                    version: crate::reading::READING_FORMAT_VERSION,
                    reading_id,
                    revision: 1,
                    parents: Vec::new(),
                    author: crate::reading::ProvenanceDto {
                        producer: crate::reading::ProducerDto::Human { name: None },
                        created_unix_ms: None,
                        source_revision: None,
                        note: None,
                    },
                    source: crate::reading::ReadingSource {
                        fingerprints: vec![crate::reading::PortableDigest {
                            algorithm: crate::reading::PortableDigestAlgorithm::Sha256,
                            bytes: [7; 32],
                        }],
                        sample_rate: 48_000,
                        channels: 2,
                        frame_count: 100,
                        declared_title: Some(title.into()),
                        extensions: BTreeMap::new(),
                    },
                    sections: Vec::new(),
                    attachments: Vec::new(),
                    extensions: BTreeMap::new(),
                },
                verification,
            }),
            evidence: Vec::new(),
            edit_consequences: Vec::new(),
            comparisons: Vec::new(),
        }
    }

    #[test]
    fn empty_store_yields_empty_semantic_collections() {
        let store = crate::reverse_surface::ReverseSurfaceStore::new();
        let collections = ExplorerSemanticCollections::from_reverse_documents(store.documents());
        assert_eq!(collections, ExplorerSemanticCollections::default());
    }

    #[test]
    fn mixed_reverse_documents_populate_the_four_semantic_lists() {
        let finding = finding_ref(2);
        let later_finding = finding_ref(10);
        let reading_id = ReadingId::new([5; 16]).unwrap();
        let documents = [
            surface_document(ObjectRef::Comparison(ComparisonId(2)), "comparison two"),
            surface_document(ObjectRef::Explanation(ExplanationId(2)), "explanation two"),
            surface_document(ObjectRef::Finding(later_finding), "later finding"),
            surface_document(ObjectRef::Finding(finding), "earlier finding"),
            surface_document(ObjectRef::Explanation(ExplanationId(10)), "explanation ten"),
            surface_document(ObjectRef::Comparison(ComparisonId(10)), "comparison ten"),
            reading_document(
                reading_id,
                "portable reading",
                Ok(crate::reading::VerificationTier::GraphOnly),
            ),
        ];
        let collections = ExplorerSemanticCollections::from_reverse_documents(&documents);
        assert_eq!(collections.findings, vec![later_finding, finding]);
        assert_eq!(
            collections.explanations,
            vec![ExplanationId(10), ExplanationId(2)]
        );
        assert_eq!(
            collections.comparisons,
            vec![ComparisonId(10), ComparisonId(2)]
        );
        assert_eq!(
            collections.readings,
            vec![ExplorerReading {
                id: reading_id,
                title: "portable reading".into(),
                verification: "GraphOnly".into(),
            }]
        );
        assert_ne!(
            collections.readings[0].verification.to_ascii_lowercase(),
            "verified"
        );
    }

    #[test]
    fn duplicate_reverse_addresses_collapse() {
        let finding = finding_ref(7);
        let reading_id = ReadingId::new([9; 16]).unwrap();
        let first = surface_document(ObjectRef::Finding(finding), "first title");
        let second = surface_document(ObjectRef::Finding(finding), "second title");
        let first_reading = reading_document(
            reading_id,
            "kept title",
            Ok(crate::reading::VerificationTier::SourceMatched),
        );
        let second_reading = reading_document(
            reading_id,
            "dropped title",
            Err(crate::reading::ReadingVerificationRefusal::SourceNotMatched),
        );
        let collections = ExplorerSemanticCollections::from_reverse_documents([
            &first,
            &second,
            &first_reading,
            &second_reading,
            &first,
        ]);
        assert_eq!(collections.findings, vec![finding]);
        assert_eq!(
            collections.readings,
            vec![ExplorerReading {
                id: reading_id,
                title: "kept title".into(),
                verification: "SourceMatched".into(),
            }]
        );
    }

    #[test]
    fn collections_feed_investigate_and_readings_instead_of_empty_slices() {
        let project = DawProject::new("collections", 8_000, 120.0).unwrap();
        let finding = finding_ref(3);
        let reading_id = ReadingId::new([3; 16]).unwrap();
        let documents = [
            surface_document(ObjectRef::Finding(finding), "kept finding"),
            surface_document(ObjectRef::Explanation(ExplanationId(4)), "kept explanation"),
            surface_document(ObjectRef::Comparison(ComparisonId(5)), "kept comparison"),
            reading_document(
                reading_id,
                "imported reading",
                Ok(crate::reading::VerificationTier::Replicated),
            ),
        ];
        let collections = ExplorerSemanticCollections::from_reverse_documents(&documents);
        let model = ExplorerModel::build(ExplorerInput::from_collections(&project, &collections));
        let investigate = model.root(ExplorerMode::Investigate);
        assert_eq!(
            investigate.children[0].children[0].label,
            format!("Finding · {}", finding_address(finding))
        );
        assert_eq!(investigate.children[1].children[0].label, "Explanation 4");
        assert_eq!(investigate.children[2].children[0].label, "Comparison 5");
        let readings = model.root(ExplorerMode::Readings);
        assert_eq!(readings.children[0].children[0].label, "imported reading");
        assert_eq!(
            readings.children[0].children[0].detail.as_deref(),
            Some("Replicated")
        );
    }
}
