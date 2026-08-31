//! Stable, GPUI-independent action metadata and invocation context.
//!
//! Buttons, menus, shortcuts, accessibility, context menus, and the command
//! palette should all resolve one [`ActionId`]. This registry describes intent
//! and availability only; project mutations still pass through command
//! envelopes, and editor-local navigation remains view state.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::workspace_items::{EditorTarget, WorkspaceItemKind, WorkspaceViewId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionId(pub &'static str);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionCategory {
    File,
    Edit,
    View,
    Transport,
    Workspace,
    Track,
    Clip,
    Pattern,
    Automation,
    Mixer,
    Analysis,
    Help,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorClass {
    Arrangement,
    Pattern,
    Automation,
    Mixer,
    Browser,
    Inspector,
    Analysis,
}

impl WorkspaceItemKind {
    pub const fn editor_class(self) -> EditorClass {
        match self {
            Self::Overview | Self::Arrangement => EditorClass::Arrangement,
            Self::Browser => EditorClass::Browser,
            Self::Inspector => EditorClass::Inspector,
            Self::PatternEditor => EditorClass::Pattern,
            Self::AutomationEditor => EditorClass::Automation,
            Self::Mixer => EditorClass::Mixer,
            Self::Analysis(_) => EditorClass::Analysis,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionScope {
    Application,
    Project,
    Workspace,
    Editor(EditorClass),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActionFlags(u16);

impl ActionFlags {
    pub const NONE: Self = Self(0);
    pub const REQUIRES_PROJECT: Self = Self(1 << 0);
    pub const REQUIRES_SELECTION: Self = Self(1 << 1);
    pub const ALLOW_IN_TEXT_INPUT: Self = Self(1 << 2);
    pub const ALLOW_IN_MODAL: Self = Self(1 << 3);
    pub const CHECKABLE: Self = Self(1 << 4);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionDescriptor {
    pub id: ActionId,
    pub label: &'static str,
    pub category: ActionCategory,
    pub scope: ActionScope,
    /// Platform-shaped strings interpreted only by the GPUI binding adapter.
    pub default_keys: &'static [&'static str],
    pub flags: ActionFlags,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionContext {
    pub has_project: bool,
    pub has_selection: bool,
    pub active_view: Option<WorkspaceViewId>,
    pub active_kind: Option<WorkspaceItemKind>,
    pub target: Option<EditorTarget>,
    pub text_input_focused: bool,
    pub modal_active: bool,
    pub can_undo: bool,
    pub can_redo: bool,
    pub loop_enabled: bool,
    pub transport_playing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionState {
    pub enabled: bool,
    pub checked: bool,
    pub disabled_reason: Option<&'static str>,
}

impl ActionState {
    pub const fn enabled() -> Self {
        Self {
            enabled: true,
            checked: false,
            disabled_reason: None,
        }
    }

    pub const fn disabled(reason: &'static str) -> Self {
        Self {
            enabled: false,
            checked: false,
            disabled_reason: Some(reason),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InvocationModifiers {
    pub shift: bool,
    pub command: bool,
    pub option: bool,
    pub control: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvocationOrigin {
    Shortcut,
    Menu,
    ContextMenu,
    Toolbar,
    Palette,
    Accessibility,
    ExternalProtocol,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionInvocation {
    pub action: ActionId,
    pub origin: InvocationOrigin,
    pub view: Option<WorkspaceViewId>,
    pub target: Option<EditorTarget>,
    pub modifiers: InvocationModifiers,
}

#[derive(Clone, Debug, Default)]
pub struct ActionRegistry {
    descriptors: BTreeMap<ActionId, ActionDescriptor>,
}

impl ActionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn audec_defaults() -> Self {
        let mut registry = Self::new();
        for descriptor in builtins() {
            registry
                .register(descriptor)
                .expect("built-in action IDs are unique");
        }
        registry
    }

    pub fn register(&mut self, descriptor: ActionDescriptor) -> Result<(), ActionRegistryError> {
        if descriptor.id.0.trim().is_empty() {
            return Err(ActionRegistryError::EmptyId);
        }
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(ActionRegistryError::DuplicateId(descriptor.id));
        }
        self.descriptors.insert(descriptor.id, descriptor);
        Ok(())
    }

    pub fn get(&self, id: ActionId) -> Option<&ActionDescriptor> {
        self.descriptors.get(&id)
    }

    pub fn descriptors(&self) -> impl ExactSizeIterator<Item = &ActionDescriptor> {
        self.descriptors.values()
    }

    pub fn resolve(&self, id: ActionId, context: &ActionContext) -> Option<ActionState> {
        let descriptor = self.get(id)?;
        let mut state = if context.modal_active
            && !descriptor.flags.contains(ActionFlags::ALLOW_IN_MODAL)
        {
            ActionState::disabled("A modal operation is active")
        } else if context.text_input_focused
            && !descriptor.flags.contains(ActionFlags::ALLOW_IN_TEXT_INPUT)
        {
            ActionState::disabled("Text input has keyboard focus")
        } else if descriptor.flags.contains(ActionFlags::REQUIRES_PROJECT) && !context.has_project {
            ActionState::disabled("No project is open")
        } else if descriptor.flags.contains(ActionFlags::REQUIRES_SELECTION)
            && !context.has_selection
        {
            ActionState::disabled("Nothing is selected")
        } else if let ActionScope::Editor(required) = descriptor.scope {
            if context.active_kind.map(WorkspaceItemKind::editor_class) != Some(required) {
                ActionState::disabled("The focused editor does not support this action")
            } else {
                ActionState::enabled()
            }
        } else {
            ActionState::enabled()
        };

        if state.enabled {
            state = match id.0 {
                "audec.edit.undo" if !context.can_undo => {
                    ActionState::disabled("There is nothing to undo")
                }
                "audec.edit.redo" if !context.can_redo => {
                    ActionState::disabled("There is nothing to redo")
                }
                "audec.loop.toggle" => ActionState {
                    checked: context.loop_enabled,
                    ..state
                },
                "audec.transport.toggle" => ActionState {
                    checked: context.transport_playing,
                    ..state
                },
                _ => state,
            };
        }
        Some(state)
    }
}

const PROJECT: ActionFlags = ActionFlags::REQUIRES_PROJECT;
const PROJECT_SELECTION: ActionFlags =
    ActionFlags::REQUIRES_PROJECT.union(ActionFlags::REQUIRES_SELECTION);
const TEXT_SAFE_PROJECT: ActionFlags =
    ActionFlags::REQUIRES_PROJECT.union(ActionFlags::ALLOW_IN_TEXT_INPUT);

fn builtins() -> Vec<ActionDescriptor> {
    vec![
        action(
            "audec.file.open",
            "Open…",
            ActionCategory::File,
            ActionScope::Application,
            &["cmd-o"],
            ActionFlags::ALLOW_IN_TEXT_INPUT.union(ActionFlags::ALLOW_IN_MODAL),
        ),
        action(
            "audec.file.save",
            "Save",
            ActionCategory::File,
            ActionScope::Project,
            &["cmd-s"],
            TEXT_SAFE_PROJECT,
        ),
        action(
            "audec.file.export",
            "Export Audio…",
            ActionCategory::File,
            ActionScope::Project,
            &["cmd-shift-e"],
            PROJECT,
        ),
        action(
            "audec.transport.toggle",
            "Play / Pause",
            ActionCategory::Transport,
            ActionScope::Project,
            &["space"],
            PROJECT.union(ActionFlags::CHECKABLE),
        ),
        action(
            "audec.transport.stop",
            "Stop",
            ActionCategory::Transport,
            ActionScope::Project,
            &["shift-space"],
            PROJECT,
        ),
        action(
            "audec.loop.toggle",
            "Loop",
            ActionCategory::Transport,
            ActionScope::Project,
            &["l"],
            PROJECT.union(ActionFlags::CHECKABLE),
        ),
        action(
            "audec.edit.undo",
            "Undo",
            ActionCategory::Edit,
            ActionScope::Project,
            &["cmd-z"],
            TEXT_SAFE_PROJECT,
        ),
        action(
            "audec.edit.redo",
            "Redo",
            ActionCategory::Edit,
            ActionScope::Project,
            &["cmd-shift-z"],
            TEXT_SAFE_PROJECT,
        ),
        action(
            "audec.edit.delete",
            "Delete",
            ActionCategory::Edit,
            ActionScope::Project,
            &["delete", "backspace"],
            PROJECT_SELECTION,
        ),
        action(
            "audec.edit.duplicate",
            "Duplicate",
            ActionCategory::Edit,
            ActionScope::Project,
            &["cmd-d"],
            PROJECT_SELECTION,
        ),
        action(
            "audec.clip.split",
            "Split Clip",
            ActionCategory::Clip,
            ActionScope::Editor(EditorClass::Arrangement),
            &["cmd-e"],
            PROJECT_SELECTION,
        ),
        action(
            "audec.editor.arrangement",
            "Arrangement",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-1"],
            PROJECT,
        ),
        action(
            "audec.editor.piano_roll",
            "Piano Roll",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-2"],
            PROJECT,
        ),
        action(
            "audec.editor.drums",
            "Drum Editor",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-3"],
            PROJECT,
        ),
        action(
            "audec.editor.automation",
            "Automation",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-4"],
            PROJECT,
        ),
        action(
            "audec.editor.mixer",
            "Mixer",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-5"],
            PROJECT,
        ),
        action(
            "audec.palette.open",
            "Command Palette",
            ActionCategory::Workspace,
            ActionScope::Application,
            &["cmd-shift-p"],
            ActionFlags::ALLOW_IN_TEXT_INPUT,
        ),
    ]
}

const fn action(
    id: &'static str,
    label: &'static str,
    category: ActionCategory,
    scope: ActionScope,
    default_keys: &'static [&'static str],
    flags: ActionFlags,
) -> ActionDescriptor {
    ActionDescriptor {
        id: ActionId(id),
        label,
        category,
        scope,
        default_keys,
        flags,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionRegistryError {
    EmptyId,
    DuplicateId(ActionId),
}

impl fmt::Display for ActionRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyId => formatter.write_str("action ID must not be empty"),
            Self::DuplicateId(id) => write!(formatter, "action {} is registered twice", id.0),
        }
    }
}

impl Error for ActionRegistryError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_unique_stable_ids() {
        let registry = ActionRegistry::audec_defaults();
        assert_eq!(registry.descriptors().count(), 17);
        assert_eq!(
            registry
                .get(ActionId("audec.transport.toggle"))
                .unwrap()
                .label,
            "Play / Pause"
        );
    }

    #[test]
    fn text_input_suppresses_destructive_actions_but_not_save() {
        let registry = ActionRegistry::audec_defaults();
        let context = ActionContext {
            has_project: true,
            has_selection: true,
            text_input_focused: true,
            ..ActionContext::default()
        };
        assert!(
            !registry
                .resolve(ActionId("audec.edit.delete"), &context)
                .unwrap()
                .enabled
        );
        assert!(
            registry
                .resolve(ActionId("audec.file.save"), &context)
                .unwrap()
                .enabled
        );
    }
}
