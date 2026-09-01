//! Compact GPUI media-pool browser backed directly by [`crate::assets`].
//!
//! The view keeps only ephemeral browser state (query, sort, selection). Asset
//! facts, favorites, tags, availability, provenance, and usage all come from
//! the injected shared registry, so docked and floating instances stay in sync.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use gpui::{
    div, prelude::*, px, rgb, rgba, App, Context, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, Render, SharedString, Window,
};

use crate::assets::{
    AssetAvailability, AssetAvailabilityKind, AssetId, AssetOrigin, AssetQuery, AssetRegistry,
    AssetSort, AssetUsageOwner, MediaAsset,
};
use crate::mixer::BusId;
use crate::sample_actions::{
    sample_result_provenance_label, ChopPreviewIntent, MakeBeatIntent, MakeBeatResultFocus,
    MaterialPoolSnapshot, NamedSampleAsset, OnsetChopPreview, SampleAction, SampleActionCallback,
    SampleActionError, SampleActionResult, SampleActionTracker, SampleAuditionIntent,
    SampleChopIntent, SampleDispatchReceipt, SampleFeedbackTone, SampleFocusCallback,
    SampleKitDestination, SamplePublishedResult, SampleRequestId, SampleResultFocus,
    SampleSelection, SampleViewOutcome, SamplerViewDisposition,
};
use crate::sample_kit::SampleTargetRef;
use crate::sample_material::{SampleMaterialProvenance, SourceMaterialRef};
use crate::ui_drag::AssetDrag;

const BACKGROUND: u32 = 0x090b10;
const PANEL: u32 = 0x10141d;
const PANEL_ALT: u32 = 0x0d1118;
const BORDER: u32 = 0x252c38;
const TEXT: u32 = 0xe8edf5;
const MUTED: u32 = 0x8c98a9;
const DIM: u32 = 0x596579;
const CYAN: u32 = 0x50d8d7;
const MAGENTA: u32 = 0xf172b6;
const AMBER: u32 = 0xf6b760;
const LIME: u32 = 0xa7d877;

/// The semantic gesture emitted to the host. The browser intentionally does
/// not own a decoder or transport; the project controller decides what
/// activation and momentary audition mean in the current workspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetBrowserEvent {
    Activate(AssetId),
    Audition(AssetId),
}

pub type AssetBrowserCallback = Arc<dyn Fn(AssetBrowserEvent) + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BrowserAvailability {
    #[default]
    All,
    Present,
    Missing,
    Relinked,
}

impl BrowserAvailability {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Present,
            Self::Present => Self::Missing,
            Self::Missing => Self::Relinked,
            Self::Relinked => Self::All,
        }
    }

    fn query_kind(self) -> Option<AssetAvailabilityKind> {
        match self {
            Self::All => None,
            Self::Present => Some(AssetAvailabilityKind::Present),
            Self::Missing => Some(AssetAvailabilityKind::Missing),
            Self::Relinked => Some(AssetAvailabilityKind::Relinked),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "All files",
            Self::Present => "Online",
            Self::Missing => "Missing",
            Self::Relinked => "Relinked",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BrowserSort {
    #[default]
    NameAscending,
    NameDescending,
    DurationAscending,
    DurationDescending,
    UsageDescending,
    ImportedNewest,
}

impl BrowserSort {
    fn next(self) -> Self {
        match self {
            Self::NameAscending => Self::NameDescending,
            Self::NameDescending => Self::DurationAscending,
            Self::DurationAscending => Self::DurationDescending,
            Self::DurationDescending => Self::UsageDescending,
            Self::UsageDescending => Self::ImportedNewest,
            Self::ImportedNewest => Self::NameAscending,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::NameAscending => "Name ↑",
            Self::NameDescending => "Name ↓",
            Self::DurationAscending => "Duration ↑",
            Self::DurationDescending => "Duration ↓",
            Self::UsageDescending => "Uses ↓",
            Self::ImportedNewest => "Imported ↓",
        }
    }
}

/// Pure, serializable-in-spirit view state. It is public so a workspace can
/// persist browser tabs without coupling persistence to GPUI entities.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AssetBrowserState {
    pub search: String,
    pub tag: Option<String>,
    pub favorites_only: bool,
    pub availability: BrowserAvailability,
    pub sort: BrowserSort,
    pub selected: Option<AssetId>,
}

impl AssetBrowserState {
    pub fn filtered_ids(&self, registry: &AssetRegistry) -> Vec<AssetId> {
        let mut tags_all = BTreeSet::new();
        if let Some(tag) = self.tag.as_ref() {
            tags_all.insert(tag.clone());
        }
        let query = AssetQuery {
            text: (!self.search.trim().is_empty()).then(|| self.search.clone()),
            tags_all,
            favorite: self.favorites_only.then_some(true),
            availability: self.availability.query_kind(),
            ..AssetQuery::default()
        };
        let registry_sort = match self.sort {
            BrowserSort::NameAscending => AssetSort::NameAscending,
            BrowserSort::NameDescending => AssetSort::NameDescending,
            BrowserSort::DurationAscending => AssetSort::FrameCountAscending,
            BrowserSort::DurationDescending => AssetSort::FrameCountDescending,
            BrowserSort::UsageDescending | BrowserSort::ImportedNewest => AssetSort::IdAscending,
        };
        let mut ids = registry.search(&query, registry_sort).unwrap_or_default();
        match self.sort {
            BrowserSort::UsageDescending => ids.sort_by(|left, right| {
                registry
                    .get(*right)
                    .map(|asset| asset.usages().len())
                    .cmp(&registry.get(*left).map(|asset| asset.usages().len()))
                    .then_with(|| left.cmp(right))
            }),
            BrowserSort::ImportedNewest => ids.sort_by(|left, right| {
                registry
                    .get(*right)
                    .map(|asset| asset.provenance().imported_at_unix_ms())
                    .cmp(
                        &registry
                            .get(*left)
                            .map(|asset| asset.provenance().imported_at_unix_ms()),
                    )
                    .then_with(|| left.cmp(right))
            }),
            _ => {}
        }
        ids
    }

    pub fn reconcile_selection(&mut self, visible: &[AssetId]) {
        if self.selected.is_none_or(|id| !visible.contains(&id)) {
            self.selected = visible.first().copied();
        }
    }

    pub fn move_selection(&mut self, visible: &[AssetId], delta: isize) {
        if visible.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .and_then(|selected| visible.iter().position(|id| *id == selected))
            .unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, visible.len() as isize - 1) as usize;
        self.selected = Some(visible[next]);
    }
}

pub struct AssetBrowserView {
    registry: Arc<Mutex<AssetRegistry>>,
    state: AssetBrowserState,
    callback: Option<AssetBrowserCallback>,
    sample_callback: Option<SampleActionCallback>,
    sample_focus_callback: Option<SampleFocusCallback>,
    sample_actions: SampleActionTracker,
    audition_status: Option<SampleAuditionIntent>,
    last_publication: Option<SamplePublishedResult>,
    instrument_samples: Vec<NamedSampleAsset>,
    selected_instrument_sample: Option<SampleTargetRef>,
    material_pool_revision: Option<u64>,
    focus_handle: FocusHandle,
    search_focused: bool,
    source_range: Option<crate::assets::AssetFrameRange>,
    chop: SampleChopIntent,
    chop_preview: Option<OnsetChopPreview>,
    make_beat_target: Option<BusId>,
    status: String,
}

impl AssetBrowserView {
    pub fn new(registry: Arc<Mutex<AssetRegistry>>, cx: &mut Context<Self>) -> Self {
        Self::with_callback(registry, None, cx)
    }

