//! Product-level input, focus, and accessibility semantics.
//!
//! Audec's views are custom-painted GPUI trees. The pinned GPUI 0.2.2 API can
//! make those trees keyboard-focusable, but does not expose semantic
//! accessibility roles, names, values, states, or actions to the platform.
//! Consequently, adding more mouse handlers cannot repair the native AX tree:
//! the operating system currently discovers the native window chrome and not
//! Audec's content. This module is the intentionally GPUI-neutral half of the
//! repair. It gives every product surface a typed focus address, routes pointer,
//! keyboard, and assistive activation through one action vocabulary, models
//! composite/roving focus and modal close guards, and produces a semantic tree
//! suitable for a future GPUI/AccessKit or platform bridge.
//!
//! This module refuses to mutate project state, guess project identities from
//! presentation indices, or pretend its semantic snapshot is already visible
//! to a screen reader. A host must lower [`ProductAction`] through the same
//! controller/command path used by pointer gestures.

use crate::arrangement::{ClipId, TrackId};
use crate::explorer_model::{ExplorerMode, InspectorSectionKind};
use crate::project_controller::{ObjectRef, PadRef};
use crate::sequencer::{PatternId, StepLaneId};
use crate::timeline::TimelineControllerId;
use crate::workspace_document::WorkspaceViewId;

/// What the pinned UI toolkit can truthfully provide today.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessibilityCapability {
    /// Tab stops, focus changes, and key actions work, but the custom-painted
    /// content is absent from the native accessibility tree.
    KeyboardFocusOnly,
    /// A future host adapter publishes [`AccessibilitySnapshot`] to a native
    /// semantic tree and invokes its actions through [`ProductAction`].
    NativeSemanticTree,
}

pub const PINNED_GPUI_ACCESSIBILITY: AccessibilityCapability =
    AccessibilityCapability::KeyboardFocusOnly;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CloseRequestId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CloseScope {
    View(WorkspaceViewId),
    Window,
    Application,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CloseChoice {
    Save,
    Discard,
    Cancel,
}

/// Stable semantic identities for scroll ownership. These are deliberately
/// pane-relative, not coordinates or indices into a render-time child list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScrollRegion {
    ExplorerTree,
    InspectorReport,
    Timeline,
    ArrangementTracks,
    SamplerPads,
    PatternGrid,
    DialogBody,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScrollContainerRef {
    pub view: WorkspaceViewId,
    pub region: ScrollRegion,
}

/// A logical focus address. GPUI should normally expose one native tab stop
/// per composite surface, while this address is its roving active descendant.
/// No variant uses a display row, grid index, or object name as identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FocusTarget {
    ExplorerSurface(WorkspaceViewId),
    ExplorerSearch(WorkspaceViewId),
    ExplorerMode {
        view: WorkspaceViewId,
        mode: ExplorerMode,
    },
    ExplorerObject {
        view: WorkspaceViewId,
        object: ObjectRef,
    },
    InspectorSurface {
        view: WorkspaceViewId,
        object: ObjectRef,
    },
    InspectorSection {
        view: WorkspaceViewId,
        object: ObjectRef,
        section: InspectorSectionKind,
    },
    InspectorReveal {
        view: WorkspaceViewId,
        owner: ObjectRef,
        object: ObjectRef,
    },
    Timeline {
        view: WorkspaceViewId,
        controller: TimelineControllerId,
    },
    ArrangementSurface(WorkspaceViewId),
    ArrangementTrack {
        view: WorkspaceViewId,
        track: TrackId,
    },
    ArrangementClip {
        view: WorkspaceViewId,
        clip: ClipId,
    },
    SamplerSurface(WorkspaceViewId),
    SamplerPad {
        view: WorkspaceViewId,
        pad: PadRef,
    },
    PatternSurface {
        view: WorkspaceViewId,
        pattern: PatternId,
    },
    PatternCell {
        view: WorkspaceViewId,
        pattern: PatternId,
        lane: StepLaneId,
        step: u32,
    },
    ClosePrompt {
        request: CloseRequestId,
        choice: CloseChoice,
    },
    ScrollContainer(ScrollContainerRef),
}

impl FocusTarget {
    pub fn view(&self) -> Option<WorkspaceViewId> {
        match self {
            Self::ExplorerSurface(view)
            | Self::ExplorerSearch(view)
            | Self::ArrangementSurface(view)
            | Self::SamplerSurface(view) => Some(*view),
            Self::ExplorerMode { view, .. }
            | Self::ExplorerObject { view, .. }
            | Self::InspectorSurface { view, .. }
            | Self::InspectorSection { view, .. }
            | Self::InspectorReveal { view, .. }
            | Self::Timeline { view, .. }
            | Self::ArrangementTrack { view, .. }
            | Self::ArrangementClip { view, .. }
            | Self::SamplerPad { view, .. }
            | Self::PatternSurface { view, .. }
            | Self::PatternCell { view, .. } => Some(*view),
            Self::ScrollContainer(container) => Some(container.view),
            Self::ClosePrompt { .. } => None,
        }
    }

