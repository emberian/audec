//! Native GPUI/AccessKit lowering for Audec's product-semantic surfaces.
//!
//! The portable surface remains the meaning and command authority. This
//! module owns the current platform adapter and intentionally makes no claim
//! that component tests replace VoiceOver, Narrator, or Orca walkthroughs.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::rc::Rc;

use gpui::{
    accesskit, prelude::*, AccessibleAction, App, Div, FocusHandle, Role, Stateful, Toggled, Window,
};

use super::{
    ProjectedSemanticNode, SemanticActionBinding, SemanticNativeAction, SemanticProjection,
    SemanticProjectionStamp, SemanticSurfaceNodeId, SemanticSurfaceRole, SemanticSurfaceValue,
    SemanticVisibility,
};
use crate::ui_actions::{
    ActionFlags, ActionId, ActionParameterValue, ActionParameters, ActionProjectionSnapshot,
    ActionRequest, ActionState, InvocationModifiers, InvocationOrigin, KeyChord, ProjectionEpoch,
    ShortcutResolution,
};
use crate::workspace_items::{EditorTarget, WorkspaceViewId};

pub const PLATFORM_SEMANTICS_SCHEMA_VERSION: u32 = 1;

/// Automated evidence and real platform acceptance are deliberately distinct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeAccessibilityReadiness {
    NativeElementBridgeComponentTested,
    PlatformScreenReaderQaPending,
}