    pub fn with_callback(
        registry: Arc<Mutex<AssetRegistry>>,
        callback: Option<AssetBrowserCallback>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            registry,
            state: AssetBrowserState::default(),
            callback,
            sample_callback: None,
            sample_focus_callback: None,
            sample_actions: SampleActionTracker::default(),
            audition_status: None,
            last_publication: None,
            instrument_samples: Vec::new(),
            selected_instrument_sample: None,
            material_pool_revision: None,
            focus_handle: cx.focus_handle(),
            search_focused: false,
            source_range: None,
            chop: SampleChopIntent::default(),
            chop_preview: None,
            make_beat_target: None,
            status: "Ready · Enter opens · Space auditions · / searches".into(),
        }
    }

    pub fn state(&self) -> &AssetBrowserState {
        &self.state
    }

    pub fn set_state(&mut self, state: AssetBrowserState, cx: &mut Context<Self>) {
        if self.state.selected != state.selected {
            self.source_range = None;
            self.chop_preview = None;
            self.selected_instrument_sample = None;
        }
        self.state = state;
        self.reconcile();
        cx.notify();
    }

    pub fn registry(&self) -> Arc<Mutex<AssetRegistry>> {
        Arc::clone(&self.registry)
    }

    pub fn set_callback(&mut self, callback: Option<AssetBrowserCallback>) {
        self.callback = callback;
    }

    /// Add the musician-facing semantic callback without disturbing the
    /// legacy activate/audition bridge used by the workspace shell.
    pub fn set_sample_callback(&mut self, callback: Option<SampleActionCallback>) {
        self.sample_callback = callback;
    }

    pub fn set_sample_focus_callback(&mut self, callback: Option<SampleFocusCallback>) {
        self.sample_focus_callback = callback;
    }

    /// Install the project controller's revision-pinned material projection.
    /// Imported-source rows continue to use the shared registry; authored
    /// samples retain their kit/pad/zone identity and exact source ranges.
    pub fn set_material_pool_snapshot(
        &mut self,
        snapshot: MaterialPoolSnapshot,
        cx: &mut Context<Self>,
    ) {
        self.material_pool_revision = Some(snapshot.project_revision);
        self.instrument_samples = snapshot.instrument_samples;
        self.instrument_samples.sort_by(|left, right| {
            left.instrument_name
                .cmp(&right.instrument_name)
                .then_with(|| left.target.cmp(&right.target))
        });
        if self.selected_instrument_sample.is_some_and(|target| {
            !self
                .instrument_samples
                .iter()
                .any(|sample| sample.target == target)
        }) {
            self.selected_instrument_sample = None;
        }
        cx.notify();
    }

    pub fn selected_instrument_sample(&self) -> Option<&NamedSampleAsset> {
        let target = self.selected_instrument_sample?;
        self.instrument_samples
            .iter()
            .find(|sample| sample.target == target)
    }

    pub fn sample_feedback(&self) -> &crate::sample_actions::SampleActionFeedback {
        self.sample_actions.feedback()
    }

    pub fn pending_sample_action_count(&self) -> usize {
        self.sample_actions.pending_count()
    }

    pub fn audition_status(&self) -> Option<SampleAuditionIntent> {
        self.audition_status
    }

    pub fn clear_audition_status(&mut self, cx: &mut Context<Self>) {
        self.audition_status = None;
        cx.notify();
    }

    pub fn last_sample_publication(&self) -> Option<&SamplePublishedResult> {
        self.last_publication.as_ref()
    }

    /// Re-emit only the controller-authored focus from the durable receipt.
    pub fn reveal_last_result(&self) -> bool {
        let Some(receipt) = self.last_publication.as_ref() else {
            return false;
        };
        if receipt.focus == SampleResultFocus::Stay {
            return false;
        }
        let Some(callback) = self.sample_focus_callback.as_ref() else {
            return false;
        };
        callback(receipt.focus);
        true
    }

    /// Deliver a result previously accepted by the session adapter. Unknown or
    /// stale IDs are ignored so an old analysis cannot overwrite a new range.
    pub fn complete_request(
        &mut self,
        request_id: SampleRequestId,
        result: SampleActionResult,
        cx: &mut Context<Self>,
    ) -> bool {
        let Ok(action) = self.sample_actions.complete(request_id, &result) else {
            return false;
        };
        self.apply_sample_outcome(action, result, cx);
        true
    }

    pub fn set_make_beat_target(&mut self, bus: Option<BusId>, cx: &mut Context<Self>) {
        self.make_beat_target = bus;
        cx.notify();
    }

    pub fn set_onset_chop_preview(
        &mut self,
        preview: Option<OnsetChopPreview>,
        cx: &mut Context<Self>,
    ) {
        self.chop_preview = preview.filter(|preview| {
            preview.is_valid()
                && self
                    .selected_sample()
                    .is_some_and(|selection| preview.is_for(selection))
        });
        self.status = self.chop_preview.as_ref().map_or_else(
            || "Onset preview cleared".into(),
            |preview| format!("Onset preview · {} boundaries", preview.boundaries.len()),
        );
        cx.notify();
    }

    pub fn selected_sample(&self) -> Option<SampleSelection> {
        self.state.selected.map(|asset| SampleSelection {
            asset,
            source_range: self.source_range,
        })
    }

    fn visible_instrument_samples(&self) -> Vec<NamedSampleAsset> {
        let query = self.state.search.trim().to_ascii_lowercase();
        self.instrument_samples
            .iter()
            .filter(|sample| {
                query.is_empty()
                    || sample.name.to_ascii_lowercase().contains(&query)
                    || sample.instrument_name.to_ascii_lowercase().contains(&query)
            })
            .cloned()
            .collect()
    }

    fn select_instrument_sample(&mut self, target: SampleTargetRef, cx: &mut Context<Self>) {
        self.selected_instrument_sample = Some(target);
        self.status = format!(
            "Selected instrument sample {}:{}:{}",
            target.kit.get(),
            target.pad.get(),
            target.zone.get()
        );
        cx.notify();
    }

    fn audition_instrument_sample(&mut self, target: SampleTargetRef, cx: &mut Context<Self>) {
        let Some(sample) = self
            .instrument_samples
            .iter()
            .find(|sample| sample.target == target)
        else {
            return;
        };
        self.dispatch_sample_action(
            SampleAction::Audition(SampleAuditionIntent::MaterialOneShot {
                material: sample.material,
                velocity: 1.0,
            }),
            cx,
        );
    }

    fn focus_instrument_sample(&mut self, target: SampleTargetRef) -> bool {
        let Some(callback) = self.sample_focus_callback.as_ref() else {
            return false;
        };
        callback(SampleResultFocus::Pad {
            kit: target.kit,
            pad: target.pad,
        });
        true
    }

    pub fn selected_drag(&self) -> Option<AssetDrag> {
        self.selected_sample().map(|selection| AssetDrag {
            asset: selection.asset,
            source_range: selection.source_range,
        })
    }

    /// Set an exact decoded-frame selection. Bounds are checked against the
    /// currently selected registry asset before the ephemeral range changes.
    pub fn set_source_range(
        &mut self,
        range: Option<crate::assets::AssetFrameRange>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if let Some(range) = range {
            let selected = self.state.selected.ok_or("no asset is selected")?;
            let registry = self.registry.lock().map_err(|_| "asset registry is busy")?;
            let asset = registry
                .get(selected)
                .ok_or("selected asset no longer exists")?;
            if !range.is_within(asset.metadata().frame_count) {
                return Err("sample range is outside decoded asset bounds".into());
            }
        }
        self.source_range = range;
        self.chop_preview = None;
        self.status = range.map_or_else(
            || "Using the full source asset".into(),
            |range| format!("Selected frames {}–{}", range.start.0, range.end.0),
        );
        cx.notify();
        Ok(())
    }

    pub fn set_search(&mut self, search: impl Into<String>, cx: &mut Context<Self>) {
        self.state.search = search.into();
        self.reconcile();
        cx.notify();
    }

    fn visible_ids(&self) -> Vec<AssetId> {
        self.registry
            .lock()
            .map(|registry| self.state.filtered_ids(&registry))
            .unwrap_or_default()
    }

    fn reconcile(&mut self) {
        let visible = self.visible_ids();
        let previous = self.state.selected;
        self.state.reconcile_selection(&visible);
        if self.state.selected != previous {
            self.source_range = None;
            self.chop_preview = None;
        }
    }

    fn select(&mut self, id: AssetId, cx: &mut Context<Self>) {
        if self.state.selected != Some(id) {
            self.source_range = None;
            self.chop_preview = None;
        }
        self.state.selected = Some(id);
        self.selected_instrument_sample = None;
        self.status = format!("Selected asset {}", id.0);
        cx.notify();
    }

    fn emit(&mut self, event: AssetBrowserEvent, cx: &mut Context<Self>) {
        self.status = match event {
            AssetBrowserEvent::Activate(id) => format!("Opening asset {}", id.0),
            AssetBrowserEvent::Audition(id) => format!("Auditioning asset {}", id.0),
        };
        if let Some(callback) = self.callback.as_ref() {
            callback(event);
        }
        cx.notify();
    }

    fn emit_selected(&mut self, audition: bool, cx: &mut Context<Self>) {
        if let Some(id) = self.state.selected {
            if audition {
                self.audition_selected(cx);
            } else {
                self.emit(AssetBrowserEvent::Activate(id), cx);
            }
        }
    }

    fn audition_selected(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.selected_sample() else {
            return;
        };
        // Prefer the exact-range seam when connected. The legacy whole-asset
        // callback remains a fallback, avoiding two simultaneous auditions.
        if self.sample_callback.is_some() {
            self.dispatch_sample_action(
                SampleAction::Audition(SampleAuditionIntent::MaterialOneShot {
                    material: selection.material(),
                    velocity: 1.0,
                }),
                cx,
            );
            return;
        }
        self.emit(AssetBrowserEvent::Audition(selection.asset), cx);
    }

    fn select_middle_half(&mut self, cx: &mut Context<Self>) {
        let frame_count = self.state.selected.and_then(|id| {
            self.registry
                .lock()
                .ok()
                .and_then(|registry| registry.get(id).map(|asset| asset.metadata().frame_count.0))
        });
        let Some(frame_count) = frame_count else {
            return;
        };
        let quarter = frame_count / 4;
        let start = crate::assets::SampleFrames(quarter);
        let end = crate::assets::SampleFrames(frame_count.saturating_sub(quarter).max(quarter + 1));
        if let Ok(range) = crate::assets::AssetFrameRange::new(start, end) {
            let _ = self.set_source_range(Some(range), cx);
        }
    }

    fn cycle_chop(&mut self, cx: &mut Context<Self>) {
        self.chop = match self.chop {
            SampleChopIntent::OneShot => SampleChopIntent::EqualSlices { count: 4 },
            SampleChopIntent::EqualSlices { count: 4 } => {
                SampleChopIntent::EqualSlices { count: 8 }
            }
            SampleChopIntent::EqualSlices { count: 8 } => {
                SampleChopIntent::EqualSlices { count: 16 }
            }
            SampleChopIntent::EqualSlices { .. } => SampleChopIntent::DetectOnsets {
                analyzer: "project-default-onset".into(),
                sensitivity: 0.62,
                minimum_gap_frames: 1_024,
            },
            SampleChopIntent::DetectOnsets { .. } => SampleChopIntent::OneShot,
        };
        if !self.chop.is_previewable() {
            self.chop_preview = None;
        }
        self.status = format!("Chop mode: {}", chop_label(&self.chop));
        cx.notify();
    }

    fn preview_chop(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.selected_sample() else {
            return;
        };
        if !self.chop.is_previewable() {
            self.status = "Choose CHOP ONSETS before requesting a preview".into();
        } else if self.sample_callback.is_some() {
            self.dispatch_sample_action(
                SampleAction::PreviewChop(ChopPreviewIntent {
                    source,
                    chop: self.chop.clone(),
                }),
                cx,
            );
        } else {
            let action = SampleAction::PreviewChop(ChopPreviewIntent {
                source,
                chop: self.chop.clone(),
            });
            self.sample_actions.disconnect(&action);
        }
        cx.notify();
    }

    fn make_beat(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.selected_sample() else {
            return;
        };
        if self.sample_callback.is_some() {
            self.dispatch_sample_action(
                SampleAction::MakeBeat(MakeBeatIntent {
                    source,
                    chop: self.chop.clone(),
                    kit: SampleKitDestination::NewKit,
                    target_bus: self.make_beat_target,
                    bars: 2,
                    quantize_ticks: 240,
                    result_focus: MakeBeatResultFocus::Sampler(SamplerViewDisposition::OpenNew),
                }),
                cx,
            );
        } else {
            let action = SampleAction::MakeBeat(MakeBeatIntent {
                source,
                chop: self.chop.clone(),
                kit: SampleKitDestination::NewKit,
                target_bus: self.make_beat_target,
                bars: 2,
                quantize_ticks: 240,
                result_focus: MakeBeatResultFocus::Sampler(SamplerViewDisposition::OpenNew),
            });
            self.sample_actions.disconnect(&action);
        }
        cx.notify();
    }

    fn dispatch_sample_action(&mut self, action: SampleAction, cx: &mut Context<Self>) {
        let request = self.sample_actions.prepare(action);
        let Some(callback) = self.sample_callback.as_ref() else {
            self.sample_actions.disconnect(&request.action);
            cx.notify();
            return;
        };
        match callback(request.clone()) {
            SampleDispatchReceipt::Completed(result) => {
                self.sample_actions.complete_now(&request.action, &result);
                self.apply_sample_outcome(request.action, result, cx);
            }
            SampleDispatchReceipt::Accepted {
                request_id,
                kind,
                provenance,
            } => {
                let _ = self
                    .sample_actions
                    .accept(request, request_id, kind, provenance);
                cx.notify();
            }
        }
    }

    fn apply_sample_outcome(
        &mut self,
        action: SampleAction,
        result: SampleActionResult,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(SampleViewOutcome::Audition(intent)) => {
                self.audition_status = Some(intent);
            }
            Ok(SampleViewOutcome::ChopPreview(preview)) => {
                let valid_for_selection = preview.is_valid()
                    && self
                        .selected_sample()
                        .is_some_and(|selection| preview.is_for(selection));
                if valid_for_selection {
                    self.chop_preview = Some(preview);
                } else {
                    self.sample_actions.complete_now(
                        &action,
                        &Err(SampleActionError::new(
                            "sample.stale-preview",
                            "Onset preview does not match the current exact selection",
                        )),
                    );
                }
            }
            Ok(SampleViewOutcome::Published(receipt)) => {
                let focus = receipt.focus;
                self.last_publication = Some(receipt);
                if focus != SampleResultFocus::Stay {
                    if let Some(callback) = self.sample_focus_callback.as_ref() {
                        callback(focus);
                    }
                }
            }
            Ok(SampleViewOutcome::Acknowledged { .. }) => {}
            Err(_) => {
                if matches!(action, SampleAction::Audition(_)) {
                    self.audition_status = None;
                }
            }
        }
        cx.notify();
    }

    fn toggle_favorite(&mut self, id: AssetId, cx: &mut Context<Self>) {
        let result = self
            .registry
            .lock()
            .map_err(|_| "asset registry is busy".to_owned())
            .and_then(|mut registry| {
                let favorite = registry
                    .get(id)
                    .map(|asset| asset.is_favorite())
                    .ok_or_else(|| "asset no longer exists".to_owned())?;
                registry
                    .set_favorite(id, !favorite)
                    .map_err(|error| error.to_string())?;
                Ok(!favorite)
            });
        self.status = match result {
            Ok(true) => "Added to favorites".into(),
            Ok(false) => "Removed from favorites".into(),
            Err(error) => format!("Could not update favorite: {error}"),
        };
        self.reconcile();
        cx.notify();
    }

    fn toggle_tag(&mut self, tag: String, cx: &mut Context<Self>) {
        self.state.tag = (self.state.tag.as_deref() != Some(tag.as_str())).then_some(tag);
        self.reconcile();
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        if event.keystroke.modifiers.platform && key == "f" {
            self.search_focused = true;
            window.focus(&self.focus_handle, cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.search_focused {
            match key {
                "escape" => self.search_focused = false,
                "enter" => self.search_focused = false,
                "backspace" => {
                    self.state.search.pop();
                    self.reconcile();
                }
                _ if !event.keystroke.modifiers.platform && !event.keystroke.modifiers.control => {
                    if let Some(text) = event.keystroke.key_char.as_deref() {
                        if text.chars().all(|ch| !ch.is_control()) {
                            self.state.search.push_str(text);
                            self.reconcile();
                        }
                    }
                }
                _ => return,
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }

        let visible = self.visible_ids();
        match key {
            "/" => self.search_focused = true,
            "up" => {
                self.selected_instrument_sample = None;
                self.state.move_selection(&visible, -1);
            }
            "down" => {
                self.selected_instrument_sample = None;
                self.state.move_selection(&visible, 1);
            }
            "pageup" => {
                self.selected_instrument_sample = None;
                self.state.move_selection(&visible, -8);
            }
            "pagedown" => {
                self.selected_instrument_sample = None;
                self.state.move_selection(&visible, 8);
            }
            "home" => {
                self.selected_instrument_sample = None;
                self.state.selected = visible.first().copied();
            }
            "end" => {
                self.selected_instrument_sample = None;
                self.state.selected = visible.last().copied();
            }
            "enter" => self.emit_selected(false, cx),
            "space" => self.emit_selected(true, cx),
            "f" => {
                if let Some(id) = self.state.selected {
                    self.toggle_favorite(id, cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn snapshot(&mut self) -> BrowserSnapshot {
        let Ok(registry) = self.registry.lock() else {
            return BrowserSnapshot::default();
        };
        let visible = self.state.filtered_ids(&registry);
        let previous = self.state.selected;
        self.state.reconcile_selection(&visible);
        if self.state.selected != previous {
            self.source_range = None;
            self.chop_preview = None;
        }
        let rows = visible
            .iter()
            .filter_map(|id| registry.get(*id).cloned())
            .collect();
        let selected = self.state.selected.and_then(|id| registry.get(id).cloned());
        let mut tags = registry
            .assets()
            .values()
            .flat_map(|asset| asset.tags().iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        tags.sort();
        BrowserSnapshot {
            total: registry.assets().len(),
            rows,
            selected,
            tags,
        }
    }

    fn render_row(
        &self,
        asset: MediaAsset,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = asset.id();
        let availability = availability_label(asset.availability());
        let status_color = availability_color(asset.availability());
        let duration = duration_label(
            asset.metadata().frame_count.0,
            asset.metadata().sample_rate_hz,
        );
        let tags = asset.tags().iter().cloned().collect::<Vec<_>>().join(" · ");
        let favorite = asset.is_favorite();
        let drag = AssetDrag {
            asset: id,
            source_range: selected.then_some(self.source_range).flatten(),
        };
        let drag_name: SharedString = asset.name().to_owned().into();
        div()
            .id(SharedString::from(format!("asset-row-{}", id.0)))
            .h(px(48.0))
            .flex_none()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(if selected {
                rgba(0x50d8d71a)
            } else {
                rgba(0x00000000)
            })
            .hover(|style| style.bg(rgba(0xffffff09)))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.select(id, cx);
                    if event.click_count >= 2 {
                        this.emit(AssetBrowserEvent::Activate(id), cx);
                    }
                }),
            )
            .on_drag(drag, move |source: &AssetDrag, _, _, cx| {
                let source = *source;
                let name = drag_name.clone();
                cx.new(move |_| AssetDragPreview { source, name })
            })
            .child(
                div()
                    .w(px(28.0))
                    .text_center()
                    .text_color(rgb(if favorite { AMBER } else { DIM }))
                    .child(if favorite { "★" } else { "☆" }),
            )
            .child(
                div()
                    .w(px(8.0))
                    .h(px(8.0))
                    .rounded_full()
                    .bg(rgb(status_color)),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .ml_3()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(asset.name().to_owned()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(DIM))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(if tags.is_empty() {
                                "untagged".into()
                            } else {
                                tags
                            }),
                    ),
            )
            .child(
                div()
                    .w(px(82.0))
                    .text_xs()
                    .text_color(rgb(status_color))
                    .child(availability),
            )
            .child(
                div()
                    .w(px(66.0))
                    .text_right()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(duration),
            )
            .child(
                div()
                    .w(px(48.0))
                    .text_right()
                    .pr_3()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(format!("{}×", asset.usages().len())),
            )
    }

    fn render_instrument_sample_row(
        &self,
        sample: NamedSampleAsset,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let target = sample.target;
        let drag = material_drag(sample.material);
        let drag_name: SharedString = sample.name.clone().into();
        let range = match sample.material {
            SourceMaterialRef::Asset(_) => "FULL".into(),
            SourceMaterialRef::VirtualSlice(slice) => {
                format!("{}f", slice.source_range.len().0)
            }
        };
        let provenance = material_provenance_label(&sample.provenance);
        div()
            .id(SharedString::from(format!(
                "instrument-sample-row-{}-{}-{}",
                target.kit.get(),
                target.pad.get(),
                target.zone.get()
            )))
            .h(px(52.0))
            .flex_none()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(rgb(BORDER))
            .bg(if selected {
                rgba(0xf172b61c)
            } else {
                rgba(0x00000000)
            })
            .hover(|style| style.bg(rgba(0xffffff09)))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.select_instrument_sample(target, cx);
                    if event.click_count >= 2 {
                        let _ = this.focus_instrument_sample(target);
                    }
                }),
            )
            .on_drag(drag, move |source: &AssetDrag, _, _, cx| {
                let source = *source;
                let name = drag_name.clone();
                cx.new(move |_| AssetDragPreview { source, name })
            })
            .child(
                div()
                    .id(SharedString::from(format!(
                        "instrument-sample-audition-{}",
                        target.zone.get()
                    )))
                    .w(px(36.0))
                    .text_center()
                    .text_color(rgb(MAGENTA))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.audition_instrument_sample(target, cx)
                    }))
                    .child("▶"),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .ml_3()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(sample.name),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(DIM))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(format!("{} · {}", sample.instrument_name, provenance)),
                    ),
            )
            .child(
                div()
                    .w(px(82.0))
                    .text_xs()
                    .text_color(rgb(LIME))
                    .child("PLAYABLE"),
            )
            .child(
                div()
                    .w(px(66.0))
                    .text_right()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(range),
            )
            .child(
                div()
                    .w(px(48.0))
                    .pr_3()
                    .text_right()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(format!("P{}", target.pad.get())),
            )
    }

    fn render_inspector(
        &self,
        selected: Option<MediaAsset>,
        selected_instrument_sample: Option<NamedSampleAsset>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if let Some(sample) = selected_instrument_sample {
            let target = sample.target;
            let material = match sample.material {
                SourceMaterialRef::Asset(asset) => format!("Asset {} · full source", asset.0),
                SourceMaterialRef::VirtualSlice(slice) => format!(
                    "Asset {} · exact frames {}–{}",
                    slice.source_asset.0, slice.source_range.start.0, slice.source_range.end.0
                ),
            };
            let provenance = material_provenance_label(&sample.provenance);
            return div()
                .w(px(300.0))
                .h_full()
                .flex_none()
                .flex()
                .flex_col()
                .border_l_1()
                .border_color(rgb(BORDER))
                .bg(rgb(PANEL_ALT))
                .child(
                    div()
                        .p_4()
                        .border_b_1()
                        .border_color(rgb(BORDER))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(MAGENTA))
                                .child("INSTRUMENT SAMPLE"),
                        )
                        .child(
                            div()
                                .mt_1()
                                .text_base()
                                .text_color(rgb(TEXT))
                                .child(sample.name),
                        )
                        .child(
                            div()
                                .mt_1()
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child(sample.instrument_name),
                        ),
                )
                .child(
                    div()
                        .p_4()
                        .flex()
                        .gap_2()
                        .child(
                            inspector_button("instrument-sample-preview", "▶ Exact sample")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.audition_instrument_sample(target, cx)
                                })),
                        )
                        .child(
                            inspector_button("instrument-sample-open", "Open pad ↗").on_click(
                                cx.listener(move |this, _, _, _| {
                                    let _ = this.focus_instrument_sample(target);
                                }),
                            ),
                        ),
                )
                .child(
                    div()
                        .id("instrument-sample-inspector-scroll")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .px_4()
                        .pb_4()
                        .child(inspector_section("SOURCE MATERIAL", material))
                        .child(inspector_section("PROVENANCE", provenance))
                        .child(inspector_section(
                            "PLAYABLE TARGET",
                            format!(
                                "Instrument {} · pad {} · zone {}",
                                target.kit.get(),
                                target.pad.get(),
                                target.zone.get()
                            ),
                        ))
                        .child(inspector_section(
                            "OUTPUT ROUTE",
                            format!("Bus {}", sample.output_bus.get()),
                        )),
                );
        }
        let Some(asset) = selected else {
            return div()
                .w(px(300.0))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .border_l_1()
                .border_color(rgb(BORDER))
                .bg(rgb(PANEL_ALT))
                .text_sm()
                .text_color(rgb(DIM))
                .child("No asset selected");
        };
        let id = asset.id();
        let current_path = location_label(asset.location());
        let original_path = location_label(asset.provenance().original_location());
        let origin = origin_label(asset.provenance().origin());
        let format = format_label(&asset);
        let fingerprint = asset.content().id.to_hex();
        let fingerprint_short = format!("{}…{}", &fingerprint[..8], &fingerprint[24..]);
        let preview_boundaries = self
            .chop_preview
            .as_ref()
            .filter(|preview| {
                self.selected_sample()
                    .is_some_and(|selection| preview.is_for(selection))
            })
            .map(|preview| preview.boundaries.clone())
            .unwrap_or_default();
        let preview_summary = self.chop_preview.as_ref().map(|preview| {
            preview.diagnostic.clone().unwrap_or_else(|| {
                format!(
                    "{} onset boundaries · {}",
                    preview.boundaries.len(),
                    preview.analyzer
                )
            })
        });
        let published_summary = self.last_publication.as_ref().map(|receipt| {
            format!(
                "Created kit {} · {} pads · {} zones · revision {}",
                receipt.kit.get(),
                receipt.created_pads.len(),
                receipt.created_zones.len(),
                receipt.revision
            )
        });
        let feedback = self.sample_actions.feedback().clone();
        let pending_count = self.sample_actions.pending_count();
        let mut usages = div().flex().flex_col().gap_1();
        if asset.usages().is_empty() {
            usages = usages.child(
                div()
                    .text_xs()
                    .text_color(rgb(DIM))
                    .child("Not used in this project yet"),
            );
        } else {
            for usage in asset.usages().values() {
                usages = usages.child(
                    div()
                        .py_1()
                        .border_b_1()
                        .border_color(rgb(BORDER))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(TEXT))
                                .child(usage.label.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(DIM))
                                .child(usage_owner_label(&usage.owner)),
                        ),
                );
            }
        }
        div()
            .w(px(300.0))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL_ALT))
            .child(
                div()
                    .p_4()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(CYAN))
                            .child("ASSET INSPECTOR"),
                    )
                    .child(
                        div()
                            .mt_1()
                            .text_base()
                            .text_color(rgb(TEXT))
                            .child(asset.name().to_owned()),
                    )
                    .child(div().mt_1().text_xs().text_color(rgb(MUTED)).child(format)),
            )
            .child(
                div()
                    .p_4()
                    .flex()
                    .gap_2()
                    .child(
                        inspector_button("asset-audition", "▶ Audition").on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.state.selected = Some(id);
                                this.audition_selected(cx)
                            },
                        )),
                    )
                    .child(
                        inspector_button("asset-open", "Open ↗").on_click(cx.listener(
                            move |this, _, _, cx| this.emit(AssetBrowserEvent::Activate(id), cx),
                        )),
                    )
                    .child(
                        inspector_button(
                            "asset-favorite",
                            if asset.is_favorite() { "★" } else { "☆" },
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_favorite(id, cx))),
                    ),
            )
            .child(
                div()
                    .px_4()
                    .pb_4()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .mb_2()
                            .text_xs()
                            .text_color(rgb(CYAN))
                            .child("SAMPLE / CHOP MATERIAL"),
                    )
                    .child(selection_strip(
                        self.source_range,
                        asset.metadata().frame_count.0,
                        preview_boundaries,
                    ))
                    .child(
                        div()
                            .mt_2()
                            .flex()
                            .gap_2()
                            .child(
                                inspector_button("sample-full", "FULL").on_click(cx.listener(
                                    |this, _, _, cx| {
                                        let _ = this.set_source_range(None, cx);
                                    },
                                )),
                            )
                            .child(inspector_button("sample-middle", "MIDDLE 50%").on_click(
                                cx.listener(|this, _, _, cx| this.select_middle_half(cx)),
                            ))
                            .child(
                                inspector_button("sample-chop", chop_label(&self.chop))
                                    .on_click(cx.listener(|this, _, _, cx| this.cycle_chop(cx))),
                            )
                            .child(
                                inspector_button("sample-preview-chop", "PREVIEW")
                                    .on_click(cx.listener(|this, _, _, cx| this.preview_chop(cx))),
                            ),
                    )
                    .when_some(preview_summary, |this, summary| {
                        this.child(div().mt_2().text_xs().text_color(rgb(AMBER)).child(summary))
                    })
                    .child(
                        div()
                            .id("sample-make-beat")
                            .mt_2()
                            .h(px(30.0))
                            .px_3()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(MAGENTA))
                            .bg(rgba(0xf172b616))
                            .text_xs()
                            .text_color(rgb(TEXT))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgba(0xf172b62b)))
                            .on_click(cx.listener(|this, _, _, cx| this.make_beat(cx)))
                            .child("SAMPLE SELECTION & MAKE BEAT  →"),
                    )
                    .when(feedback.tone != SampleFeedbackTone::Idle, |this| {
                        let provenance = feedback
                            .provenance
                            .as_ref()
                            .map(sample_result_provenance_label);
                        this.child(
                            div()
                                .mt_2()
                                .p_2()
                                .rounded_sm()
                                .border_1()
                                .border_color(rgb(feedback_color(feedback.tone)))
                                .bg(rgba(feedback_background(feedback.tone)))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(feedback_color(feedback.tone)))
                                        .child(feedback.headline.clone()),
                                )
                                .when_some(feedback.detail.clone(), |this, detail| {
                                    this.child(
                                        div().mt_1().text_xs().text_color(rgb(MUTED)).child(detail),
                                    )
                                })
                                .when_some(provenance, |this, provenance| {
                                    this.child(
                                        div()
                                            .mt_1()
                                            .text_xs()
                                            .text_color(rgb(DIM))
                                            .child(provenance),
                                    )
                                })
                                .when(pending_count > 0, |this| {
                                    this.child(div().mt_1().text_xs().text_color(rgb(AMBER)).child(
                                        format!("{pending_count} sampling actions in flight"),
                                    ))
                                }),
                        )
                    })
                    .when_some(published_summary, |this, summary| {
                        this.child(
                            div()
                                .mt_2()
                                .p_2()
                                .flex()
                                .items_center()
                                .justify_between()
                                .rounded_sm()
                                .border_1()
                                .border_color(rgb(CYAN))
                                .bg(rgba(0x50d8d70d))
                                .child(div().text_xs().text_color(rgb(MUTED)).child(summary))
                                .child(
                                    inspector_button("asset-reveal-sample-result", "REVEAL ↗")
                                        .on_click(cx.listener(|this, _, _, _| {
                                            let _ = this.reveal_last_result();
                                        })),
                                ),
                        )
                    }),
            )
            .child(
                div()
                    .id("asset-inspector-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_4()
                    .pb_4()
                    .child(inspector_section("CURRENT LOCATION", current_path))
                    .child(inspector_section("ORIGIN", origin))
                    .child(inspector_section("ORIGINAL LOCATION", original_path))
                    .child(inspector_section("CONTENT ID", fingerprint_short))
                    .child(
                        div()
                            .mt_4()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(CYAN))
                                    .child(format!("USES · {}", asset.usages().len())),
                            )
                            .child(div().mt_2().child(usages)),
                    ),
            )
    }
}