    pub const fn is_text_entry(&self) -> bool {
        matches!(self, Self::ExplorerSearch(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticRole {
    Application,
    Group,
    SearchField,
    Tab,
    Tree,
    TreeItem,
    Region,
    Timeline,
    Track,
    Clip,
    Grid,
    GridCell,
    Button,
    Dialog,
    ScrollArea,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SemanticState {
    pub disabled: bool,
    pub selected: bool,
    pub expanded: Option<bool>,
    pub checked: Option<bool>,
    pub busy: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessibleAction {
    Focus,
    Activate,
    ShowInspector,
    Expand,
    Collapse,
    Increment,
    Decrement,
    ScrollForward,
    ScrollBackward,
}

/// Deterministic semantic content independent of GPUI's paint tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticNode {
    pub target: FocusTarget,
    pub role: SemanticRole,
    pub name: String,
    pub description: Option<String>,
    pub value: Option<String>,
    pub state: SemanticState,
    /// Only composite surface roots and ordinary controls should normally be
    /// tab stops. Tree items, pads, and grid cells use roving focus.
    pub tab_stop: bool,
    pub actions: Vec<AccessibleAction>,
    pub neighbors: FocusNeighbors,
    pub default_action: Option<ProductAction>,
    pub press_action: Option<ProductAction>,
    pub release_action: Option<ProductAction>,
    pub children: Vec<SemanticNode>,
}

impl SemanticNode {
    pub fn leaf(target: FocusTarget, role: SemanticRole, name: impl Into<String>) -> Self {
        Self {
            target,
            role,
            name: name.into(),
            description: None,
            value: None,
            state: SemanticState::default(),
            tab_stop: false,
            actions: vec![AccessibleAction::Focus],
            neighbors: FocusNeighbors::default(),
            default_action: None,
            press_action: None,
            release_action: None,
            children: Vec::new(),
        }
    }

    pub fn with_default_action(mut self, action: ProductAction) -> Self {
        self.actions.push(AccessibleAction::Activate);
        self.default_action = Some(action);
        self
    }

    pub fn with_gate(mut self, press: ProductAction, release: ProductAction) -> Self {
        self.actions.push(AccessibleAction::Activate);
        self.press_action = Some(press);
        self.release_action = Some(release);
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FocusNeighbors {
    pub previous: Option<FocusTarget>,
    pub next: Option<FocusTarget>,
    pub left: Option<FocusTarget>,
    pub right: Option<FocusTarget>,
    pub up: Option<FocusTarget>,
    pub down: Option<FocusTarget>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusDirection {
    Previous,
    Next,
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollAmount {
    Line(i8),
    Page(i8),
    Start,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimelineInputAction {
    PlayPause,
    NudgeCursor {
        direction: i8,
        extend_selection: bool,
    },
    SetLoopFromSelection,
    ToggleLoop,
    ClearSelection,
    ZoomIn,
    ZoomOut,
    Fit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrangementInputAction {
    DeleteClip(ClipId),
    DuplicateClip(ClipId),
    SplitClipAtCursor(ClipId),
    SelectTrack(TrackId),
    SelectClip(ClipId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SamplerInputAction {
    GateOn(PadRef),
    GateOff(PadRef),
    ShowPad(PadRef),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatternInputAction {
    ToggleStep {
        pattern: PatternId,
        lane: StepLaneId,
        step: u32,
    },
    ClearStep {
        pattern: PatternId,
        lane: StepLaneId,
        step: u32,
    },
    ShowPattern(PatternId),
}

/// The single vocabulary consumed by keyboard, pointer, and future native AX
/// adapters. These are semantic requests, not project mutations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProductAction {
    Reveal(ObjectRef),
    ShowInspector(ObjectRef),
    SetExplorerMode(ExplorerMode),
    Timeline {
        controller: TimelineControllerId,
        action: TimelineInputAction,
    },
    Arrangement(ArrangementInputAction),
    Sampler(SamplerInputAction),
    Pattern(PatternInputAction),
    Scroll {
        container: ScrollContainerRef,
        amount: ScrollAmount,
    },
    CloseChoice {
        request: CloseRequestId,
        choice: CloseChoice,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub command: bool,
    pub control: bool,
    pub option: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Tab,
    Enter,
    Escape,
    Space,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    Backspace,
    Character(char),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyPhase {
    Press,
    Release,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyStroke {
    pub key: Key,
    pub modifiers: Modifiers,
    pub phase: KeyPhase,
    pub repeated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PointerId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Primary,
    Secondary,
    Middle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerPhase {
    Down,
    Up,
    Cancel,
}

/// Product-level pointer input. Continuous arrangement/timeline coordinates
/// remain owned by their specialist gesture controllers; this contract owns
/// only stable target focus, capture, activation, and gate release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointerInput {
    pub id: PointerId,
    pub phase: PointerPhase,
    pub button: PointerButton,
    /// `None` on cancellation or release outside the original control.
    pub target: Option<FocusTarget>,
}

impl KeyStroke {
    pub const fn press(key: Key) -> Self {
        Self {
            key,
            modifiers: Modifiers {
                shift: false,
                command: false,
                control: false,
                option: false,
            },
            phase: KeyPhase::Press,
            repeated: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEffect {
    Focus(FocusTarget),
    EnsureVisible(FocusTarget),
    Dispatch(ProductAction),
    Announce(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SemanticTreeDiagnostic {
    EmptyName(FocusTarget),
    DuplicateTarget(FocusTarget),
    MissingDefaultAction(FocusTarget),
    MissingGatePair(FocusTarget),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccessibilitySnapshot {
    pub roots: Vec<SemanticNode>,
}

impl AccessibilitySnapshot {
    pub fn validate(&self) -> Vec<SemanticTreeDiagnostic> {
        let mut diagnostics = Vec::new();
        let mut seen: Vec<&FocusTarget> = Vec::new();
        for root in &self.roots {
            validate_node(root, &mut seen, &mut diagnostics);
        }
        diagnostics
    }

    fn find(&self, target: &FocusTarget) -> Option<&SemanticNode> {
        self.roots.iter().find_map(|node| find_node(node, target))
    }

    fn flattened(&self) -> Vec<&SemanticNode> {
        let mut nodes = Vec::new();
        for root in &self.roots {
            flatten_node(root, &mut nodes);
        }
        nodes
    }
}

fn validate_node<'a>(
    node: &'a SemanticNode,
    seen: &mut Vec<&'a FocusTarget>,
    diagnostics: &mut Vec<SemanticTreeDiagnostic>,
) {
    if node.name.trim().is_empty() {
        diagnostics.push(SemanticTreeDiagnostic::EmptyName(node.target.clone()));
    }
    if seen.iter().any(|target| *target == &node.target) {
        diagnostics.push(SemanticTreeDiagnostic::DuplicateTarget(node.target.clone()));
    } else {
        seen.push(&node.target);
    }
    if node.actions.contains(&AccessibleAction::Activate)
        && node.default_action.is_none()
        && node.press_action.is_none()
    {
        diagnostics.push(SemanticTreeDiagnostic::MissingDefaultAction(
            node.target.clone(),
        ));
    }
    if node.press_action.is_some() != node.release_action.is_some() {
        diagnostics.push(SemanticTreeDiagnostic::MissingGatePair(node.target.clone()));
    }
    for child in &node.children {
        validate_node(child, seen, diagnostics);
    }
}

fn find_node<'a>(node: &'a SemanticNode, target: &FocusTarget) -> Option<&'a SemanticNode> {
    if &node.target == target {
        return Some(node);
    }
    node.children
        .iter()
        .find_map(|child| find_node(child, target))
}

fn flatten_node<'a>(node: &'a SemanticNode, output: &mut Vec<&'a SemanticNode>) {
    output.push(node);
    for child in &node.children {
        flatten_node(child, output);
    }
}

/// Logical focus and action state. GPUI owns the native [`gpui::FocusHandle`]
/// corresponding to a composite root; this controller owns its typed active
/// descendant and ordered effects.
#[derive(Clone, Debug)]
pub struct ProductInputController {
    snapshot: AccessibilitySnapshot,
    focused: Option<FocusTarget>,
    modal: Option<CloseRequestId>,
    restore_after_modal: Option<FocusTarget>,
    pressed_gates: Vec<(FocusTarget, Key, ProductAction)>,
    pointer_capture: Option<PointerCapture>,
}

#[derive(Clone, Debug)]
struct PointerCapture {
    id: PointerId,
    target: FocusTarget,
    release_action: Option<ProductAction>,
}

impl ProductInputController {
    pub fn new(snapshot: AccessibilitySnapshot) -> Self {
        let focused = snapshot
            .flattened()
            .into_iter()
            .find(|node| node.tab_stop && !node.state.disabled)
            .map(|node| node.target.clone());
        Self {
            snapshot,
            focused,
            modal: None,
            restore_after_modal: None,
            pressed_gates: Vec::new(),
            pointer_capture: None,
        }
    }

    pub fn snapshot(&self) -> &AccessibilitySnapshot {
        &self.snapshot
    }

    pub fn focused(&self) -> Option<&FocusTarget> {
        self.focused.as_ref()
    }

    /// Publication refreshes preserve a still-valid logical target. This is
    /// essential during an arrangement drag: a new immutable project snapshot
    /// must not reset focus or steal pointer ownership from the view controller.
    pub fn replace_snapshot(&mut self, snapshot: AccessibilitySnapshot) -> Vec<InputEffect> {
        self.snapshot = snapshot;
        let mut effects = Vec::new();
        let mut index = 0;
        while index < self.pressed_gates.len() {
            if self.snapshot.find(&self.pressed_gates[index].0).is_none() {
                let (_, _, release) = self.pressed_gates.remove(index);
                effects.push(InputEffect::Dispatch(release));
            } else {
                index += 1;
            }
        }
        if self
            .pointer_capture
            .as_ref()
            .is_some_and(|capture| self.snapshot.find(&capture.target).is_none())
        {
            if let Some(release) = self
                .pointer_capture
                .take()
                .and_then(|capture| capture.release_action)
            {
                effects.push(InputEffect::Dispatch(release));
            }
        }
        if self
            .focused
            .as_ref()
            .is_some_and(|target| self.allowed(target) && self.snapshot.find(target).is_some())
        {
            return effects;
        }
        let replacement = self.first_focusable();
        self.focused = replacement.clone();
        effects.extend(replacement.into_iter().flat_map(|target| {
            [
                InputEffect::Focus(target.clone()),
                InputEffect::EnsureVisible(target),
            ]
        }));
        effects
    }

    pub fn focus(&mut self, target: FocusTarget) -> Vec<InputEffect> {
        let Some(node) = self.snapshot.find(&target) else {
            return vec![InputEffect::Announce(
                "That control is no longer available".into(),
            )];
        };
        if node.state.disabled || !self.allowed(&target) {
            return vec![InputEffect::Announce(format!(
                "{} is unavailable",
                node.name
            ))];
        }
        self.focused = Some(target.clone());
        vec![
            InputEffect::Focus(target.clone()),
            InputEffect::EnsureVisible(target),
        ]
    }

    pub fn tab(&mut self, reverse: bool) -> Vec<InputEffect> {
        let candidates: Vec<FocusTarget> = self
            .snapshot
            .flattened()
            .into_iter()
            .filter(|node| node.tab_stop && !node.state.disabled && self.allowed(&node.target))
            .map(|node| node.target.clone())
            .collect();
        if candidates.is_empty() {
            return Vec::new();
        }
        let current = self
            .focused
            .as_ref()
            .and_then(|target| candidates.iter().position(|item| item == target));
        let next = match (current, reverse) {
            (Some(0), true) | (None, true) => candidates.len() - 1,
            (Some(index), true) => index - 1,
            (Some(index), false) => (index + 1) % candidates.len(),
            (None, false) => 0,
        };
        self.focus(candidates[next].clone())
    }

    pub fn move_roving(&mut self, direction: FocusDirection) -> Vec<InputEffect> {
        let Some(current) = self.focused.clone() else {
            return Vec::new();
        };
        let Some(node) = self.snapshot.find(&current) else {
            return Vec::new();
        };
        let next = match direction {
            FocusDirection::Previous => &node.neighbors.previous,
            FocusDirection::Next => &node.neighbors.next,
            FocusDirection::Left => &node.neighbors.left,
            FocusDirection::Right => &node.neighbors.right,
            FocusDirection::Up => &node.neighbors.up,
            FocusDirection::Down => &node.neighbors.down,
        };
        next.clone()
            .map_or_else(Vec::new, |target| self.focus(target))
    }

    /// Pointer and native accessibility activation deliberately share this
    /// implementation with Enter/Space keyboard activation.
    pub fn activate(&mut self, target: FocusTarget) -> Vec<InputEffect> {
        let mut effects = self.focus(target.clone());
        if !matches!(effects.first(), Some(InputEffect::Focus(_))) {
            return effects;
        }
        if let Some(action) = self
            .snapshot
            .find(&target)
            .and_then(|node| node.default_action.clone())
        {
            effects.push(InputEffect::Dispatch(action));
        }
        effects
    }

    pub fn enter_modal(
        &mut self,
        request: CloseRequestId,
        preferred: FocusTarget,
    ) -> Vec<InputEffect> {
        self.restore_after_modal = self.focused.clone();
        self.modal = Some(request);
        self.focus(preferred)
    }

    pub fn leave_modal(&mut self) -> Vec<InputEffect> {
        self.modal = None;
        let restore = self.restore_after_modal.take();
        restore.map_or_else(Vec::new, |target| self.focus(target))
    }

    pub fn handle_key(&mut self, stroke: KeyStroke) -> Vec<InputEffect> {
        let Some(target) = self.focused.clone() else {
            return Vec::new();
        };

        if stroke.key == Key::Tab && stroke.phase == KeyPhase::Press {
            return self.tab(stroke.modifiers.shift);
        }

        if stroke.phase == KeyPhase::Release {
            if let Some(index) = self
                .pressed_gates
                .iter()
                .position(|(_, key, _)| *key == stroke.key)
            {
                let (_, _, action) = self.pressed_gates.remove(index);
                return vec![InputEffect::Dispatch(action)];
            }
            return Vec::new();
        }

        if stroke.key == Key::Escape {
            if let Some(request) = self.modal {
                return vec![InputEffect::Dispatch(ProductAction::CloseChoice {
                    request,
                    choice: CloseChoice::Cancel,
                })];
            }
            return self.default_or(Vec::new(), ProductActionFallback::ClearSelection);
        }

        if target.is_text_entry() {
            return Vec::new();
        }

        let direction = match stroke.key {
            Key::Left => Some(FocusDirection::Left),
            Key::Right => Some(FocusDirection::Right),
            Key::Up => Some(FocusDirection::Up),
            Key::Down => Some(FocusDirection::Down),
            _ => None,
        };
        if let Some(direction) = direction {
            let moved = self.move_roving(direction);
            if !moved.is_empty() {
                return moved;
            }
        }

        if matches!(stroke.key, Key::Enter | Key::Space) {
            let node = self.snapshot.find(&target);
            if let Some(release) = node.and_then(|node| node.release_action.clone()) {
                if !stroke.repeated
                    && !self
                        .pressed_gates
                        .iter()
                        .any(|(owner, key, _)| owner == &target && *key == stroke.key)
                {
                    self.pressed_gates
                        .push((target.clone(), stroke.key, release));
                    return node
                        .and_then(|node| node.press_action.clone())
                        .map(|action| vec![InputEffect::Dispatch(action)])
                        .unwrap_or_default();
                }
                return Vec::new();
            }
            if stroke.key == Key::Space {
                if let Some(action) = self.key_action(&target, stroke) {
                    return vec![InputEffect::Dispatch(action)];
                }
            }
            return self.activate(target);
        }

        self.key_action(&target, stroke)
            .map(|action| vec![InputEffect::Dispatch(action)])
            .unwrap_or_default()
    }

    pub fn handle_pointer(&mut self, input: PointerInput) -> Vec<InputEffect> {
        if input.button != PointerButton::Primary && input.phase != PointerPhase::Cancel {
            return Vec::new();
        }
        match input.phase {
            PointerPhase::Down => {
                let Some(target) = input.target else {
                    return Vec::new();
                };
                let mut effects = self
                    .pointer_capture
                    .take()
                    .and_then(|capture| capture.release_action)
                    .map(|action| vec![InputEffect::Dispatch(action)])
                    .unwrap_or_default();
                let focus_effects = self.focus(target.clone());
                if !matches!(focus_effects.first(), Some(InputEffect::Focus(_))) {
                    effects.extend(focus_effects);
                    return effects;
                }
                effects.extend(focus_effects);
                let (press_action, release_action) = self
                    .snapshot
                    .find(&target)
                    .map(|node| (node.press_action.clone(), node.release_action.clone()))
                    .unwrap_or_default();
                self.pointer_capture = Some(PointerCapture {
                    id: input.id,
                    target,
                    release_action,
                });
                if let Some(action) = press_action {
                    effects.push(InputEffect::Dispatch(action));
                }
                effects
            }
            PointerPhase::Up => {
                let Some(capture) = self.pointer_capture.take() else {
                    return Vec::new();
                };
                if capture.id != input.id {
                    self.pointer_capture = Some(capture);
                    return Vec::new();
                }
                if let Some(action) = capture.release_action {
                    return vec![InputEffect::Dispatch(action)];
                }
                if input.target.as_ref() == Some(&capture.target) {
                    self.activate(capture.target)
                } else {
                    Vec::new()
                }
            }
            PointerPhase::Cancel => {
                let Some(capture) = self.pointer_capture.take() else {
                    return Vec::new();
                };
                if capture.id != input.id {
                    self.pointer_capture = Some(capture);
                    return Vec::new();
                }
                capture
                    .release_action
                    .map(|action| vec![InputEffect::Dispatch(action)])
                    .unwrap_or_default()
            }
        }
    }

    fn key_action(&self, target: &FocusTarget, stroke: KeyStroke) -> Option<ProductAction> {
        match target {
            FocusTarget::Timeline { controller, .. } => match stroke.key {
                Key::Space => Some(ProductAction::Timeline {
                    controller: *controller,
                    action: TimelineInputAction::PlayPause,
                }),
                Key::Left | Key::Right => Some(ProductAction::Timeline {
                    controller: *controller,
                    action: TimelineInputAction::NudgeCursor {
                        direction: if stroke.key == Key::Left { -1 } else { 1 },
                        extend_selection: stroke.modifiers.shift,
                    },
                }),
                Key::Character('l') | Key::Character('L') if stroke.modifiers.shift => {
                    Some(ProductAction::Timeline {
                        controller: *controller,
                        action: TimelineInputAction::SetLoopFromSelection,
                    })
                }
                Key::Character('l') | Key::Character('L') => Some(ProductAction::Timeline {
                    controller: *controller,
                    action: TimelineInputAction::ToggleLoop,
                }),
                Key::Character('0') if stroke.modifiers.command => Some(ProductAction::Timeline {
                    controller: *controller,
                    action: TimelineInputAction::Fit,
                }),
                _ => None,
            },
            FocusTarget::ArrangementClip { clip, .. } => match stroke.key {
                Key::Delete | Key::Backspace => Some(ProductAction::Arrangement(
                    ArrangementInputAction::DeleteClip(*clip),
                )),
                Key::Character('d') | Key::Character('D') if stroke.modifiers.command => Some(
                    ProductAction::Arrangement(ArrangementInputAction::DuplicateClip(*clip)),
                ),
                Key::Character('s') | Key::Character('S') if stroke.modifiers.command => Some(
                    ProductAction::Arrangement(ArrangementInputAction::SplitClipAtCursor(*clip)),
                ),
                _ => None,
            },
            FocusTarget::PatternCell {
                pattern,
                lane,
                step,
                ..
            } if matches!(stroke.key, Key::Delete | Key::Backspace) => {
                Some(ProductAction::Pattern(PatternInputAction::ClearStep {
                    pattern: *pattern,
                    lane: *lane,
                    step: *step,
                }))
            }
            FocusTarget::ScrollContainer(container) => match stroke.key {
                Key::PageUp => Some(ProductAction::Scroll {
                    container: *container,
                    amount: ScrollAmount::Page(-1),
                }),
                Key::PageDown => Some(ProductAction::Scroll {
                    container: *container,
                    amount: ScrollAmount::Page(1),
                }),
                Key::Home => Some(ProductAction::Scroll {
                    container: *container,
                    amount: ScrollAmount::Start,
                }),
                Key::End => Some(ProductAction::Scroll {
                    container: *container,
                    amount: ScrollAmount::End,
                }),
                _ => None,
            },
            _ => None,
        }
    }

    fn default_or(
        &self,
        fallback: Vec<InputEffect>,
        action: ProductActionFallback,
    ) -> Vec<InputEffect> {
        let Some(FocusTarget::Timeline { controller, .. }) = self.focused.as_ref() else {
            return fallback;
        };
        match action {
            ProductActionFallback::ClearSelection => {
                vec![InputEffect::Dispatch(ProductAction::Timeline {
                    controller: *controller,
                    action: TimelineInputAction::ClearSelection,
                })]
            }
        }
    }

    fn first_focusable(&self) -> Option<FocusTarget> {
        self.snapshot
            .flattened()
            .into_iter()
            .find(|node| node.tab_stop && !node.state.disabled && self.allowed(&node.target))
            .map(|node| node.target.clone())
    }

    fn allowed(&self, target: &FocusTarget) -> bool {
        self.modal.map_or(true, |request| {
            matches!(target, FocusTarget::ClosePrompt { request: target_request, .. } if *target_request == request)
        })
    }
}

#[derive(Clone, Copy)]
enum ProductActionFallback {
    ClearSelection,
}

/// Dirty-state close/quit state machine. Async save completion is explicit,
/// so a failed save can never accidentally fall through to quit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseGuardState {
    Idle,
    Prompting {
        request: CloseRequestId,
        scope: CloseScope,
    },
    Saving {
        request: CloseRequestId,
        scope: CloseScope,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseGuardEffect {
    OpenPrompt {
        request: CloseRequestId,
        scope: CloseScope,
        default: CloseChoice,
    },
    SaveProject {
        request: CloseRequestId,
    },
    CloseNow(CloseScope),
    KeepOpen,
}

#[derive(Clone, Debug)]
pub struct CloseGuard {
    state: CloseGuardState,
    next_request: u64,
}

impl Default for CloseGuard {
    fn default() -> Self {
        Self {
            state: CloseGuardState::Idle,
            next_request: 1,
        }
    }
}

impl CloseGuard {
    pub const fn state(&self) -> CloseGuardState {
        self.state
    }

    pub fn request(&mut self, scope: CloseScope, dirty: bool) -> CloseGuardEffect {
        if self.state != CloseGuardState::Idle {
            return CloseGuardEffect::KeepOpen;
        }
        if !dirty {
            return CloseGuardEffect::CloseNow(scope);
        }
        let request = CloseRequestId(self.next_request);
        self.next_request = self.next_request.saturating_add(1);
        self.state = CloseGuardState::Prompting { request, scope };
        CloseGuardEffect::OpenPrompt {
            request,
            scope,
            default: CloseChoice::Save,
        }
    }

    pub fn choose(&mut self, request: CloseRequestId, choice: CloseChoice) -> CloseGuardEffect {
        let CloseGuardState::Prompting {
            request: active,
            scope,
        } = self.state
        else {
            return CloseGuardEffect::KeepOpen;
        };
        if active != request {
            return CloseGuardEffect::KeepOpen;
        }
        match choice {
            CloseChoice::Save => {
                self.state = CloseGuardState::Saving { request, scope };
                CloseGuardEffect::SaveProject { request }
            }
            CloseChoice::Discard => {
                self.state = CloseGuardState::Idle;
                CloseGuardEffect::CloseNow(scope)
            }
            CloseChoice::Cancel => {
                self.state = CloseGuardState::Idle;
                CloseGuardEffect::KeepOpen
            }
        }
    }

    pub fn save_finished(&mut self, request: CloseRequestId, succeeded: bool) -> CloseGuardEffect {
        let CloseGuardState::Saving {
            request: active,
            scope,
        } = self.state
        else {
            return CloseGuardEffect::KeepOpen;
        };
        if active != request {
            return CloseGuardEffect::KeepOpen;
        }
        self.state = CloseGuardState::Idle;
        if succeeded {
            CloseGuardEffect::CloseNow(scope)
        } else {
            CloseGuardEffect::KeepOpen
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::AssetId;
    use crate::sample_kit::{KitId, PadId};

    fn view(raw: u64) -> WorkspaceViewId {
        WorkspaceViewId(raw)
    }

    fn asset(raw: u64) -> ObjectRef {
        ObjectRef::Material(AssetId(raw))
    }

    fn pad(raw: u64) -> PadRef {
        PadRef {
            kit: KitId::from_raw(4),
            pad: PadId::from_raw(raw),
            zone: None,
        }
    }

    fn key(key: Key) -> KeyStroke {
        KeyStroke::press(key)
    }

    #[test]
    fn pointer_keyboard_and_assistive_activation_share_one_action() {
        let target = FocusTarget::ExplorerObject {
            view: view(7),
            object: asset(19),
        };
        let node = SemanticNode::leaf(target.clone(), SemanticRole::TreeItem, "Break.wav")
            .with_default_action(ProductAction::Reveal(asset(19)));
        let mut controller =
            ProductInputController::new(AccessibilitySnapshot { roots: vec![node] });

        controller.handle_pointer(PointerInput {
            id: PointerId(1),
            phase: PointerPhase::Down,
            button: PointerButton::Primary,
            target: Some(target.clone()),
        });
        let pointer = controller.handle_pointer(PointerInput {
            id: PointerId(1),
            phase: PointerPhase::Up,
            button: PointerButton::Primary,
            target: Some(target.clone()),
        });
        controller.focused = Some(target);
        let keyboard = controller.handle_key(key(Key::Enter));
        assert_eq!(pointer.last(), keyboard.last());
        assert_eq!(
            keyboard.last(),
            Some(&InputEffect::Dispatch(ProductAction::Reveal(asset(19))))
        );
    }

    #[test]
    fn explorer_uses_roving_focus_and_scroll_reveal() {
        let first = FocusTarget::ExplorerObject {
            view: view(7),
            object: asset(1),
        };
        let second = FocusTarget::ExplorerObject {
            view: view(7),
            object: asset(2),
        };
        let mut first_node = SemanticNode::leaf(first.clone(), SemanticRole::TreeItem, "One");
        first_node.tab_stop = true;
        first_node.neighbors.down = Some(second.clone());
        let mut second_node = SemanticNode::leaf(second.clone(), SemanticRole::TreeItem, "Two");
        second_node.neighbors.up = Some(first);
        let mut controller = ProductInputController::new(AccessibilitySnapshot {
            roots: vec![first_node, second_node],
        });

        let effects = controller.handle_key(key(Key::Down));
        assert_eq!(controller.focused(), Some(&second));
        assert_eq!(effects.last(), Some(&InputEffect::EnsureVisible(second)));
    }

    #[test]
    fn inspector_reveal_is_typed_and_not_a_row_index() {
        let owner = asset(3);
        let destination = asset(8);
        let target = FocusTarget::InspectorReveal {
            view: view(8),
            owner,
            object: destination.clone(),
        };
        let node = SemanticNode::leaf(target.clone(), SemanticRole::Button, "Reveal source")
            .with_default_action(ProductAction::Reveal(destination.clone()));
        let mut controller =
            ProductInputController::new(AccessibilitySnapshot { roots: vec![node] });
        assert_eq!(
            controller.activate(target).last(),
            Some(&InputEffect::Dispatch(ProductAction::Reveal(destination)))
        );
    }

    #[test]
    fn timeline_loop_and_selection_shortcuts_remain_distinct() {
        let target = FocusTarget::Timeline {
            view: view(9),
            controller: TimelineControllerId(21),
        };
        let mut node = SemanticNode::leaf(target, SemanticRole::Timeline, "Arrangement timeline");
        node.tab_stop = true;
        let mut controller =
            ProductInputController::new(AccessibilitySnapshot { roots: vec![node] });
        let mut set_loop = key(Key::Character('l'));
        set_loop.modifiers.shift = true;
        assert_eq!(
            controller.handle_key(set_loop),
            vec![InputEffect::Dispatch(ProductAction::Timeline {
                controller: TimelineControllerId(21),
                action: TimelineInputAction::SetLoopFromSelection,
            })]
        );
        assert_eq!(
            controller.handle_key(key(Key::Escape)),
            vec![InputEffect::Dispatch(ProductAction::Timeline {
                controller: TimelineControllerId(21),
                action: TimelineInputAction::ClearSelection,
            })]
        );
    }

    #[test]
    fn arrangement_keys_are_scoped_to_a_typed_clip() {
        let clip = ClipId::from_raw(41);
        let target = FocusTarget::ArrangementClip {
            view: view(10),
            clip,
        };
        let mut node = SemanticNode::leaf(target, SemanticRole::Clip, "Vocal clip");
        node.tab_stop = true;
        let mut controller =
            ProductInputController::new(AccessibilitySnapshot { roots: vec![node] });
        assert_eq!(
            controller.handle_key(key(Key::Delete)),
            vec![InputEffect::Dispatch(ProductAction::Arrangement(
                ArrangementInputAction::DeleteClip(clip)
            ))]
        );
    }

    #[test]
    fn sampler_gate_pairs_press_release_and_ignores_key_repeat() {
        let pad = pad(2);
        let target = FocusTarget::SamplerPad {
            view: view(11),
            pad,
        };
        let mut node = SemanticNode::leaf(target, SemanticRole::Button, "Pad 2").with_gate(
            ProductAction::Sampler(SamplerInputAction::GateOn(pad)),
            ProductAction::Sampler(SamplerInputAction::GateOff(pad)),
        );
        node.tab_stop = true;
        let mut controller =
            ProductInputController::new(AccessibilitySnapshot { roots: vec![node] });
        assert_eq!(
            controller.handle_key(key(Key::Space)),
            vec![InputEffect::Dispatch(ProductAction::Sampler(
                SamplerInputAction::GateOn(pad)
            ))]
        );
        let mut repeat = key(Key::Space);
        repeat.repeated = true;
        assert!(controller.handle_key(repeat).is_empty());
        let mut release = key(Key::Space);
        release.phase = KeyPhase::Release;
        assert_eq!(
            controller.handle_key(release),
            vec![InputEffect::Dispatch(ProductAction::Sampler(
                SamplerInputAction::GateOff(pad)
            ))]
        );
    }

    #[test]
    fn pointer_cancel_releases_sampler_gate_without_activating_another_pad() {
        let pad = pad(3);
        let target = FocusTarget::SamplerPad {
            view: view(11),
            pad,
        };
        let mut node = SemanticNode::leaf(target.clone(), SemanticRole::Button, "Pad 3").with_gate(
            ProductAction::Sampler(SamplerInputAction::GateOn(pad)),
            ProductAction::Sampler(SamplerInputAction::GateOff(pad)),
        );
        node.tab_stop = true;
        let mut controller =
            ProductInputController::new(AccessibilitySnapshot { roots: vec![node] });
        let down = controller.handle_pointer(PointerInput {
            id: PointerId(9),
            phase: PointerPhase::Down,
            button: PointerButton::Primary,
            target: Some(target),
        });
        assert_eq!(
            down.last(),
            Some(&InputEffect::Dispatch(ProductAction::Sampler(
                SamplerInputAction::GateOn(pad)
            )))
        );
        assert_eq!(
            controller.handle_pointer(PointerInput {
                id: PointerId(9),
                phase: PointerPhase::Cancel,
                button: PointerButton::Primary,
                target: None,
            }),
            vec![InputEffect::Dispatch(ProductAction::Sampler(
                SamplerInputAction::GateOff(pad)
            ))]
        );
    }

    #[test]
    fn removing_a_held_pad_publication_emits_gate_off_before_focus_fallback() {
        let pad = pad(7);
        let target = FocusTarget::SamplerPad {
            view: view(11),
            pad,
        };
        let mut node = SemanticNode::leaf(target, SemanticRole::Button, "Pad 7").with_gate(
            ProductAction::Sampler(SamplerInputAction::GateOn(pad)),
            ProductAction::Sampler(SamplerInputAction::GateOff(pad)),
        );
        node.tab_stop = true;
        let mut controller =
            ProductInputController::new(AccessibilitySnapshot { roots: vec![node] });
        controller.handle_key(key(Key::Space));
        assert_eq!(
            controller.replace_snapshot(AccessibilitySnapshot::default()),
            vec![InputEffect::Dispatch(ProductAction::Sampler(
                SamplerInputAction::GateOff(pad)
            ))]
        );
    }

    #[test]
    fn pattern_grid_arrows_roam_and_delete_clears_exact_cell() {
        let pattern = PatternId::from_raw(5);
        let lane = StepLaneId::from_raw(6);
        let first = FocusTarget::PatternCell {
            view: view(12),
            pattern,
            lane,
            step: 3,
        };
        let second = FocusTarget::PatternCell {
            view: view(12),
            pattern,
            lane,
            step: 4,
        };
        let mut one = SemanticNode::leaf(first, SemanticRole::GridCell, "Kick, step 4");
        one.tab_stop = true;
        one.neighbors.right = Some(second.clone());
        let two = SemanticNode::leaf(second.clone(), SemanticRole::GridCell, "Kick, step 5");
        let mut controller = ProductInputController::new(AccessibilitySnapshot {
            roots: vec![one, two],
        });
        controller.handle_key(key(Key::Right));
        assert_eq!(controller.focused(), Some(&second));
        assert_eq!(
            controller.handle_key(key(Key::Delete)),
            vec![InputEffect::Dispatch(ProductAction::Pattern(
                PatternInputAction::ClearStep {
                    pattern,
                    lane,
                    step: 4,
                }
            ))]
        );
    }

    #[test]
    fn dirty_quit_traps_focus_and_failed_save_never_quits() {
        let mut guard = CloseGuard::default();
        let effect = guard.request(CloseScope::Application, true);
        let CloseGuardEffect::OpenPrompt {
            request, default, ..
        } = effect
        else {
            panic!("dirty quit must prompt")
        };
        assert_eq!(default, CloseChoice::Save);
        assert_eq!(
            guard.choose(request, CloseChoice::Save),
            CloseGuardEffect::SaveProject { request }
        );
        assert_eq!(
            guard.save_finished(request, false),
            CloseGuardEffect::KeepOpen
        );
        assert_eq!(guard.state(), CloseGuardState::Idle);
    }

    #[test]
    fn modal_cancel_is_keyboard_reachable_and_restores_focus() {
        let explorer = FocusTarget::ExplorerSurface(view(7));
        let cancel = FocusTarget::ClosePrompt {
            request: CloseRequestId(3),
            choice: CloseChoice::Cancel,
        };
        let mut explorer_node =
            SemanticNode::leaf(explorer.clone(), SemanticRole::Tree, "Explorer");
        explorer_node.tab_stop = true;
        let mut cancel_node = SemanticNode::leaf(cancel.clone(), SemanticRole::Button, "Cancel")
            .with_default_action(ProductAction::CloseChoice {
                request: CloseRequestId(3),
                choice: CloseChoice::Cancel,
            });
        cancel_node.tab_stop = true;
        let mut controller = ProductInputController::new(AccessibilitySnapshot {
            roots: vec![explorer_node, cancel_node],
        });
        controller.enter_modal(CloseRequestId(3), cancel);
        assert_eq!(
            controller.handle_key(key(Key::Escape)),
            vec![InputEffect::Dispatch(ProductAction::CloseChoice {
                request: CloseRequestId(3),
                choice: CloseChoice::Cancel,
            })]
        );
        controller.leave_modal();
        assert_eq!(controller.focused(), Some(&explorer));
    }

    #[test]
    fn scroll_container_consumes_page_keys_with_stable_owner() {
        let container = ScrollContainerRef {
            view: view(15),
            region: ScrollRegion::InspectorReport,
        };
        let mut node = SemanticNode::leaf(
            FocusTarget::ScrollContainer(container),
            SemanticRole::ScrollArea,
            "Inspector details",
        );
        node.tab_stop = true;
        let mut controller =
            ProductInputController::new(AccessibilitySnapshot { roots: vec![node] });
        assert_eq!(
            controller.handle_key(key(Key::PageDown)),
            vec![InputEffect::Dispatch(ProductAction::Scroll {
                container,
                amount: ScrollAmount::Page(1),
            })]
        );
    }

    #[test]
    fn publication_refresh_preserves_focus_during_pointer_owned_gesture() {
        let clip = FocusTarget::ArrangementClip {
            view: view(16),
            clip: ClipId::from_raw(2),
        };
        let mut node = SemanticNode::leaf(clip.clone(), SemanticRole::Clip, "Loop");
        node.tab_stop = true;
        let snapshot = AccessibilitySnapshot { roots: vec![node] };
        let mut controller = ProductInputController::new(snapshot.clone());
        assert!(controller.replace_snapshot(snapshot).is_empty());
        assert_eq!(controller.focused(), Some(&clip));
    }

    #[test]
    fn semantic_tree_rejects_unlabelled_duplicate_and_unpaired_controls() {
        let target = FocusTarget::SamplerPad {
            view: view(17),
            pad: pad(1),
        };
        let mut malformed = SemanticNode::leaf(target.clone(), SemanticRole::Button, "");
        malformed.actions.push(AccessibleAction::Activate);
        malformed.press_action = Some(ProductAction::Sampler(SamplerInputAction::GateOn(pad(1))));
        let snapshot = AccessibilitySnapshot {
            roots: vec![malformed.clone(), malformed],
        };
        let diagnostics = snapshot.validate();
        assert!(diagnostics
            .iter()
            .any(|item| matches!(item, SemanticTreeDiagnostic::EmptyName(_))));
        assert!(diagnostics
            .iter()
            .any(|item| matches!(item, SemanticTreeDiagnostic::DuplicateTarget(_))));
        assert!(diagnostics
            .iter()
            .any(|item| matches!(item, SemanticTreeDiagnostic::MissingGatePair(_))));
    }
}