pub const fn accessibility_readiness() -> [NativeAccessibilityReadiness; 2] {
    [
        NativeAccessibilityReadiness::NativeElementBridgeComponentTested,
        NativeAccessibilityReadiness::PlatformScreenReaderQaPending,
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NativeSemanticAction {
    Click,
    Focus,
    Increment,
    Decrement,
    Expand,
    Collapse,
    SetValue,
    ShowContextMenu,
    Custom(i32),
}

impl NativeSemanticAction {
    pub const fn gpui_action(self) -> AccessibleAction {
        match self {
            Self::Click => AccessibleAction::Click,
            Self::Focus => AccessibleAction::Focus,
            Self::Increment => AccessibleAction::Increment,
            Self::Decrement => AccessibleAction::Decrement,
            Self::Expand => AccessibleAction::Expand,
            Self::Collapse => AccessibleAction::Collapse,
            Self::SetValue => AccessibleAction::SetValue,
            Self::ShowContextMenu => AccessibleAction::ShowContextMenu,
            Self::Custom(_) => AccessibleAction::CustomAction,
        }
    }
}

impl From<SemanticNativeAction> for NativeSemanticAction {
    fn from(action: SemanticNativeAction) -> Self {
        match action {
            SemanticNativeAction::Custom => Self::Custom(0),
            SemanticNativeAction::Activate => Self::Click,
            SemanticNativeAction::Focus => Self::Focus,
            SemanticNativeAction::Increment => Self::Increment,
            SemanticNativeAction::Decrement => Self::Decrement,
            SemanticNativeAction::Expand => Self::Expand,
            SemanticNativeAction::Collapse => Self::Collapse,
            SemanticNativeAction::SetValue => Self::SetValue,
            SemanticNativeAction::ShowContextMenu => Self::ShowContextMenu,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeActionInstallState {
    Registered,
    Disabled,
    NodeDisabled,
    FocusScopeMismatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyboardParity {
    Verified,
    NoShortcut,
    Conflicted,
}

/// Evidence that a native action, its keyboard bindings and the focused
/// command scope all point at the same immutable registry projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FocusKeyboardParityReceipt {
    pub semantic_projection: SemanticProjectionStamp,
    pub action_projection: ProjectionEpoch,
    pub node: SemanticSurfaceNodeId,
    pub action: ActionId,
    pub native_action: NativeSemanticAction,
    pub install_state: NativeActionInstallState,
    pub keyboard_parity: KeyboardParity,
    pub shortcuts: Vec<KeyChord>,
    pub view: Option<WorkspaceViewId>,
    pub target: Option<EditorTarget>,
}

impl FocusKeyboardParityReceipt {
    pub const fn native_and_command_scope_matched(&self) -> bool {
        matches!(self.install_state, NativeActionInstallState::Registered)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeCommandBinding {
    pub action: ActionId,
    pub label: &'static str,
    pub state: ActionState,
    pub native_action: NativeSemanticAction,
    pub request: Option<ActionRequest>,
    pub receipt: FocusKeyboardParityReceipt,
}

#[derive(Clone, Debug, PartialEq)]
pub enum NativeSemanticValue {
    Text(String),
    Numeric {
        current: f64,
        minimum: Option<f64>,
        maximum: Option<f64>,
        step: Option<f64>,
        formatted: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeSemanticState {
    pub selected: bool,
    pub checked: Option<bool>,
    pub expanded: Option<bool>,
    pub disabled: bool,
    pub busy: bool,
    pub position_in_set: Option<usize>,
    pub set_size: Option<usize>,
    /// Product state only. Native focus is established by binding a real
    /// `FocusHandle`; the adapter never guesses from this observation.
    pub semantically_focused: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NativeSemanticNode {
    pub id: SemanticSurfaceNodeId,
    pub element_id: String,
    pub product_role: SemanticSurfaceRole,
    pub role: Role,
    pub role_description: Option<&'static str>,
    pub label: String,
    pub description: Option<String>,
    pub value: Option<NativeSemanticValue>,
    pub state: NativeSemanticState,
    pub commands: Vec<NativeCommandBinding>,
}

impl NativeSemanticNode {
    pub fn registered_commands(&self) -> impl Iterator<Item = &NativeCommandBinding> {
        self.commands
            .iter()
            .filter(|binding| binding.request.is_some())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedOffscreenSemanticNode {
    pub id: SemanticSurfaceNodeId,
    pub selected: bool,
    pub focused: bool,
}

/// Bounded platform payload. Offscreen retained nodes remain inspectable for
/// continuity but are not inserted into native traversal.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeSemanticProjection {
    pub schema_version: u32,
    pub semantic_stamp: SemanticProjectionStamp,
    pub action_epoch: ProjectionEpoch,
    pub nodes: Vec<NativeSemanticNode>,
    pub retained_offscreen: Vec<RetainedOffscreenSemanticNode>,
}

impl NativeSemanticProjection {
    pub fn node(&self, id: &SemanticSurfaceNodeId) -> Option<&NativeSemanticNode> {
        self.nodes.iter().find(|node| &node.id == id)
    }
}

#[derive(Clone, Debug, Default)]
pub struct GpuiPlatformSemanticAdapter;

impl GpuiPlatformSemanticAdapter {
    pub fn project(
        &self,
        semantic: &SemanticProjection,
        actions: &ActionProjectionSnapshot,
    ) -> Result<NativeSemanticProjection, PlatformSemanticError> {
        let mut nodes = Vec::new();
        let mut retained_offscreen = Vec::new();
        let mut element_ids = BTreeSet::new();
        for node in &semantic.nodes {
            match node.state.visibility {
                SemanticVisibility::Hidden => continue,
                SemanticVisibility::OffscreenRetained => {
                    retained_offscreen.push(RetainedOffscreenSemanticNode {
                        id: node.id.clone(),
                        selected: node.selection.selected,
                        focused: node.state.focused,
                    });
                    continue;
                }
                SemanticVisibility::Visible => {}
            }
            let lowered = lower_node(semantic.stamp, node, actions)?;
            if !element_ids.insert(lowered.element_id.clone()) {
                return Err(PlatformSemanticError::DuplicateElementId(
                    lowered.element_id,
                ));
            }
            nodes.push(lowered);
        }
        Ok(NativeSemanticProjection {
            schema_version: PLATFORM_SEMANTICS_SCHEMA_VERSION,
            semantic_stamp: semantic.stamp,
            action_epoch: actions.epoch,
            nodes,
            retained_offscreen,
        })
    }
}

fn lower_node(
    semantic_stamp: SemanticProjectionStamp,
    node: &ProjectedSemanticNode,
    actions: &ActionProjectionSnapshot,
) -> Result<NativeSemanticNode, PlatformSemanticError> {
    if node.label.trim().is_empty() {
        return Err(PlatformSemanticError::EmptyAccessibleName(node.id.clone()));
    }
    let value = lower_value(node)?;
    let mut description = node.description.clone();
    if let Some(summary) = &node.canvas_summary {
        append_description(&mut description, &summary.announcement());
    }
    if let Some(reason) = node.state.disabled_reason.as_deref() {
        append_description(&mut description, reason);
    }

    let position_in_set = checked_usize(node.selection.position_in_set, &node.id)?;
    let set_size = checked_usize(node.selection.set_size, &node.id)?;
    let focus_scope_matches = actions.active_view == Some(node.id.view)
        && node
            .target
            .as_ref()
            .is_none_or(|target| Some(target) == actions.target.as_ref());
    let effective_target = node.target.clone().or_else(|| actions.target.clone());
    let mut projected_checked = BTreeSet::new();
    for binding in &node.actions {
        if let Some(projected) = actions.get(binding.id) {
            if projected.descriptor.flags.contains(ActionFlags::CHECKABLE) {
                projected_checked.insert(projected.state.checked);
            }
        }
    }
    if projected_checked.len() > 1 {
        return Err(PlatformSemanticError::AmbiguousCheckedState(
            node.id.clone(),
        ));
    }
    let checked = match (node.state.checked, projected_checked.first().copied()) {
        (Some(semantic), Some(projected)) if semantic != projected => {
            return Err(PlatformSemanticError::CheckedStateMismatch {
                node: node.id.clone(),
                semantic,
                projected,
            });
        }
        (semantic, projected) => semantic.or(projected),
    };
    let mut native_kinds = BTreeMap::new();
    let mut commands = Vec::new();
    for binding in &node.actions {
        let command = lower_command(
            semantic_stamp,
            node,
            binding,
            actions,
            focus_scope_matches,
            effective_target.clone(),
        )?;
        if command.request.is_some() {
            let collision_key = match command.native_action {
                NativeSemanticAction::Custom(id) => (AccessibleAction::CustomAction, Some(id)),
                standard => (standard.gpui_action(), None),
            };
            if let Some(existing) = native_kinds.insert(collision_key, command.action) {
                return Err(PlatformSemanticError::DuplicateNativeAction {
                    node: node.id.clone(),
                    first: existing,
                    second: command.action,
                    native: command.native_action,
                });
            }
        }
        commands.push(command);
    }

    Ok(NativeSemanticNode {
        id: node.id.clone(),
        element_id: stable_element_id(&node.id),
        product_role: node.role,
        role: native_role(node.role),
        role_description: role_description(node.role),
        label: node.label.clone(),
        description,
        value,
        state: NativeSemanticState {
            selected: node.selection.selected,
            checked,
            expanded: node.state.expanded,
            disabled: node.state.disabled,
            busy: node.state.busy,
            position_in_set,
            set_size,
            semantically_focused: node.state.focused,
        },
        commands,
    })
}

fn lower_command(
    semantic_stamp: SemanticProjectionStamp,
    node: &ProjectedSemanticNode,
    binding: &SemanticActionBinding,
    actions: &ActionProjectionSnapshot,
    focus_scope_matches: bool,
    effective_target: Option<EditorTarget>,
) -> Result<NativeCommandBinding, PlatformSemanticError> {
    let projected = actions
        .get(binding.id)
        .ok_or(PlatformSemanticError::UnknownProjectedAction(binding.id))?;
    if binding.state != projected.state {
        return Err(PlatformSemanticError::ActionStateMismatch {
            node: node.id.clone(),
            action: binding.id,
            semantic: binding.state.clone(),
            projected: projected.state.clone(),
        });
    }
    let native_action = match binding.native {
        SemanticNativeAction::Custom => NativeSemanticAction::Custom(custom_action_id(binding.id)),
        action => action.into(),
    };
    let shortcuts: Vec<_> = projected
        .bindings
        .iter()
        .map(|binding| binding.chord.clone())
        .collect();
    let keyboard_parity = if shortcuts.is_empty() {
        KeyboardParity::NoShortcut
    } else if shortcuts.iter().all(|chord| {
        matches!(
            actions.resolve_projected_shortcut(chord),
            ShortcutResolution::Invoke(action) if action == binding.id
        )
    }) {
        KeyboardParity::Verified
    } else {
        KeyboardParity::Conflicted
    };
    let install_state = if node.state.disabled {
        NativeActionInstallState::NodeDisabled
    } else if !binding.state.enabled {
        NativeActionInstallState::Disabled
    } else if !focus_scope_matches {
        NativeActionInstallState::FocusScopeMismatch
    } else {
        NativeActionInstallState::Registered
    };
    let request = matches!(install_state, NativeActionInstallState::Registered)
        .then(|| {
            actions.request_for_target(
                binding.id,
                InvocationOrigin::Accessibility,
                InvocationModifiers::default(),
                ActionParameters::default(),
                Some(node.id.view),
                effective_target.clone(),
            )
        })
        .transpose()
        .map_err(PlatformSemanticError::ActionRequest)?;
    let receipt = FocusKeyboardParityReceipt {
        semantic_projection: semantic_stamp,
        action_projection: actions.epoch,
        node: node.id.clone(),
        action: binding.id,
        native_action,
        install_state,
        keyboard_parity,
        shortcuts,
        view: Some(node.id.view),
        target: effective_target,
    };
    Ok(NativeCommandBinding {
        action: binding.id,
        label: projected.descriptor.label,
        state: binding.state.clone(),
        native_action,
        request,
        receipt,
    })
}

fn lower_value(
    node: &ProjectedSemanticNode,
) -> Result<Option<NativeSemanticValue>, PlatformSemanticError> {
    match node.value.as_ref() {
        None => Ok(None),
        Some(SemanticSurfaceValue::Text(text)) => Ok(Some(NativeSemanticValue::Text(text.clone()))),
        Some(SemanticSurfaceValue::ProjectFrame { formatted, .. }) => {
            Ok(Some(NativeSemanticValue::Text(formatted.clone())))
        }
        Some(SemanticSurfaceValue::Numeric(value)) => {
            for candidate in [
                Some(value.current),
                value.minimum,
                value.maximum,
                value.step,
            ]
            .into_iter()
            .flatten()
            {
                if !candidate.is_finite() {
                    return Err(PlatformSemanticError::NonFiniteNumericValue(
                        node.id.clone(),
                    ));
                }
            }
            Ok(Some(NativeSemanticValue::Numeric {
                current: value.current,
                minimum: value.minimum,
                maximum: value.maximum,
                step: value.step,
                formatted: value.formatted.clone(),
            }))
        }
    }
}

fn checked_usize(
    value: Option<u64>,
    node: &SemanticSurfaceNodeId,
) -> Result<Option<usize>, PlatformSemanticError> {
    value
        .map(|value| {
            usize::try_from(value).map_err(|_| PlatformSemanticError::CollectionValueTooLarge {
                node: node.clone(),
                value,
            })
        })
        .transpose()
}

fn append_description(description: &mut Option<String>, suffix: &str) {
    match description {
        Some(description) => {
            if !description.ends_with('.') {
                description.push('.');
            }
            description.push(' ');
            description.push_str(suffix);
        }
        None => *description = Some(suffix.into()),
    }
}

fn stable_element_id(id: &SemanticSurfaceNodeId) -> String {
    format!("audec-semantic/{}/{}", id.view.0, id.local_key)
}

pub const fn native_role(role: SemanticSurfaceRole) -> Role {
    match role {
        SemanticSurfaceRole::Application => Role::Application,
        SemanticSurfaceRole::Window => Role::Window,
        SemanticSurfaceRole::Workspace => Role::Main,
        SemanticSurfaceRole::Region => Role::Region,
        SemanticSurfaceRole::Group => Role::Group,
        SemanticSurfaceRole::Toolbar => Role::Toolbar,
        SemanticSurfaceRole::TabList => Role::TabList,
        SemanticSurfaceRole::Tab => Role::Tab,
        SemanticSurfaceRole::TabPanel => Role::TabPanel,
        SemanticSurfaceRole::Button | SemanticSurfaceRole::ToggleButton => Role::Button,
        SemanticSurfaceRole::Checkbox => Role::CheckBox,
        SemanticSurfaceRole::TextInput => Role::TextInput,
        SemanticSurfaceRole::Slider => Role::Slider,
        SemanticSurfaceRole::Meter => Role::Meter,
        SemanticSurfaceRole::List => Role::List,
        SemanticSurfaceRole::ListItem | SemanticSurfaceRole::Clip => Role::ListItem,
        SemanticSurfaceRole::Grid | SemanticSurfaceRole::Timeline => Role::Grid,
        SemanticSurfaceRole::Row => Role::Row,
        SemanticSurfaceRole::Cell
        | SemanticSurfaceRole::Note
        | SemanticSurfaceRole::Step
        | SemanticSurfaceRole::AutomationPoint => Role::GridCell,
        SemanticSurfaceRole::Tree => Role::Tree,
        SemanticSurfaceRole::TreeItem => Role::TreeItem,
        SemanticSurfaceRole::Menu => Role::Menu,
        SemanticSurfaceRole::MenuItem => Role::MenuItem,
        SemanticSurfaceRole::Status => Role::Status,
        SemanticSurfaceRole::Alert => Role::Alert,
        SemanticSurfaceRole::Graphic
        | SemanticSurfaceRole::Waveform
        | SemanticSurfaceRole::Spectrogram => Role::GraphicsObject,
        SemanticSurfaceRole::MixerChannel => Role::Group,
    }
}

const fn role_description(role: SemanticSurfaceRole) -> Option<&'static str> {
    match role {
        SemanticSurfaceRole::Timeline => Some("audio timeline"),
        SemanticSurfaceRole::Waveform => Some("audio waveform"),
        SemanticSurfaceRole::Spectrogram => Some("audio spectrogram"),
        SemanticSurfaceRole::Clip => Some("audio clip"),
        SemanticSurfaceRole::Note => Some("musical note"),
        SemanticSurfaceRole::Step => Some("sequencer step"),
        SemanticSurfaceRole::AutomationPoint => Some("automation point"),
        SemanticSurfaceRole::MixerChannel => Some("mixer channel"),
        _ => None,
    }
}

fn custom_action_id(action: ActionId) -> i32 {
    let mut hash = 0x811c_9dc5u32;
    for byte in action.0.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash as i32
}

/// Event delivered by the real GPUI accessibility callback. The request still
/// carries the projection epoch and must pass `ActionRegistry::validate_request`
/// at the command authority immediately before mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeSemanticDispatch {
    pub request: ActionRequest,
    pub receipt: FocusKeyboardParityReceipt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeSemanticDispatchError {
    UnknownCustomAction(i32),
    MissingActionData(NativeSemanticAction),
    UnexpectedActionData(NativeSemanticAction),
}

type DispatchHandler =
    dyn Fn(Result<NativeSemanticDispatch, NativeSemanticDispatchError>, &mut Window, &mut App);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeFocusTracking {
    /// Appropriate for a non-interactive graphic or summary node.
    NotRequested,
    /// `track_focus_element` was installed with the pane's real focus handle.
    /// The frame-scoped `Window::focused_element_id` query remains the runtime
    /// proof of which node actually owns focus.
    TrackElementRequested,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeElementBindingReceipt {
    pub node: SemanticSurfaceNodeId,
    pub element_id: String,
    pub focus_tracking: NativeFocusTracking,
    pub semantically_focused: bool,
    pub registered_actions: Vec<ActionId>,
    pub keyboard_verified_actions: Vec<ActionId>,
}

pub struct NativeSemanticElementBinding {
    pub element: Stateful<Div>,
    pub receipt: NativeElementBindingReceipt,
}

impl NativeSemanticElementBinding {
    pub fn into_element(self) -> Stateful<Div> {
        self.element
    }
}

/// Apply one projected node to a real GPUI `Div`.
///
/// Passing the control's actual focus handle is what establishes keyboard /
/// AccessKit focus parity. Omitting it is supported for non-focusable graphics,
/// but the adapter never converts `semantically_focused` into a native focus
/// assertion by guessing.
pub fn bind_native_semantics(
    element: Div,
    node: &NativeSemanticNode,
    focus: Option<&FocusHandle>,
    on_dispatch: impl Fn(Result<NativeSemanticDispatch, NativeSemanticDispatchError>, &mut Window, &mut App)
        + 'static,
) -> NativeSemanticElementBinding {
    let focus_tracking = if focus.is_some() {
        NativeFocusTracking::TrackElementRequested
    } else {
        NativeFocusTracking::NotRequested
    };
    let mut element = element
        .id(node.element_id.clone())
        .role(node.role)
        .aria_label(node.label.clone())
        .aria_selected(node.state.selected);
    if let Some(focus) = focus {
        element = element.track_focus_element(focus);
    }
    if let Some(checked) = node.state.checked {
        element = element.aria_toggled(Toggled::from(checked));
    }
    if let Some(expanded) = node.state.expanded {
        element = element.aria_expanded(expanded);
    }
    if let Some(position) = node.state.position_in_set {
        element = element.aria_position_in_set(position);
    }
    if let Some(size) = node.state.set_size {
        element = element.aria_size_of_set(size);
    }
    if let Some(value) = &node.value {
        match value {
            NativeSemanticValue::Text(value) => {
                element = element.aria_value(value.clone());
            }
            NativeSemanticValue::Numeric {
                current,
                minimum,
                maximum,
                step,
                formatted,
            } => {
                element = element
                    .aria_numeric_value(*current)
                    .aria_value(formatted.clone());
                if let Some(minimum) = minimum {
                    element = element.aria_min_numeric_value(*minimum);
                }
                if let Some(maximum) = maximum {
                    element = element.aria_max_numeric_value(*maximum);
                }
                if let Some(step) = step {
                    element = element.aria_numeric_value_step(*step);
                }
            }
        }
    }

    let custom_actions: Vec<_> = node
        .registered_commands()
        .filter_map(|binding| match binding.native_action {
            NativeSemanticAction::Custom(id) => Some(accesskit::CustomAction {
                id,
                description: binding.label.into(),
            }),
            _ => None,
        })
        .collect();
    let description = node.description.clone();
    let role_description = node.role_description;
    let disabled = node.state.disabled;
    let busy = node.state.busy;
    let keyboard_shortcut = node
        .registered_commands()
        .find_map(|binding| binding.receipt.shortcuts.first().map(ToString::to_string));
    element = element.a11y_synthetic_children(move |builder| {
        let native = builder.parent_node();
        if let Some(description) = &description {
            native.set_description(description.clone());
        }
        if let Some(role_description) = role_description {
            native.set_role_description(role_description);
        }
        if disabled {
            native.set_disabled();
        }
        if busy {
            native.set_busy();
        }
        if let Some(shortcut) = &keyboard_shortcut {
            native.set_keyboard_shortcut(shortcut.clone());
        }
        if !custom_actions.is_empty() {
            native.set_custom_actions(custom_actions);
        }
    });

    let handler: Rc<DispatchHandler> = Rc::new(on_dispatch);
    let mut custom_bindings = Vec::new();
    for binding in node.registered_commands() {
        if matches!(binding.native_action, NativeSemanticAction::Custom(_)) {
            custom_bindings.push(binding.clone());
            continue;
        }
        let binding = binding.clone();
        let handler = Rc::clone(&handler);
        element = element.on_a11y_action(
            binding.native_action.gpui_action(),
            move |data, window, cx| {
                handler(dispatch_for_binding(&binding, data), window, cx);
            },
        );
    }
    if !custom_bindings.is_empty() {
        let handler = Rc::clone(&handler);
        element =
            element.on_a11y_action(AccessibleAction::CustomAction, move |data, window, cx| {
                let id = match data {
                    Some(accesskit::ActionData::CustomAction(id)) => *id,
                    _ => {
                        handler(
                            Err(NativeSemanticDispatchError::MissingActionData(
                                NativeSemanticAction::Custom(0),
                            )),
                            window,
                            cx,
                        );
                        return;
                    }
                };
                match custom_bindings
                    .iter()
                    .find(|binding| binding.native_action == NativeSemanticAction::Custom(id))
                {
                    Some(binding) => handler(dispatch_for_binding(binding, data), window, cx),
                    None => handler(
                        Err(NativeSemanticDispatchError::UnknownCustomAction(id)),
                        window,
                        cx,
                    ),
                }
            });
    }
    let registered_actions = node
        .registered_commands()
        .map(|binding| binding.action)
        .collect();
    let keyboard_verified_actions = node
        .registered_commands()
        .filter(|binding| binding.receipt.keyboard_parity == KeyboardParity::Verified)
        .map(|binding| binding.action)
        .collect();
    NativeSemanticElementBinding {
        element,
        receipt: NativeElementBindingReceipt {
            node: node.id.clone(),
            element_id: node.element_id.clone(),
            focus_tracking,
            semantically_focused: node.state.semantically_focused,
            registered_actions,
            keyboard_verified_actions,
        },
    }
}

fn dispatch_for_binding(
    binding: &NativeCommandBinding,
    data: Option<&accesskit::ActionData>,
) -> Result<NativeSemanticDispatch, NativeSemanticDispatchError> {
    let mut request = binding
        .request
        .clone()
        .expect("only registered native commands receive handlers");
    match (binding.native_action, data) {
        (NativeSemanticAction::SetValue, Some(accesskit::ActionData::NumericValue(value))) => {
            request.parameters.insert(
                "accessibility_numeric_value_bits",
                ActionParameterValue::Unsigned(value.to_bits()),
            );
        }
        (NativeSemanticAction::SetValue, Some(accesskit::ActionData::Value(value))) => {
            request.parameters.insert(
                "accessibility_text_value",
                ActionParameterValue::Text(value.to_string()),
            );
        }
        (NativeSemanticAction::SetValue, _) => {
            return Err(NativeSemanticDispatchError::MissingActionData(
                NativeSemanticAction::SetValue,
            ));
        }
        (NativeSemanticAction::Custom(expected), Some(accesskit::ActionData::CustomAction(id)))
            if expected == *id => {}
        (NativeSemanticAction::Custom(_), _) => {
            return Err(NativeSemanticDispatchError::UnexpectedActionData(
                binding.native_action,
            ));
        }
        (_, None) => {}
        (_, Some(_)) => {
            return Err(NativeSemanticDispatchError::UnexpectedActionData(
                binding.native_action,
            ));
        }
    }
    Ok(NativeSemanticDispatch {
        request,
        receipt: binding.receipt.clone(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlatformSemanticError {
    EmptyAccessibleName(SemanticSurfaceNodeId),
    DuplicateElementId(String),
    UnknownProjectedAction(ActionId),
    ActionStateMismatch {
        node: SemanticSurfaceNodeId,
        action: ActionId,
        semantic: ActionState,
        projected: ActionState,
    },
    CheckedStateMismatch {
        node: SemanticSurfaceNodeId,
        semantic: bool,
        projected: bool,
    },
    AmbiguousCheckedState(SemanticSurfaceNodeId),
    DuplicateNativeAction {
        node: SemanticSurfaceNodeId,
        first: ActionId,
        second: ActionId,
        native: NativeSemanticAction,
    },
    NonFiniteNumericValue(SemanticSurfaceNodeId),
    CollectionValueTooLarge {
        node: SemanticSurfaceNodeId,
        value: u64,
    },
    ActionRequest(crate::ui_actions::ActionDispatchError),
}

impl fmt::Display for PlatformSemanticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyAccessibleName(node) => {
                write!(formatter, "semantic node {node:?} has no accessible name")
            }
            Self::DuplicateElementId(id) => {
                write!(formatter, "native element ID {id} is duplicated")
            }
            Self::UnknownProjectedAction(action) => {
                write!(formatter, "action {} is absent from the action projection", action.0)
            }
            Self::ActionStateMismatch {
                node,
                action,
                semantic,
                projected,
            } => write!(
                formatter,
                "semantic/native action state disagrees for {} on {node:?}: {semantic:?} != {projected:?}",
                action.0
            ),
            Self::CheckedStateMismatch {
                node,
                semantic,
                projected,
            } => write!(
                formatter,
                "semantic/native checked state disagrees on {node:?}: {semantic} != {projected}"
            ),
            Self::AmbiguousCheckedState(node) => write!(
                formatter,
                "semantic node {node:?} exposes conflicting checkable action states"
            ),
            Self::DuplicateNativeAction {
                node,
                first,
                second,
                native,
            } => write!(
                formatter,
                "actions {} and {} both claim native action {native:?} on {node:?}",
                first.0, second.0
            ),
            Self::NonFiniteNumericValue(node) => {
                write!(formatter, "semantic node {node:?} has a non-finite numeric value")
            }
            Self::CollectionValueTooLarge { node, value } => write!(
                formatter,
                "semantic collection value {value} on {node:?} exceeds this platform's usize"
            ),
            Self::ActionRequest(error) => write!(formatter, "native action request: {error}"),
        }
    }
}

impl Error for PlatformSemanticError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_actions::{ids, ActionContext, ActionRegistry, ContextEpoch, UserKeymap};
    use crate::workspace::accessibility::{
        scope_canvas_semantics, CanvasOffscreenPolicy, CanvasSemanticChild, CanvasSemanticPolicy,
        CanvasVisibleWindow, SemanticActionBinding, SemanticNumericValue, SemanticSurface,
        SemanticSurfaceNode,
    };
    use crate::workspace_document::LegacyBuiltinView;

    fn projected_surface() -> (SemanticProjection, ActionProjectionSnapshot) {
        let view = LegacyBuiltinView::Track.id();
        let registry = ActionRegistry::audec_defaults();
        let context = ActionContext {
            epoch: ContextEpoch(8),
            has_project: true,
            has_selection: true,
            active_view: Some(view),
            target: Some(EditorTarget::Arrangement),
            ..ActionContext::default()
        };
        let actions = registry.project(&context, &UserKeymap::default());
        let mut root = SemanticSurfaceNode::new(
            SemanticSurfaceNodeId::root(view),
            SemanticSurfaceRole::Timeline,
            "Arrangement timeline",
        );
        root.target = Some(EditorTarget::Arrangement);
        root.value = Some(SemanticSurfaceValue::Numeric(SemanticNumericValue {
            current: 2.0,
            minimum: Some(0.0),
            maximum: Some(8.0),
            step: Some(0.25),
            formatted: "bar 3".into(),
            unit: Some("bars".into()),
        }));
        root.actions.push(
            SemanticActionBinding::resolve_as(
                &registry,
                ids::EDIT_DELETE,
                &context,
                SemanticNativeAction::Activate,
            )
            .unwrap(),
        );
        (
            SemanticSurface::new(view, 19, root).unwrap().project(),
            actions,
        )
    }

    #[test]
    fn stable_product_semantics_lower_to_real_native_metadata() {
        let (semantic, actions) = projected_surface();
        let native = GpuiPlatformSemanticAdapter
            .project(&semantic, &actions)
            .unwrap();
        let node = &native.nodes[0];
        assert_eq!(node.element_id, "audec-semantic/1/surface");
        assert_eq!(node.role, Role::Grid);
        assert_eq!(node.role_description, Some("audio timeline"));
        assert!(matches!(
            node.value,
            Some(NativeSemanticValue::Numeric { current: 2.0, .. })
        ));
        assert_eq!(node.commands[0].native_action, NativeSemanticAction::Click);
        assert!(node.commands[0].request.is_some());
    }

    #[test]
    fn action_receipt_proves_native_keyboard_and_focus_scope_parity() {
        let (semantic, actions) = projected_surface();
        let native = GpuiPlatformSemanticAdapter
            .project(&semantic, &actions)
            .unwrap();
        let receipt = &native.nodes[0].commands[0].receipt;
        assert!(receipt.native_and_command_scope_matched());
        assert_eq!(receipt.keyboard_parity, KeyboardParity::Verified);
        assert_eq!(receipt.action, ids::EDIT_DELETE);
        assert_eq!(receipt.action_projection.context, ContextEpoch(8));
        assert_eq!(receipt.target, Some(EditorTarget::Arrangement));
    }

    #[test]
    fn checkable_and_disabled_state_come_from_the_same_action_projection() {
        let view = LegacyBuiltinView::Track.id();
        let registry = ActionRegistry::audec_defaults();
        let context = ActionContext {
            has_project: true,
            active_view: Some(view),
            target: Some(EditorTarget::Arrangement),
            loop_enabled: true,
            ..ActionContext::default()
        };
        let actions = registry.project(&context, &UserKeymap::default());
        let mut root = SemanticSurfaceNode::new(
            SemanticSurfaceNodeId::root(view),
            SemanticSurfaceRole::ToggleButton,
            "Loop playback",
        );
        root.target = Some(EditorTarget::Arrangement);
        root.actions.push(
            SemanticActionBinding::resolve_as(
                &registry,
                ids::LOOP_TOGGLE,
                &context,
                SemanticNativeAction::Activate,
            )
            .unwrap(),
        );
        let semantic = SemanticSurface::new(view, 3, root).unwrap().project();
        let native = GpuiPlatformSemanticAdapter
            .project(&semantic, &actions)
            .unwrap();
        assert_eq!(native.nodes[0].state.checked, Some(true));
        assert_eq!(
            native.nodes[0].commands[0].receipt.install_state,
            NativeActionInstallState::Registered
        );

        let disabled_context = ActionContext {
            active_view: Some(view),
            target: Some(EditorTarget::Arrangement),
            ..ActionContext::default()
        };
        let disabled_actions = registry.project(&disabled_context, &UserKeymap::default());
        let mut disabled_root = SemanticSurfaceNode::new(
            SemanticSurfaceNodeId::root(view),
            SemanticSurfaceRole::ToggleButton,
            "Loop playback",
        );
        disabled_root.target = Some(EditorTarget::Arrangement);
        disabled_root.actions.push(
            SemanticActionBinding::resolve_as(
                &registry,
                ids::LOOP_TOGGLE,
                &disabled_context,
                SemanticNativeAction::Activate,
            )
            .unwrap(),
        );
        let disabled_semantic = SemanticSurface::new(view, 4, disabled_root)
            .unwrap()
            .project();
        let disabled_native = GpuiPlatformSemanticAdapter
            .project(&disabled_semantic, &disabled_actions)
            .unwrap();
        assert_eq!(
            disabled_native.nodes[0].commands[0].state.disabled_reason,
            Some("No project is open")
        );
        assert!(disabled_native.nodes[0].commands[0].request.is_none());
    }

    #[test]
    fn offscreen_canvas_objects_are_summarized_not_put_in_native_traversal() {
        let view = LegacyBuiltinView::Track.id();
        let registry = ActionRegistry::audec_defaults();
        let context = ActionContext {
            has_project: true,
            has_selection: true,
            active_view: Some(view),
            target: Some(EditorTarget::Arrangement),
            ..ActionContext::default()
        };
        let actions = registry.project(&context, &UserKeymap::default());
        let children = (0..10).map(|ordinal| {
            let mut node = SemanticSurfaceNode::new(
                SemanticSurfaceNodeId::object(view, "clip", ordinal),
                SemanticSurfaceRole::Clip,
                format!("Clip {ordinal}"),
            );
            node.target = Some(EditorTarget::Arrangement);
            if ordinal == 0 {
                node.selection.selected = true;
            }
            CanvasSemanticChild { ordinal, node }
        });
        let scoped = scope_canvas_semantics(
            children,
            CanvasSemanticPolicy {
                window: CanvasVisibleWindow {
                    first: 4,
                    count: 2,
                    total: 10,
                },
                offscreen: CanvasOffscreenPolicy::RetainFocusedAndSelected,
            },
        )
        .unwrap();
        let mut root = SemanticSurfaceNode::new(
            SemanticSurfaceNodeId::root(view),
            SemanticSurfaceRole::Timeline,
            "Timeline",
        );
        root.target = Some(EditorTarget::Arrangement);
        root.install_canvas_semantics(scoped);
        let semantic = SemanticSurface::new(view, 2, root).unwrap().project();
        let native = GpuiPlatformSemanticAdapter
            .project(&semantic, &actions)
            .unwrap();
        assert_eq!(native.nodes.len(), 3); // canvas + visible clips 4 and 5
        assert_eq!(native.retained_offscreen.len(), 1);
        let description = native.nodes[0].description.as_deref().unwrap();
        assert!(description.contains("10 items total"));
        assert!(description.contains("7 offscreen items omitted"));
    }

    #[test]
    fn focus_scope_mismatch_never_installs_a_native_mutation_callback() {
        let (semantic, mut actions) = projected_surface();
        actions.active_view = Some(LegacyBuiltinView::Waterfall.id());
        let native = GpuiPlatformSemanticAdapter
            .project(&semantic, &actions)
            .unwrap();
        let command = &native.nodes[0].commands[0];
        assert_eq!(
            command.receipt.install_state,
            NativeActionInstallState::FocusScopeMismatch
        );
        assert!(command.request.is_none());
    }

    #[test]
    fn release_status_does_not_claim_unrun_screen_reader_qa() {
        assert!(accessibility_readiness()
            .contains(&NativeAccessibilityReadiness::PlatformScreenReaderQaPending));
    }
}