impl Focusable for AssetBrowserView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AssetBrowserView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.snapshot();
        let selected_id = self.state.selected;
        let visible_instrument_samples = self.visible_instrument_samples();
        let selected_instrument_sample = self.selected_instrument_sample().cloned();
        let count = snapshot.rows.len() + visible_instrument_samples.len();
        let total = snapshot.total + self.instrument_samples.len();
        let pool_count = self.instrument_samples.len();
        let pool_revision = self.material_pool_revision;
        let feedback = self.sample_actions.feedback().clone();
        let pending_count = self.sample_actions.pending_count();
        let footer_status = if feedback.tone == SampleFeedbackTone::Idle {
            self.status.clone()
        } else if pending_count > 0 {
            format!("{} · {pending_count} in flight", feedback.headline)
        } else {
            feedback.headline
        };
        let query_text = if self.state.search.is_empty() {
            "Search name, tag, or path…".to_owned()
        } else {
            self.state.search.clone()
        };
        let mut tag_bar = div()
            .id("asset-tag-scroll")
            .flex()
            .items_center()
            .gap_1()
            .overflow_x_scroll();
        tag_bar = tag_bar.child(
            filter_chip("asset-tag-all", "All tags", self.state.tag.is_none()).on_click(
                cx.listener(|this, _, _, cx| {
                    this.state.tag = None;
                    this.reconcile();
                    cx.notify();
                }),
            ),
        );
        for tag in snapshot.tags {
            let active = self.state.tag.as_deref() == Some(tag.as_str());
            let chip_tag = tag.clone();
            tag_bar = tag_bar.child(
                filter_chip(SharedString::from(format!("asset-tag-{tag}")), tag, active).on_click(
                    cx.listener(move |this, _, _, cx| this.toggle_tag(chip_tag.clone(), cx)),
                ),
            );
        }
        let mut rows = div().flex().flex_col();
        if snapshot.rows.is_empty() && visible_instrument_samples.is_empty() {
            rows = rows.child(
                div()
                    .h(px(180.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(DIM))
                    .child(div().text_sm().child("No samples match this view"))
                    .child(
                        div()
                            .mt_1()
                            .text_xs()
                            .child("Clear search or cycle the filters"),
                    ),
            );
        } else {
            for asset in snapshot.rows {
                let selected = selected_id == Some(asset.id());
                rows = rows.child(self.render_row(asset, selected, cx));
            }
            if !visible_instrument_samples.is_empty() {
                rows = rows.child(
                    div()
                        .h(px(28.0))
                        .flex_none()
                        .px_3()
                        .flex()
                        .items_center()
                        .justify_between()
                        .border_b_1()
                        .border_color(rgb(BORDER))
                        .bg(rgba(0xf172b610))
                        .text_xs()
                        .text_color(rgb(MAGENTA))
                        .child("INSTRUMENT SAMPLES")
                        .child(format!("{} playable", visible_instrument_samples.len())),
                );
                for sample in visible_instrument_samples {
                    let selected = self.selected_instrument_sample == Some(sample.target);
                    rows = rows.child(self.render_instrument_sample_row(sample, selected, cx));
                }
            }
        }
        div()
            .key_context("AudecAssetBrowser")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(BACKGROUND))
            .text_color(rgb(TEXT))
            .child(
                div()
                    .h(px(54.0))
                    .flex_none()
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL_ALT))
                    .child(
                        div()
                            .id("asset-search")
                            .h(px(30.0))
                            .min_w(px(180.0))
                            .flex_1()
                            .px_3()
                            .flex()
                            .items_center()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(if self.search_focused { CYAN } else { BORDER }))
                            .bg(rgb(PANEL))
                            .cursor_text()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.search_focused = true;
                                window.focus(&this.focus_handle, cx);
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(if self.state.search.is_empty() {
                                        DIM
                                    } else {
                                        TEXT
                                    }))
                                    .child(query_text),
                            ),
                    )
                    .child(
                        filter_chip("asset-favorites", "★ Favorites", self.state.favorites_only)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.favorites_only = !this.state.favorites_only;
                                this.reconcile();
                                cx.notify();
                            })),
                    )
                    .child(
                        filter_chip(
                            "asset-availability",
                            self.state.availability.label(),
                            self.state.availability != BrowserAvailability::All,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.state.availability = this.state.availability.next();
                            this.reconcile();
                            cx.notify();
                        })),
                    )
                    .child(
                        filter_chip("asset-sort", self.state.sort.label(), false).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.state.sort = this.state.sort.next();
                                this.reconcile();
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .child(
                div()
                    .h(px(34.0))
                    .flex_none()
                    .px_3()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL))
                    .child(tag_bar),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .h(px(27.0))
                                    .flex_none()
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .border_b_1()
                                    .border_color(rgb(BORDER))
                                    .bg(rgb(PANEL_ALT))
                                    .text_xs()
                                    .text_color(rgb(DIM))
                                    .child(div().w(px(36.0)).child("FAV"))
                                    .child(div().flex_1().child("NAME / TAGS"))
                                    .child(div().w(px(82.0)).child("STATUS"))
                                    .child(div().w(px(66.0)).text_right().child("LENGTH"))
                                    .child(div().w(px(48.0)).pr_3().text_right().child("USES")),
                            )
                            .child(
                                div()
                                    .id("asset-list-scroll")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .child(rows),
                            ),
                    )
                    .child(self.render_inspector(
                        snapshot.selected,
                        selected_instrument_sample,
                        cx,
                    )),
            )
            .child(
                div()
                    .h(px(27.0))
                    .flex_none()
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL_ALT))
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .text_color(rgb(if feedback.tone == SampleFeedbackTone::Idle {
                        MUTED
                    } else {
                        feedback_color(feedback.tone)
                    }))
                    .child(footer_status)
                    .child(pool_revision.map_or_else(
                        || format!("{count} shown · {total} in pool"),
                        |revision| {
                            format!(
                                "{count} shown · {total} in pool · {pool_count} instrument · r{revision}"
                            )
                        },
                    )),
            )
    }
}

#[derive(Default)]
struct BrowserSnapshot {
    total: usize,
    rows: Vec<MediaAsset>,
    selected: Option<MediaAsset>,
    tags: Vec<String>,
}

struct AssetDragPreview {
    source: AssetDrag,
    name: SharedString,
}

fn material_drag(material: SourceMaterialRef) -> AssetDrag {
    match material {
        SourceMaterialRef::Asset(asset) => AssetDrag {
            asset,
            source_range: None,
        },
        SourceMaterialRef::VirtualSlice(slice) => AssetDrag {
            asset: slice.source_asset,
            source_range: Some(slice.source_range),
        },
    }
}

fn material_provenance_label(provenance: &SampleMaterialProvenance) -> String {
    match provenance {
        SampleMaterialProvenance::ExistingAsset => "existing source".into(),
        SampleMaterialProvenance::ManualSelection => "exact selection".into(),
        SampleMaterialProvenance::OnsetChop { analyzer, evidence } => {
            format!("onset chop · {analyzer} · {} evidence", evidence.len())
        }
        SampleMaterialProvenance::Deprojection { proposal, evidence } => format!(
            "deprojection {} · {} evidence",
            proposal.local,
            evidence.len()
        ),
        SampleMaterialProvenance::AnalysisTemplate { analyzer, evidence } => {
            format!("{analyzer} template · {} evidence", evidence.len())
        }
        SampleMaterialProvenance::Consolidated(record) => format!(
            "consolidated from asset {} · {}–{}",
            record.derived_from.source_asset.0,
            record.derived_from.source_range.start.0,
            record.derived_from.source_range.end.0
        ),
    }
}

impl Render for AssetDragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let detail = self.source.source_range.map_or_else(
            || "FULL SOURCE".into(),
            |range| format!("FRAMES {}–{}", range.start.0, range.end.0),
        );
        div()
            .w(px(220.0))
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(rgb(CYAN))
            .bg(rgb(PANEL))
            .text_color(rgb(TEXT))
            .shadow_lg()
            .child(div().text_sm().child(self.name.clone()))
            .child(div().mt_1().text_xs().text_color(rgb(CYAN)).child(detail))
    }
}

fn chop_label(chop: &SampleChopIntent) -> &'static str {
    match chop {
        SampleChopIntent::OneShot => "ONE SHOT",
        SampleChopIntent::EqualSlices { count: 4 } => "CHOP ×4",
        SampleChopIntent::EqualSlices { count: 8 } => "CHOP ×8",
        SampleChopIntent::EqualSlices { count: 16 } => "CHOP ×16",
        SampleChopIntent::EqualSlices { .. } => "CHOP EVEN",
        SampleChopIntent::DetectOnsets { .. } => "CHOP ONSETS",
    }
}

fn feedback_color(tone: SampleFeedbackTone) -> u32 {
    match tone {
        SampleFeedbackTone::Idle => MUTED,
        SampleFeedbackTone::Pending => AMBER,
        SampleFeedbackTone::Success => LIME,
        SampleFeedbackTone::Error => MAGENTA,
    }
}

fn feedback_background(tone: SampleFeedbackTone) -> u32 {
    match tone {
        SampleFeedbackTone::Idle => 0x00000000,
        SampleFeedbackTone::Pending => 0xf6b76012,
        SampleFeedbackTone::Success => 0xa7d87712,
        SampleFeedbackTone::Error => 0xf172b618,
    }
}

fn selection_strip(
    selected: Option<crate::assets::AssetFrameRange>,
    frame_count: u64,
    preview_boundaries: Vec<crate::assets::SampleFrames>,
) -> impl IntoElement {
    let width = 264.0_f32;
    let frame_count = frame_count.max(1);
    let (start, end) = selected
        .map(|range| (range.start.0, range.end.0))
        .unwrap_or((0, frame_count));
    let left = (start.min(frame_count) as f64 / frame_count as f64) as f32 * width;
    let selected_width = ((end.min(frame_count) - start.min(frame_count)) as f64
        / frame_count as f64) as f32
        * width;
    div()
        .relative()
        .w(px(width))
        .h(px(44.0))
        .overflow_hidden()
        .rounded_sm()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(BACKGROUND))
        .children((0..24).map(|index| {
            let height = 6.0 + (((index * 13 + 5) % 24) as f32);
            div()
                .absolute()
                .left(px(5.0 + index as f32 * 10.7))
                .top(px(22.0 - height / 2.0))
                .w(px(3.0))
                .h(px(height))
                .rounded_full()
                .bg(rgba(0x8c98a94a))
        }))
        .children(preview_boundaries.into_iter().map(move |boundary| {
            let x = (boundary.0.min(frame_count) as f64 / frame_count as f64) as f32 * width;
            div()
                .absolute()
                .left(px(x))
                .top_0()
                .w(px(1.0))
                .h_full()
                .bg(rgb(AMBER))
        }))
        .child(
            div()
                .absolute()
                .left(px(left))
                .top_0()
                .w(px(selected_width.max(2.0)))
                .h_full()
                .border_l_1()
                .border_r_1()
                .border_color(rgb(MAGENTA))
                .bg(rgba(0xf172b625)),
        )
}

fn filter_chip(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    active: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(26.0))
        .px_2()
        .flex()
        .items_center()
        .rounded_sm()
        .border_1()
        .border_color(rgb(if active { CYAN } else { BORDER }))
        .bg(if active {
            rgba(0x50d8d71a)
        } else {
            rgba(0x00000000)
        })
        .text_xs()
        .text_color(rgb(if active { TEXT } else { MUTED }))
        .cursor_pointer()
        .hover(|style| style.bg(rgba(0xffffff0c)))
        .child(label.into())
}

fn inspector_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(27.0))
        .px_2()
        .flex()
        .items_center()
        .rounded_sm()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL))
        .text_xs()
        .text_color(rgb(TEXT))
        .cursor_pointer()
        .hover(|style| style.border_color(rgb(CYAN)))
        .child(label.into())
}

fn inspector_section(
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
) -> impl IntoElement {
    div()
        .mt_4()
        .child(div().text_xs().text_color(rgb(CYAN)).child(label.into()))
        .child(
            div()
                .mt_1()
                .text_xs()
                .text_color(rgb(MUTED))
                .child(value.into()),
        )
}

fn availability_label(availability: &AssetAvailability) -> &'static str {
    match availability {
        AssetAvailability::Present => "ONLINE",
        AssetAvailability::Missing { .. } => "MISSING",
        AssetAvailability::Relinked { .. } => "RELINKED",
    }
}

fn availability_color(availability: &AssetAvailability) -> u32 {
    match availability {
        AssetAvailability::Present => LIME,
        AssetAvailability::Missing { .. } => MAGENTA,
        AssetAvailability::Relinked { .. } => AMBER,
    }
}

fn duration_label(frames: u64, sample_rate: u32) -> String {
    let seconds = frames as f64 / f64::from(sample_rate.max(1));
    if seconds < 60.0 {
        format!("{seconds:.2}s")
    } else {
        format!("{}:{:04.1}", (seconds / 60.0) as u64, seconds % 60.0)
    }
}

fn location_label(location: &crate::assets::AssetLocation) -> String {
    location
        .project_relative
        .as_ref()
        .map(|path| path.as_str().to_owned())
        .or_else(|| {
            location
                .absolute
                .as_ref()
                .map(|path| path.as_str().to_owned())
        })
        .unwrap_or_else(|| "No resolvable route".into())
}

fn origin_label(origin: &AssetOrigin) -> String {
    match origin {
        AssetOrigin::ImportedFile { importer } => format!("Imported file · {importer}"),
        AssetOrigin::RecordedInput { device } => format!("Recorded input · {device}"),
        AssetOrigin::Rendered {
            renderer,
            source_revision,
        } => format!("Rendered · {renderer} · revision {source_revision}"),
        AssetOrigin::Generated { generator } => format!("Generated · {generator}"),
        AssetOrigin::Migrated { source_format } => format!("Migrated · {source_format}"),
    }
}

fn usage_owner_label(owner: &AssetUsageOwner) -> String {
    match owner {
        AssetUsageOwner::AudioClip { persistent_id } => format!("Audio clip · {persistent_id}"),
        AssetUsageOwner::SamplerZone { persistent_id } => format!("Sampler zone · {persistent_id}"),
        AssetUsageOwner::Step { persistent_id } => format!("Step · {persistent_id}"),
        AssetUsageOwner::AnalysisObject { persistent_id } => {
            format!("Analysis object · {persistent_id}")
        }
        AssetUsageOwner::Render { persistent_id } => format!("Render · {persistent_id}"),
        AssetUsageOwner::External {
            kind,
            persistent_id,
        } => format!("{kind} · {persistent_id}"),
    }
}

fn format_label(asset: &MediaAsset) -> String {
    let metadata = asset.metadata();
    let container = metadata.container.as_deref().unwrap_or("audio");
    let codec = metadata.codec.as_deref().unwrap_or("unknown codec");
    let depth = metadata
        .bit_depth
        .map(|bits| format!(" · {bits}-bit"))
        .unwrap_or_default();
    format!(
        "{} · {} · {} Hz · {} ch{} · {}",
        container.to_uppercase(),
        codec,
        metadata.sample_rate_hz,
        metadata.channels,
        depth,
        duration_label(metadata.frame_count.0, metadata.sample_rate_hz)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{
        AbsolutePath, AssetLocation, AssetProvenance, AssetRegistration, ContentFingerprint,
        DecodedAudioMetadata, SampleFrames,
    };
    use crate::sample_material::VirtualSliceRef;

    fn registration(name: &str, frames: u64, imported: u64, tags: &[&str]) -> AssetRegistration {
        let location = AssetLocation::new(
            Some(AbsolutePath::parse(format!("/samples/{name}.wav")).unwrap()),
            None,
        )
        .unwrap();
        AssetRegistration {
            name: name.into(),
            location: location.clone(),
            metadata: DecodedAudioMetadata {
                sample_rate_hz: 48_000,
                channels: 2,
                frame_count: SampleFrames(frames),
                container: Some("wav".into()),
                codec: Some("pcm".into()),
                bit_depth: Some(24),
            },
            content: ContentFingerprint::from_bytes(name.as_bytes()),
            provenance: AssetProvenance::new(
                imported,
                AssetOrigin::ImportedFile {
                    importer: "test".into(),
                },
                location,
            ),
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            favorite: false,
        }
    }

    fn registry() -> AssetRegistry {
        let mut registry = AssetRegistry::new();
        let kick = registry
            .register(registration("Glass Kick", 24_000, 10, &["kick", "glass"]))
            .unwrap();
        let hat = registry
            .register(registration("Acid Hat", 12_000, 30, &["hat", "acid"]))
            .unwrap();
        let pad = registry
            .register(registration("Cold Pad", 480_000, 20, &["pad", "cold"]))
            .unwrap();
        registry.set_favorite(kick, true).unwrap();
        registry.mark_missing(pad, 99).unwrap();
        registry
            .add_usage(
                hat,
                AssetUsageOwner::Step { persistent_id: 7 },
                None,
                "Hat lane",
            )
            .unwrap();
        registry
            .add_usage(
                hat,
                AssetUsageOwner::Step { persistent_id: 8 },
                None,
                "Hat fill",
            )
            .unwrap();
        registry
    }

    #[test]
    fn search_tag_favorite_and_missing_filters_compose() {
        let registry = registry();
        let mut state = AssetBrowserState {
            search: "glass".into(),
            favorites_only: true,
            ..Default::default()
        };
        assert_eq!(state.filtered_ids(&registry).len(), 1);
        state.search.clear();
        state.favorites_only = false;
        state.tag = Some("cold".into());
        state.availability = BrowserAvailability::Missing;
        let ids = state.filtered_ids(&registry);
        assert_eq!(ids.len(), 1);
        assert_eq!(registry.get(ids[0]).unwrap().name(), "Cold Pad");
    }

    #[test]
    fn usage_and_import_sorts_are_deterministic() {
        let registry = registry();
        let mut state = AssetBrowserState {
            sort: BrowserSort::UsageDescending,
            ..Default::default()
        };
        let ids = state.filtered_ids(&registry);
        assert_eq!(registry.get(ids[0]).unwrap().name(), "Acid Hat");
        state.sort = BrowserSort::ImportedNewest;
        let ids = state.filtered_ids(&registry);
        assert_eq!(registry.get(ids[0]).unwrap().name(), "Acid Hat");
    }

    #[test]
    fn selection_survives_sort_and_reconciles_after_filtering() {
        let registry = registry();
        let mut state = AssetBrowserState::default();
        let all = state.filtered_ids(&registry);
        state.selected = Some(all[1]);
        state.sort = BrowserSort::DurationDescending;
        let resorted = state.filtered_ids(&registry);
        state.reconcile_selection(&resorted);
        assert_eq!(state.selected, Some(all[1]));
        state.search = "cold".into();
        let filtered = state.filtered_ids(&registry);
        state.reconcile_selection(&filtered);
        assert_eq!(state.selected, filtered.first().copied());
    }

    #[test]
    fn keyboard_movement_clamps_at_edges() {
        let ids = vec![AssetId(1), AssetId(2), AssetId(3)];
        let mut state = AssetBrowserState::default();
        state.move_selection(&ids, 1);
        assert_eq!(state.selected, Some(AssetId(2)));
        state.move_selection(&ids, 99);
        assert_eq!(state.selected, Some(AssetId(3)));
        state.move_selection(&ids, -99);
        assert_eq!(state.selected, Some(AssetId(1)));
        state.move_selection(&[], 1);
        assert_eq!(state.selected, None);
    }

    #[test]
    fn duration_and_provenance_labels_are_compact() {
        assert_eq!(duration_label(24_000, 48_000), "0.50s");
        assert_eq!(duration_label(3_120_000, 48_000), "1:05.0");
        assert_eq!(
            origin_label(&AssetOrigin::Rendered {
                renderer: "Freeze".into(),
                source_revision: 42,
            }),
            "Rendered · Freeze · revision 42"
        );
    }

    #[test]
    fn instrument_sample_drag_retains_the_exact_virtual_slice() {
        let range =
            crate::assets::AssetFrameRange::new(SampleFrames(120), SampleFrames(480)).unwrap();
        let drag = material_drag(SourceMaterialRef::VirtualSlice(
            VirtualSliceRef::new(AssetId(7), range).unwrap(),
        ));
        assert_eq!(drag.asset, AssetId(7));
        assert_eq!(drag.source_range, Some(range));
    }
}
