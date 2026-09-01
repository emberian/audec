//! Stable, GPUI-independent action metadata and invocation context.
//!
//! Buttons, menus, shortcuts, accessibility, context menus, and the command
//! palette should all resolve one [`ActionId`]. This registry describes intent
//! and availability only; project mutations still pass through command
//! envelopes, and editor-local navigation remains view state.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::workspace_items::{EditorTarget, WorkspaceItemKind, WorkspaceViewId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionId(pub &'static str);

impl ActionId {
    pub const fn new(id: &'static str) -> Self {
        Self(id)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

/// Built-in identities are constants so platform adapters never need to
/// manufacture strings. The textual values remain the persistence and
/// external-protocol representation.
pub mod ids {
    use super::ActionId;

    pub const FILE_NEW: ActionId = ActionId::new("audec.file.new");
    pub const FILE_OPEN: ActionId = ActionId::new("audec.file.open");
    pub const FILE_OPEN_AUDIO: ActionId = ActionId::new("audec.file.open_audio");
    pub const FILE_SAVE: ActionId = ActionId::new("audec.file.save");
    pub const FILE_SAVE_AS: ActionId = ActionId::new("audec.file.save_as");
    pub const FILE_RECOVERY: ActionId = ActionId::new("audec.file.recovery");
    pub const FILE_EXPORT: ActionId = ActionId::new("audec.file.export");
    pub const FILE_QUIT: ActionId = ActionId::new("audec.file.quit");
    pub const TRANSPORT_TOGGLE: ActionId = ActionId::new("audec.transport.toggle");
    pub const TRANSPORT_STOP: ActionId = ActionId::new("audec.transport.stop");
    pub const LOOP_TOGGLE: ActionId = ActionId::new("audec.loop.toggle");
    pub const LOOP_FROM_SELECTION: ActionId = ActionId::new("audec.loop.from_selection");
    pub const LOOP_CLEAR: ActionId = ActionId::new("audec.loop.clear");
    pub const EDIT_UNDO: ActionId = ActionId::new("audec.edit.undo");
    pub const EDIT_REDO: ActionId = ActionId::new("audec.edit.redo");
    pub const EDIT_DELETE: ActionId = ActionId::new("audec.edit.delete");
    pub const EDIT_DUPLICATE: ActionId = ActionId::new("audec.edit.duplicate");
    pub const CLIP_SPLIT: ActionId = ActionId::new("audec.clip.split");
    pub const EDITOR_ARRANGEMENT: ActionId = ActionId::new("audec.editor.arrangement");
    pub const EDITOR_PIANO_ROLL: ActionId = ActionId::new("audec.editor.piano_roll");
    pub const EDITOR_DRUMS: ActionId = ActionId::new("audec.editor.drums");
    pub const EDITOR_AUTOMATION: ActionId = ActionId::new("audec.editor.automation");
    pub const EDITOR_MIXER: ActionId = ActionId::new("audec.editor.mixer");
    pub const EDITOR_ASSETS: ActionId = ActionId::new("audec.editor.assets");
    pub const EDITOR_SAMPLER: ActionId = ActionId::new("audec.editor.sampler");
    pub const EDITOR_READING_QUERY: ActionId = ActionId::new("audec.editor.reading_query");
    pub const SAMPLE_MAKE: ActionId = ActionId::new("audec.sample.make");
    pub const SAMPLE_SLICE_KIT: ActionId = ActionId::new("audec.sample.slice_kit");
    pub const SAMPLE_MAKE_BEAT: ActionId = ActionId::new("audec.sample.make_beat");
    pub const WORKSPACE_FOCUS: ActionId = ActionId::new("audec.workspace.focus");
    pub const WORKSPACE_ACTIVATE: ActionId = ActionId::new("audec.workspace.activate");
    pub const WORKSPACE_REOPEN: ActionId = ActionId::new("audec.workspace.reopen");
    pub const WORKSPACE_CLOSE: ActionId = ActionId::new("audec.workspace.close");
    pub const WORKSPACE_FLOAT_OR_DOCK: ActionId = ActionId::new("audec.workspace.float_or_dock");
    pub const WORKSPACE_NEXT_TAB: ActionId = ActionId::new("audec.workspace.next_tab");
    pub const WORKSPACE_PREVIOUS_TAB: ActionId = ActionId::new("audec.workspace.previous_tab");
    pub const WORKSPACE_NEXT_PANE: ActionId = ActionId::new("audec.workspace.next_pane");
    pub const WORKSPACE_PREVIOUS_PANE: ActionId = ActionId::new("audec.workspace.previous_pane");
    pub const PALETTE_OPEN: ActionId = ActionId::new("audec.palette.open");
}

/// Typed meaning behind the stable action ID. Application adapters lower this
/// enum into their existing project/workspace authorities instead of growing
/// another string match per presentation surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProductActionIntent {
    File(FileActionIntent),
    Edit(EditActionIntent),
    Transport(TransportActionIntent),
    Sample(SampleActionIntent),
    OpenPane(PaneOpenIntent),
    Workspace(WorkspaceActionIntent),
    OpenPalette,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileActionIntent {
    NewProject,
    OpenProject,
    OpenAudio,
    Save,
    SaveAs,
    OpenRecovery,
    ExportAudio,
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditActionIntent {
    Undo,
    Redo,
    Delete,
    Duplicate,
    SplitClip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportActionIntent {
    TogglePlayback,
    Stop,
    ToggleLoop,
    LoopFromSelection,
    ClearLoop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleActionIntent {
    MakeSample,
    SliceToKit,
    MakeBeat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneOpenIntent {
    Arrangement,
    PianoRoll,
    Drums,
    Automation,
    Mixer,
    Assets,
    Sampler,
    ReadingQuery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceActionIntent {
    Focus,
    Activate,
    Reopen,
    Close,
    FloatOrDock,
    NextTab,
    PreviousTab,
    NextPane,
    PreviousPane,
}

impl ProductActionIntent {
    pub fn from_action(action: ActionId) -> Option<Self> {
        use ids::*;
        Some(match action {
            FILE_NEW => Self::File(FileActionIntent::NewProject),
            FILE_OPEN => Self::File(FileActionIntent::OpenProject),
            FILE_OPEN_AUDIO => Self::File(FileActionIntent::OpenAudio),
            FILE_SAVE => Self::File(FileActionIntent::Save),
            FILE_SAVE_AS => Self::File(FileActionIntent::SaveAs),
            FILE_RECOVERY => Self::File(FileActionIntent::OpenRecovery),
            FILE_EXPORT => Self::File(FileActionIntent::ExportAudio),
            FILE_QUIT => Self::File(FileActionIntent::Quit),
            EDIT_UNDO => Self::Edit(EditActionIntent::Undo),
            EDIT_REDO => Self::Edit(EditActionIntent::Redo),
            EDIT_DELETE => Self::Edit(EditActionIntent::Delete),
            EDIT_DUPLICATE => Self::Edit(EditActionIntent::Duplicate),
            CLIP_SPLIT => Self::Edit(EditActionIntent::SplitClip),
            TRANSPORT_TOGGLE => Self::Transport(TransportActionIntent::TogglePlayback),
            TRANSPORT_STOP => Self::Transport(TransportActionIntent::Stop),
            LOOP_TOGGLE => Self::Transport(TransportActionIntent::ToggleLoop),
            LOOP_FROM_SELECTION => Self::Transport(TransportActionIntent::LoopFromSelection),
            LOOP_CLEAR => Self::Transport(TransportActionIntent::ClearLoop),
            SAMPLE_MAKE => Self::Sample(SampleActionIntent::MakeSample),
            SAMPLE_SLICE_KIT => Self::Sample(SampleActionIntent::SliceToKit),
            SAMPLE_MAKE_BEAT => Self::Sample(SampleActionIntent::MakeBeat),
            EDITOR_ARRANGEMENT => Self::OpenPane(PaneOpenIntent::Arrangement),
            EDITOR_PIANO_ROLL => Self::OpenPane(PaneOpenIntent::PianoRoll),
            EDITOR_DRUMS => Self::OpenPane(PaneOpenIntent::Drums),
            EDITOR_AUTOMATION => Self::OpenPane(PaneOpenIntent::Automation),
            EDITOR_MIXER => Self::OpenPane(PaneOpenIntent::Mixer),
            EDITOR_ASSETS => Self::OpenPane(PaneOpenIntent::Assets),
            EDITOR_SAMPLER => Self::OpenPane(PaneOpenIntent::Sampler),
            EDITOR_READING_QUERY => Self::OpenPane(PaneOpenIntent::ReadingQuery),
            WORKSPACE_FOCUS => Self::Workspace(WorkspaceActionIntent::Focus),
            WORKSPACE_ACTIVATE => Self::Workspace(WorkspaceActionIntent::Activate),
            WORKSPACE_REOPEN => Self::Workspace(WorkspaceActionIntent::Reopen),
            WORKSPACE_CLOSE => Self::Workspace(WorkspaceActionIntent::Close),
            WORKSPACE_FLOAT_OR_DOCK => Self::Workspace(WorkspaceActionIntent::FloatOrDock),
            WORKSPACE_NEXT_TAB => Self::Workspace(WorkspaceActionIntent::NextTab),
            WORKSPACE_PREVIOUS_TAB => Self::Workspace(WorkspaceActionIntent::PreviousTab),
            WORKSPACE_NEXT_PANE => Self::Workspace(WorkspaceActionIntent::NextPane),
            WORKSPACE_PREVIOUS_PANE => Self::Workspace(WorkspaceActionIntent::PreviousPane),
            PALETTE_OPEN => Self::OpenPalette,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionMenuEntry {
    Action(ActionId),
    Separator,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionMenuDescriptor {
    pub name: &'static str,
    pub entries: &'static [ActionMenuEntry],
}

const FILE_MENU: &[ActionMenuEntry] = &[
    ActionMenuEntry::Action(ids::FILE_NEW),
    ActionMenuEntry::Separator,
    ActionMenuEntry::Action(ids::FILE_OPEN),
    ActionMenuEntry::Action(ids::FILE_OPEN_AUDIO),
    ActionMenuEntry::Action(ids::FILE_RECOVERY),
    ActionMenuEntry::Separator,
    ActionMenuEntry::Action(ids::FILE_SAVE),
    ActionMenuEntry::Action(ids::FILE_SAVE_AS),
    ActionMenuEntry::Action(ids::FILE_EXPORT),
];
const EDIT_MENU: &[ActionMenuEntry] = &[
    ActionMenuEntry::Action(ids::EDIT_UNDO),
    ActionMenuEntry::Action(ids::EDIT_REDO),
    ActionMenuEntry::Separator,
    ActionMenuEntry::Action(ids::EDIT_DUPLICATE),
    ActionMenuEntry::Action(ids::EDIT_DELETE),
    ActionMenuEntry::Action(ids::CLIP_SPLIT),
];
const TRANSPORT_MENU: &[ActionMenuEntry] = &[
    ActionMenuEntry::Action(ids::TRANSPORT_TOGGLE),
    ActionMenuEntry::Action(ids::TRANSPORT_STOP),
    ActionMenuEntry::Separator,
    ActionMenuEntry::Action(ids::LOOP_FROM_SELECTION),
    ActionMenuEntry::Action(ids::LOOP_TOGGLE),
    ActionMenuEntry::Action(ids::LOOP_CLEAR),
];
const SAMPLE_MENU: &[ActionMenuEntry] = &[
    ActionMenuEntry::Action(ids::SAMPLE_MAKE),
    ActionMenuEntry::Action(ids::SAMPLE_SLICE_KIT),
    ActionMenuEntry::Action(ids::SAMPLE_MAKE_BEAT),
];
const WORKSPACE_MENU: &[ActionMenuEntry] = &[
    ActionMenuEntry::Action(ids::EDITOR_ARRANGEMENT),
    ActionMenuEntry::Action(ids::EDITOR_PIANO_ROLL),
    ActionMenuEntry::Action(ids::EDITOR_DRUMS),
    ActionMenuEntry::Action(ids::EDITOR_AUTOMATION),
    ActionMenuEntry::Action(ids::EDITOR_MIXER),
    ActionMenuEntry::Action(ids::EDITOR_ASSETS),
    ActionMenuEntry::Action(ids::EDITOR_SAMPLER),
    ActionMenuEntry::Action(ids::EDITOR_READING_QUERY),
    ActionMenuEntry::Separator,
    ActionMenuEntry::Action(ids::WORKSPACE_NEXT_PANE),
    ActionMenuEntry::Action(ids::WORKSPACE_PREVIOUS_PANE),
    ActionMenuEntry::Action(ids::WORKSPACE_FLOAT_OR_DOCK),
    ActionMenuEntry::Action(ids::WORKSPACE_CLOSE),
    ActionMenuEntry::Separator,
    ActionMenuEntry::Action(ids::PALETTE_OPEN),
];

pub const PRODUCT_MENU_LAYOUT: &[ActionMenuDescriptor] = &[
    ActionMenuDescriptor {
        name: "File",
        entries: FILE_MENU,
    },
    ActionMenuDescriptor {
        name: "Edit",
        entries: EDIT_MENU,
    },
    ActionMenuDescriptor {
        name: "Transport",
        entries: TRANSPORT_MENU,
    },
    ActionMenuDescriptor {
        name: "Sample",
        entries: SAMPLE_MENU,
    },
    ActionMenuDescriptor {
        name: "Workspace",
        entries: WORKSPACE_MENU,
    },
];

pub const PANE_CONTEXT_ACTIONS: &[ActionId] = &[ids::WORKSPACE_FLOAT_OR_DOCK, ids::WORKSPACE_CLOSE];

pub const SELECTION_CONTEXT_ACTIONS: &[ActionId] = &[
    ids::LOOP_FROM_SELECTION,
    ids::SAMPLE_MAKE,
    ids::SAMPLE_SLICE_KIT,
    ids::SAMPLE_MAKE_BEAT,
    ids::EDIT_DUPLICATE,
    ids::EDIT_DELETE,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EditorClass {
    Arrangement,
    Pattern,
    Sampler,
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
            Self::SamplerEditor => EditorClass::Sampler,
            Self::AutomationEditor => EditorClass::Automation,
            Self::Mixer => EditorClass::Mixer,
            Self::Analysis(_) => EditorClass::Analysis,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    pub const REQUIRES_ACTIVE_VIEW: Self = Self(1 << 5);

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
    /// Monotonic generation of the application state used to build this
    /// context. A projection made before a focus/selection/project change is
    /// rejected rather than dispatched against its old target.
    pub epoch: ContextEpoch,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContextEpoch(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegistryEpoch(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectionEpoch {
    pub registry: RegistryEpoch,
    pub context: ContextEpoch,
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

/// Serializable-shaped parameters carried beside an invocation. Values avoid
/// floating point so equality, journaling, remote transport, and test fixtures
/// do not inherit NaN or locale semantics. Musical values should use their
/// domain's integer frame/tick/fixed-point representation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ActionParameterValue {
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    Text(String),
    Choice(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionParameters(BTreeMap<String, ActionParameterValue>);

impl ActionParameters {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        name: impl Into<String>,
        value: ActionParameterValue,
    ) -> Option<ActionParameterValue> {
        self.0.insert(name.into(), value)
    }

    pub fn get(&self, name: &str) -> Option<&ActionParameterValue> {
        self.0.get(name)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &ActionParameterValue)> {
        self.0.iter().map(|(name, value)| (name.as_str(), value))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Epoch-bearing form used at the authority boundary. Existing callers may
/// continue to route [`ActionInvocation`] directly while adapters migrate;
/// menu/palette/context/AX projections should create this form and validate it
/// immediately before dispatch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionRequest {
    pub invocation: ActionInvocation,
    pub parameters: ActionParameters,
    pub projected_at: ProjectionEpoch,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyModifiers(u8);

impl KeyModifiers {
    pub const COMMAND: Self = Self(1 << 0);
    pub const CONTROL: Self = Self(1 << 1);
    pub const OPTION: Self = Self(1 << 2);
    pub const SHIFT: Self = Self(1 << 3);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Normalized, platform-neutral chord. Parsing accepts the strings already
/// used by GPUI (`cmd-shift-e`) and common long spellings, while display has
/// one stable order for menus, tests, and keymap persistence.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyChord {
    pub modifiers: KeyModifiers,
    pub key: String,
}

impl KeyChord {
    pub fn parse(text: &str) -> Result<Self, ShortcutParseError> {
        let text = text.trim();
        // `-` is both the chord separator and a perfectly ordinary zoom key.
        // Treat a terminal doubled separator as a literal minus key so the
        // canonical forms `-` and `cmd--` remain round-trippable.
        if text == "-" {
            return Ok(Self {
                modifiers: KeyModifiers::default(),
                key: "-".into(),
            });
        }
        let (modifier_text, literal_minus) = if let Some(prefix) = text.strip_suffix("--") {
            (prefix.strip_suffix('-').unwrap_or(prefix), true)
        } else {
            (text, false)
        };
        let mut modifiers = KeyModifiers::default();
        let mut key = literal_minus.then(|| "-".to_string());
        for raw_part in modifier_text.split('-') {
            let part = raw_part.trim().to_ascii_lowercase();
            if part.is_empty() {
                return Err(ShortcutParseError::EmptyPart);
            }
            let modifier = match part.as_str() {
                "cmd" | "command" | "meta" | "super" => Some(KeyModifiers::COMMAND),
                "ctrl" | "control" => Some(KeyModifiers::CONTROL),
                "opt" | "option" | "alt" => Some(KeyModifiers::OPTION),
                "shift" => Some(KeyModifiers::SHIFT),
                _ => None,
            };
            if let Some(modifier) = modifier {
                if modifiers.contains(modifier) {
                    return Err(ShortcutParseError::DuplicateModifier(part));
                }
                modifiers = modifiers.with(modifier);
            } else if key.replace(part).is_some() {
                return Err(ShortcutParseError::MultipleKeys);
            }
        }
        let key = key.ok_or(ShortcutParseError::MissingKey)?;
        Ok(Self { modifiers, key })
    }
}

impl fmt::Display for KeyChord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (modifier, name) in [
            (KeyModifiers::COMMAND, "cmd"),
            (KeyModifiers::CONTROL, "ctrl"),
            (KeyModifiers::OPTION, "option"),
            (KeyModifiers::SHIFT, "shift"),
        ] {
            if self.modifiers.contains(modifier) {
                write!(formatter, "{name}-")?;
            }
        }
        formatter.write_str(&self.key)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShortcutParseError {
    EmptyPart,
    DuplicateModifier(String),
    MissingKey,
    MultipleKeys,
}

impl fmt::Display for ShortcutParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPart => formatter.write_str("shortcut contains an empty part"),
            Self::DuplicateModifier(name) => {
                write!(formatter, "shortcut repeats the {name} modifier")
            }
            Self::MissingKey => formatter.write_str("shortcut has no key"),
            Self::MultipleKeys => formatter.write_str("shortcut contains more than one key"),
        }
    }
}

impl Error for ShortcutParseError {}

/// User bindings replace, rather than append to, defaults. An empty vector is
/// an explicit unbinding. String keys allow a future preferences codec to
/// preserve entries for plug-ins that are unavailable in the current launch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserKeymap {
    overrides: BTreeMap<String, Vec<KeyChord>>,
}

impl UserKeymap {
    pub fn set(&mut self, action: impl Into<String>, chords: Vec<KeyChord>) {
        self.overrides.insert(action.into(), chords);
    }

    pub fn clear_override(&mut self, action: &str) {
        self.overrides.remove(action);
    }

    pub fn override_for(&self, action: ActionId) -> Option<&[KeyChord]> {
        self.overrides.get(action.0).map(Vec::as_slice)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingSource {
    Default,
    User,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedBinding {
    pub chord: KeyChord,
    pub source: BindingSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedAction {
    pub descriptor: ActionDescriptor,
    pub state: ActionState,
    pub bindings: Vec<ProjectedBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionProjectionSnapshot {
    pub epoch: ProjectionEpoch,
    pub active_view: Option<WorkspaceViewId>,
    pub target: Option<EditorTarget>,
    entries: Vec<ProjectedAction>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionSurface {
    Menu,
    Palette,
    ContextMenu,
    Accessibility,
}

/// Presentation-only DTO. GPUI, Guise, native-menu, and AccessKit adapters
/// should render this object and return its stable action ID, never capture a
/// view method in parallel with the registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionSurfaceItem {
    pub action: ActionId,
    pub label: &'static str,
    pub category: ActionCategory,
    pub scope: ActionScope,
    pub enabled: bool,
    pub checked: bool,
    pub disabled_reason: Option<&'static str>,
    pub shortcuts: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuSection {
    pub category: ActionCategory,
    pub items: Vec<ActionSurfaceItem>,
}

impl ActionProjectionSnapshot {
    pub fn entries(&self) -> impl ExactSizeIterator<Item = &ProjectedAction> {
        self.entries.iter()
    }

    pub fn get(&self, id: ActionId) -> Option<&ProjectedAction> {
        self.entries
            .binary_search_by_key(&id, |entry| entry.descriptor.id)
            .ok()
            .map(|index| &self.entries[index])
    }

    /// Native and in-window menus receive identical section contents.
    pub fn menu_sections(&self) -> Vec<MenuSection> {
        let mut sections = BTreeMap::<ActionCategory, Vec<ActionSurfaceItem>>::new();
        for entry in &self.entries {
            sections
                .entry(entry.descriptor.category)
                .or_default()
                .push(surface_item(entry));
        }
        sections
            .into_iter()
            .map(|(category, items)| MenuSection { category, items })
            .collect()
    }

    /// Palette matching is intentionally modest and deterministic: every
    /// whitespace-separated token must be a case-insensitive substring of the
    /// label or stable ID. Presentation may add richer ranking later without
    /// changing action identity or dispatch.
    pub fn palette(&self, query: &str) -> Vec<ActionSurfaceItem> {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|term| term.to_ascii_lowercase())
            .collect();
        let mut matches: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| {
                let searchable = format!(
                    "{} {}",
                    entry.descriptor.label.to_ascii_lowercase(),
                    entry.descriptor.id.0
                );
                terms.iter().all(|term| searchable.contains(term))
            })
            .map(surface_item)
            .collect();
        matches.sort_by_key(|entry| (entry.label.to_ascii_lowercase(), entry.action));
        matches
    }

    /// Context-menu adapters name the actions appropriate to the clicked
    /// object. The registry supplies current labels/state/shortcuts and keeps
    /// the caller's requested ordering; unknown IDs are simply omitted so a
    /// plug-in disappearing cannot break the whole menu.
    pub fn context_menu(&self, actions: &[ActionId]) -> Vec<ActionSurfaceItem> {
        actions
            .iter()
            .filter_map(|id| self.get(*id))
            .map(surface_item)
            .collect()
    }

    pub fn accessibility_item(&self, action: ActionId) -> Option<ActionSurfaceItem> {
        self.get(action).map(surface_item)
    }

    pub fn request(
        &self,
        action: ActionId,
        origin: InvocationOrigin,
        modifiers: InvocationModifiers,
        parameters: ActionParameters,
    ) -> Result<ActionRequest, ActionDispatchError> {
        self.request_for_target(
            action,
            origin,
            modifiers,
            parameters,
            self.active_view,
            self.target.clone(),
        )
    }

    /// Build an epoch-bearing request for a semantic object. The authority
    /// boundary still rejects the request if this view/target is no longer the
    /// active context; allowing the adapter to name the intended target keeps
    /// context-menu and accessibility callbacks from silently falling back to
    /// whichever object happens to be focused later.
    pub fn request_for_target(
        &self,
        action: ActionId,
        origin: InvocationOrigin,
        modifiers: InvocationModifiers,
        parameters: ActionParameters,
        view: Option<WorkspaceViewId>,
        target: Option<EditorTarget>,
    ) -> Result<ActionRequest, ActionDispatchError> {
        let projected = self
            .get(action)
            .ok_or(ActionDispatchError::UnknownAction(action))?;
        if !projected.state.enabled {
            return Err(ActionDispatchError::Disabled {
                action,
                reason: projected
                    .state
                    .disabled_reason
                    .unwrap_or("Action is unavailable"),
            });
        }
        Ok(ActionRequest {
            invocation: ActionInvocation {
                action,
                origin,
                view,
                target,
                modifiers,
            },
            parameters,
            projected_at: self.epoch,
        })
    }

    /// Resolve a key against this exact immutable projection. Native action
    /// parity receipts use this instead of consulting a newly-built context.
    pub fn resolve_projected_shortcut(&self, chord: &KeyChord) -> ShortcutResolution {
        resolve_snapshot_shortcut(&self.entries, chord)
    }
}

fn surface_item(entry: &ProjectedAction) -> ActionSurfaceItem {
    ActionSurfaceItem {
        action: entry.descriptor.id,
        label: entry.descriptor.label,
        category: entry.descriptor.category,
        scope: entry.descriptor.scope,
        enabled: entry.state.enabled,
        checked: entry.state.checked,
        disabled_reason: entry.state.disabled_reason,
        shortcuts: entry
            .bindings
            .iter()
            .map(|binding| binding.chord.to_string())
            .collect(),
    }
}

fn resolve_snapshot_shortcut(entries: &[ProjectedAction], chord: &KeyChord) -> ShortcutResolution {
    let mut candidates: Vec<_> = entries
        .iter()
        .filter_map(|entry| {
            entry
                .bindings
                .iter()
                .find(|binding| &binding.chord == chord)
                .map(|binding| ShortcutCandidate {
                    action: entry.descriptor.id,
                    state: entry.state.clone(),
                    source: binding.source,
                    scope: entry.descriptor.scope,
                })
        })
        .collect();
    if candidates.is_empty() {
        return ShortcutResolution::Unbound;
    }
    candidates.sort_by_key(|candidate| candidate.action);
    let enabled: Vec<_> = candidates
        .iter()
        .filter(|candidate| candidate.state.enabled)
        .cloned()
        .collect();
    if enabled.is_empty() {
        return ShortcutResolution::Disabled(candidates);
    }
    let best_rank = enabled
        .iter()
        .map(shortcut_candidate_rank)
        .max()
        .expect("enabled is nonempty");
    let winners: Vec<_> = enabled
        .into_iter()
        .filter(|candidate| shortcut_candidate_rank(candidate) == best_rank)
        .collect();
    if winners.len() == 1 {
        ShortcutResolution::Invoke(winners[0].action)
    } else {
        ShortcutResolution::Ambiguous(winners)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortcutCandidate {
    pub action: ActionId,
    pub state: ActionState,
    pub source: BindingSource,
    pub scope: ActionScope,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShortcutResolution {
    Unbound,
    Invoke(ActionId),
    Disabled(Vec<ShortcutCandidate>),
    /// Equally specific enabled actions are never selected arbitrarily. The
    /// keymap editor can present this ordered list and ask the user to resolve
    /// it; a user override naturally outranks a default binding.
    Ambiguous(Vec<ShortcutCandidate>),
}

#[derive(Clone, Debug)]
pub struct ActionRegistry {
    descriptors: BTreeMap<ActionId, ActionDescriptor>,
    epoch: RegistryEpoch,
}

impl Default for ActionRegistry {
    fn default() -> Self {
        Self {
            descriptors: BTreeMap::new(),
            epoch: RegistryEpoch(1),
        }
    }
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

    /// Complete built-in product catalog used by native menus, the command
    /// palette, pane context menus, accessibility and external control.
    ///
    /// [`audec_defaults`](Self::audec_defaults) remains the compact historical
    /// catalog during the host migration. New presentation adapters should use
    /// this constructor so discoverability does not depend on `ui.rs` growing
    /// another private dispatch table.
    pub fn audec_product_defaults() -> Self {
        let mut registry = Self::audec_defaults();
        for descriptor in product_builtins() {
            registry
                .register(descriptor)
                .expect("built-in product action IDs are unique");
        }
        registry
    }

    pub const fn epoch(&self) -> RegistryEpoch {
        self.epoch
    }

    pub fn register(&mut self, descriptor: ActionDescriptor) -> Result<(), ActionRegistryError> {
        validate_action_id(descriptor.id)?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(ActionRegistryError::DuplicateId(descriptor.id));
        }
        let mut seen_shortcuts = BTreeSet::new();
        for shortcut in descriptor.default_keys {
            let chord = KeyChord::parse(shortcut).map_err(|source| {
                ActionRegistryError::InvalidShortcut {
                    action: descriptor.id,
                    shortcut,
                    source,
                }
            })?;
            if !seen_shortcuts.insert(chord) {
                return Err(ActionRegistryError::DuplicateShortcut {
                    action: descriptor.id,
                    shortcut,
                });
            }
        }
        self.descriptors.insert(descriptor.id, descriptor);
        self.bump_epoch();
        Ok(())
    }

    pub fn unregister(&mut self, id: ActionId) -> Option<ActionDescriptor> {
        let removed = self.descriptors.remove(&id);
        if removed.is_some() {
            self.bump_epoch();
        }
        removed
    }

    pub fn get(&self, id: ActionId) -> Option<&ActionDescriptor> {
        self.descriptors.get(&id)
    }

    pub fn get_str(&self, id: &str) -> Option<&ActionDescriptor> {
        self.descriptors.values().find(|entry| entry.id.0 == id)
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
        } else if descriptor.flags.contains(ActionFlags::REQUIRES_ACTIVE_VIEW)
            && context.active_view.is_none()
        {
            ActionState::disabled("No workspace pane is active")
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
                "audec.loop.clear" if !context.loop_enabled => {
                    ActionState::disabled("No active loop to clear")
                }
                "audec.transport.toggle" => ActionState {
                    checked: context.transport_playing,
                    ..state
                },
                _ => state,
            };
        }
        Some(state)
    }

    /// Freeze action metadata, enablement, target, and active keymap into one
    /// immutable view. Every presentation surface for a frame should consume
    /// the same snapshot so a disabled menu item cannot disagree with the
    /// command palette or accessibility tree.
    pub fn project(
        &self,
        context: &ActionContext,
        keymap: &UserKeymap,
    ) -> ActionProjectionSnapshot {
        let entries = self
            .descriptors
            .values()
            .map(|descriptor| {
                let (source, chords) = match keymap.override_for(descriptor.id) {
                    Some(chords) => (BindingSource::User, chords.to_vec()),
                    None => (
                        BindingSource::Default,
                        descriptor
                            .default_keys
                            .iter()
                            .map(|shortcut| {
                                KeyChord::parse(shortcut)
                                    .expect("registered shortcuts were validated")
                            })
                            .collect(),
                    ),
                };
                ProjectedAction {
                    descriptor: descriptor.clone(),
                    state: self
                        .resolve(descriptor.id, context)
                        .expect("descriptor came from this registry"),
                    bindings: chords
                        .into_iter()
                        .map(|chord| ProjectedBinding { chord, source })
                        .collect(),
                }
            })
            .collect();
        ActionProjectionSnapshot {
            epoch: ProjectionEpoch {
                registry: self.epoch,
                context: context.epoch,
            },
            active_view: context.active_view,
            target: context.target.clone(),
            entries,
        }
    }

    /// Resolve a normalized chord using user binding precedence and the most
    /// specific currently enabled scope. Same-rank conflicts remain explicit;
    /// adapters must not pick whichever handler registered first.
    pub fn resolve_shortcut(
        &self,
        chord: &KeyChord,
        context: &ActionContext,
        keymap: &UserKeymap,
    ) -> ShortcutResolution {
        let snapshot = self.project(context, keymap);
        snapshot.resolve_projected_shortcut(chord)
    }

    /// Recheck an epoch-bearing request at the authority boundary. This is
    /// intentionally separate from projection/request creation because native
    /// menu callbacks and accessibility actions may arrive many frames later.
    pub fn validate_request(
        &self,
        request: &ActionRequest,
        context: &ActionContext,
    ) -> Result<ActionInvocation, ActionDispatchError> {
        if request.projected_at.registry != self.epoch {
            return Err(ActionDispatchError::StaleRegistry {
                projected: request.projected_at.registry,
                current: self.epoch,
            });
        }
        if request.projected_at.context != context.epoch {
            return Err(ActionDispatchError::StaleContext {
                projected: request.projected_at.context,
                current: context.epoch,
            });
        }
        let action = request.invocation.action;
        let state = self
            .resolve(action, context)
            .ok_or(ActionDispatchError::UnknownAction(action))?;
        if !state.enabled {
            return Err(ActionDispatchError::Disabled {
                action,
                reason: state.disabled_reason.unwrap_or("Action is unavailable"),
            });
        }
        if request.invocation.view != context.active_view
            || request.invocation.target != context.target
        {
            return Err(ActionDispatchError::ContextTargetChanged { action });
        }
        Ok(request.invocation.clone())
    }

    fn bump_epoch(&mut self) {
        self.epoch.0 = self.epoch.0.wrapping_add(1).max(1);
    }
}

fn shortcut_candidate_rank(candidate: &ShortcutCandidate) -> (u8, u8) {
    let source = match candidate.source {
        BindingSource::Default => 0,
        BindingSource::User => 1,
    };
    let scope = match candidate.scope {
        ActionScope::Application => 0,
        ActionScope::Project => 1,
        ActionScope::Workspace => 2,
        ActionScope::Editor(_) => 3,
    };
    (source, scope)
}

fn validate_action_id(id: ActionId) -> Result<(), ActionRegistryError> {
    let raw = id.0;
    if raw.trim().is_empty() {
        return Err(ActionRegistryError::EmptyId);
    }
    let valid = raw.split('.').count() >= 3
        && !raw.starts_with('.')
        && !raw.ends_with('.')
        && raw.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        });
    if !valid {
        return Err(ActionRegistryError::InvalidId(id));
    }
    Ok(())
}

const PROJECT: ActionFlags = ActionFlags::REQUIRES_PROJECT;
const PROJECT_SELECTION: ActionFlags =
    ActionFlags::REQUIRES_PROJECT.union(ActionFlags::REQUIRES_SELECTION);
const TEXT_SAFE_PROJECT: ActionFlags =
    ActionFlags::REQUIRES_PROJECT.union(ActionFlags::ALLOW_IN_TEXT_INPUT);

fn builtins() -> Vec<ActionDescriptor> {
    vec![
        action(
            ids::FILE_OPEN,
            "Open…",
            ActionCategory::File,
            ActionScope::Application,
            &["cmd-o"],
            ActionFlags::ALLOW_IN_TEXT_INPUT.union(ActionFlags::ALLOW_IN_MODAL),
        ),
        action(
            ids::FILE_SAVE,
            "Save",
            ActionCategory::File,
            ActionScope::Project,
            &["cmd-s"],
            TEXT_SAFE_PROJECT,
        ),
        action(
            ids::FILE_EXPORT,
            "Export Audio…",
            ActionCategory::File,
            ActionScope::Project,
            &["cmd-shift-e"],
            PROJECT,
        ),
        action(
            ids::TRANSPORT_TOGGLE,
            "Play / Pause",
            ActionCategory::Transport,
            ActionScope::Project,
            &["space"],
            PROJECT.union(ActionFlags::CHECKABLE),
        ),
        action(
            ids::TRANSPORT_STOP,
            "Stop",
            ActionCategory::Transport,
            ActionScope::Project,
            &["shift-space"],
            PROJECT,
        ),
        action(
            ids::LOOP_TOGGLE,
            "Loop",
            ActionCategory::Transport,
            ActionScope::Project,
            &["l"],
            PROJECT.union(ActionFlags::CHECKABLE),
        ),
        action(
            ids::EDIT_UNDO,
            "Undo",
            ActionCategory::Edit,
            ActionScope::Project,
            &["cmd-z"],
            TEXT_SAFE_PROJECT,
        ),
        action(
            ids::EDIT_REDO,
            "Redo",
            ActionCategory::Edit,
            ActionScope::Project,
            &["cmd-shift-z"],
            TEXT_SAFE_PROJECT,
        ),
        action(
            ids::EDIT_DELETE,
            "Delete",
            ActionCategory::Edit,
            ActionScope::Project,
            &["delete", "backspace"],
            PROJECT_SELECTION,
        ),
        action(
            ids::EDIT_DUPLICATE,
            "Duplicate",
            ActionCategory::Edit,
            ActionScope::Project,
            &["cmd-d"],
            PROJECT_SELECTION,
        ),
        action(
            ids::CLIP_SPLIT,
            "Split Clip",
            ActionCategory::Clip,
            ActionScope::Editor(EditorClass::Arrangement),
            &["cmd-e"],
            PROJECT_SELECTION,
        ),
        action(
            ids::EDITOR_ARRANGEMENT,
            "Arrangement",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-1"],
            PROJECT,
        ),
        action(
            ids::EDITOR_PIANO_ROLL,
            "Piano Roll",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-2"],
            PROJECT,
        ),
        action(
            ids::EDITOR_DRUMS,
            "Drum Editor",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-3"],
            PROJECT,
        ),
        action(
            ids::EDITOR_AUTOMATION,
            "Automation",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-4"],
            PROJECT,
        ),
        action(
            ids::EDITOR_MIXER,
            "Mixer",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-5"],
            PROJECT,
        ),
        action(
            ids::PALETTE_OPEN,
            "Command Palette",
            ActionCategory::Workspace,
            ActionScope::Application,
            &["cmd-shift-p"],
            ActionFlags::ALLOW_IN_TEXT_INPUT,
        ),
    ]
}

fn product_builtins() -> Vec<ActionDescriptor> {
    let text_safe = ActionFlags::ALLOW_IN_TEXT_INPUT;
    let text_and_modal_safe = text_safe.union(ActionFlags::ALLOW_IN_MODAL);
    let project_and_text_safe = PROJECT.union(text_safe);
    let active_project = PROJECT.union(ActionFlags::REQUIRES_ACTIVE_VIEW);
    vec![
        action(
            ids::FILE_NEW,
            "New Project",
            ActionCategory::File,
            ActionScope::Application,
            &["cmd-n"],
            text_safe,
        ),
        action(
            ids::FILE_OPEN_AUDIO,
            "Open Audio…",
            ActionCategory::File,
            ActionScope::Application,
            &["cmd-shift-o"],
            text_safe,
        ),
        action(
            ids::FILE_SAVE_AS,
            "Save As…",
            ActionCategory::File,
            ActionScope::Project,
            &["cmd-shift-s"],
            project_and_text_safe,
        ),
        action(
            ids::FILE_RECOVERY,
            "Open Recovery…",
            ActionCategory::File,
            ActionScope::Application,
            &["cmd-option-s"],
            text_safe,
        ),
        action(
            ids::FILE_QUIT,
            "Quit Audec",
            ActionCategory::File,
            ActionScope::Application,
            &["cmd-q"],
            text_and_modal_safe,
        ),
        action(
            ids::LOOP_FROM_SELECTION,
            "Loop Selection",
            ActionCategory::Transport,
            ActionScope::Project,
            &["cmd-l"],
            PROJECT_SELECTION,
        ),
        action(
            ids::LOOP_CLEAR,
            "Clear Loop",
            ActionCategory::Transport,
            ActionScope::Project,
            &[],
            PROJECT,
        ),
        action(
            ids::SAMPLE_MAKE,
            "Make Sample from Active Span",
            ActionCategory::Clip,
            ActionScope::Project,
            &["s"],
            PROJECT_SELECTION,
        ),
        action(
            ids::SAMPLE_SLICE_KIT,
            "Slice Active Span to Kit",
            ActionCategory::Clip,
            ActionScope::Project,
            &["shift-s"],
            PROJECT_SELECTION,
        ),
        action(
            ids::SAMPLE_MAKE_BEAT,
            "Make Beat from Active Span",
            ActionCategory::Pattern,
            ActionScope::Project,
            &["b"],
            PROJECT_SELECTION,
        ),
        action(
            ids::EDITOR_ASSETS,
            "Media Pool",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-b"],
            PROJECT,
        ),
        action(
            ids::EDITOR_SAMPLER,
            "Sampler",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-shift-b"],
            PROJECT,
        ),
        action(
            ids::EDITOR_READING_QUERY,
            "Reading Query",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-shift-r"],
            PROJECT,
        ),
        action(
            ids::WORKSPACE_FOCUS,
            "Focus Pane",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &[],
            PROJECT,
        ),
        action(
            ids::WORKSPACE_ACTIVATE,
            "Activate Pane",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &[],
            PROJECT,
        ),
        action(
            ids::WORKSPACE_REOPEN,
            "Reopen Pane",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &[],
            PROJECT,
        ),
        action(
            ids::WORKSPACE_CLOSE,
            "Close Pane",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-shift-w"],
            active_project,
        ),
        action(
            ids::WORKSPACE_FLOAT_OR_DOCK,
            "Float or Dock Pane",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["cmd-option-w"],
            active_project,
        ),
        action(
            ids::WORKSPACE_NEXT_TAB,
            "Next Tab",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &[],
            active_project,
        ),
        action(
            ids::WORKSPACE_PREVIOUS_TAB,
            "Previous Tab",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &[],
            active_project,
        ),
        action(
            ids::WORKSPACE_NEXT_PANE,
            "Next Pane",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["ctrl-tab"],
            active_project,
        ),
        action(
            ids::WORKSPACE_PREVIOUS_PANE,
            "Previous Pane",
            ActionCategory::Workspace,
            ActionScope::Workspace,
            &["ctrl-shift-tab"],
            active_project,
        ),
    ]
}

const fn action(
    id: ActionId,
    label: &'static str,
    category: ActionCategory,
    scope: ActionScope,
    default_keys: &'static [&'static str],
    flags: ActionFlags,
) -> ActionDescriptor {
    ActionDescriptor {
        id,
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
    InvalidId(ActionId),
    DuplicateId(ActionId),
    InvalidShortcut {
        action: ActionId,
        shortcut: &'static str,
        source: ShortcutParseError,
    },
    DuplicateShortcut {
        action: ActionId,
        shortcut: &'static str,
    },
}

impl fmt::Display for ActionRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyId => formatter.write_str("action ID must not be empty"),
            Self::InvalidId(id) => write!(
                formatter,
                "action ID {} must be a dotted lowercase identifier",
                id.0
            ),
            Self::DuplicateId(id) => write!(formatter, "action {} is registered twice", id.0),
            Self::InvalidShortcut {
                action,
                shortcut,
                source,
            } => write!(
                formatter,
                "action {} has invalid shortcut {shortcut}: {source}",
                action.0
            ),
            Self::DuplicateShortcut { action, shortcut } => {
                write!(formatter, "action {} repeats shortcut {shortcut}", action.0)
            }
        }
    }
}

impl Error for ActionRegistryError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionDispatchError {
    UnknownAction(ActionId),
    Disabled {
        action: ActionId,
        reason: &'static str,
    },
    StaleRegistry {
        projected: RegistryEpoch,
        current: RegistryEpoch,
    },
    StaleContext {
        projected: ContextEpoch,
        current: ContextEpoch,
    },
    ContextTargetChanged {
        action: ActionId,
    },
}

impl fmt::Display for ActionDispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownAction(action) => write!(formatter, "unknown action {}", action.0),
            Self::Disabled { action, reason } => {
                write!(formatter, "action {} is disabled: {reason}", action.0)
            }
            Self::StaleRegistry { projected, current } => write!(
                formatter,
                "action registry changed after projection ({} -> {})",
                projected.0, current.0
            ),
            Self::StaleContext { projected, current } => write!(
                formatter,
                "action context changed after projection ({} -> {})",
                projected.0, current.0
            ),
            Self::ContextTargetChanged { action } => write!(
                formatter,
                "action {} targets a view or object that is no longer active",
                action.0
            ),
        }
    }
}

impl Error for ActionDispatchError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_action(
        id: &'static str,
        label: &'static str,
        scope: ActionScope,
        keys: &'static [&'static str],
    ) -> ActionDescriptor {
        ActionDescriptor {
            id: ActionId(id),
            label,
            category: ActionCategory::Edit,
            scope,
            default_keys: keys,
            flags: ActionFlags::NONE,
        }
    }

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
    fn product_catalog_makes_file_transport_sample_and_workspace_actions_discoverable() {
        let registry = ActionRegistry::audec_product_defaults();
        let critical = [
            ids::FILE_NEW,
            ids::FILE_OPEN,
            ids::FILE_OPEN_AUDIO,
            ids::FILE_SAVE,
            ids::FILE_SAVE_AS,
            ids::FILE_RECOVERY,
            ids::FILE_EXPORT,
            ids::FILE_QUIT,
            ids::EDIT_UNDO,
            ids::EDIT_REDO,
            ids::LOOP_FROM_SELECTION,
            ids::LOOP_TOGGLE,
            ids::LOOP_CLEAR,
            ids::SAMPLE_MAKE,
            ids::SAMPLE_SLICE_KIT,
            ids::SAMPLE_MAKE_BEAT,
            ids::EDITOR_ARRANGEMENT,
            ids::EDITOR_ASSETS,
            ids::EDITOR_SAMPLER,
            ids::WORKSPACE_CLOSE,
            ids::WORKSPACE_FLOAT_OR_DOCK,
            ids::WORKSPACE_NEXT_PANE,
            ids::WORKSPACE_PREVIOUS_PANE,
        ];
        assert_eq!(registry.descriptors().count(), 39);
        for action in critical {
            assert!(
                registry.get(action).is_some(),
                "{} is absent from the product action catalog",
                action.0
            );
        }
    }

    #[test]
    fn startup_file_actions_are_reachable_without_inventing_a_project() {
        let registry = ActionRegistry::audec_product_defaults();
        let context = ActionContext::default();
        for action in [
            ids::FILE_NEW,
            ids::FILE_OPEN,
            ids::FILE_OPEN_AUDIO,
            ids::FILE_RECOVERY,
            ids::FILE_QUIT,
        ] {
            assert!(
                registry.resolve(action, &context).unwrap().enabled,
                "{}",
                action.0
            );
        }
        for action in [
            ids::FILE_SAVE,
            ids::FILE_SAVE_AS,
            ids::FILE_EXPORT,
            ids::EDIT_UNDO,
            ids::SAMPLE_MAKE,
        ] {
            let state = registry.resolve(action, &context).unwrap();
            assert!(!state.enabled, "{}", action.0);
            assert!(state.disabled_reason.is_some(), "{}", action.0);
        }
    }

    #[test]
    fn active_project_projection_exposes_context_actions_and_real_loop_state() {
        let registry = ActionRegistry::audec_product_defaults();
        let context = ActionContext {
            has_project: true,
            has_selection: true,
            active_view: Some(WorkspaceViewId(7)),
            active_kind: Some(WorkspaceItemKind::Arrangement),
            target: Some(EditorTarget::Arrangement),
            can_undo: true,
            can_redo: true,
            loop_enabled: true,
            ..ActionContext::default()
        };
        let snapshot = registry.project(&context, &UserKeymap::default());
        for action in PANE_CONTEXT_ACTIONS.iter().chain(SELECTION_CONTEXT_ACTIONS) {
            assert!(snapshot.get(*action).unwrap().state.enabled, "{}", action.0);
        }
        assert!(snapshot.get(ids::LOOP_TOGGLE).unwrap().state.checked);
        assert!(snapshot.get(ids::LOOP_CLEAR).unwrap().state.enabled);

        let context_ids: BTreeSet<_> = PANE_CONTEXT_ACTIONS
            .iter()
            .chain(SELECTION_CONTEXT_ACTIONS)
            .copied()
            .collect();
        let menu_ids: BTreeSet<_> = PRODUCT_MENU_LAYOUT
            .iter()
            .flat_map(|menu| menu.entries)
            .filter_map(|entry| match entry {
                ActionMenuEntry::Action(action) => Some(*action),
                ActionMenuEntry::Separator => None,
            })
            .collect();
        assert!(context_ids.iter().all(|action| menu_ids.contains(action)));
    }

    #[test]
    fn product_intents_are_one_typed_dispatch_vocabulary() {
        assert_eq!(
            ProductActionIntent::from_action(ids::FILE_SAVE_AS),
            Some(ProductActionIntent::File(FileActionIntent::SaveAs))
        );
        assert_eq!(
            ProductActionIntent::from_action(ids::LOOP_CLEAR),
            Some(ProductActionIntent::Transport(
                TransportActionIntent::ClearLoop
            ))
        );
        assert_eq!(
            ProductActionIntent::from_action(ids::SAMPLE_SLICE_KIT),
            Some(ProductActionIntent::Sample(SampleActionIntent::SliceToKit))
        );
        assert_eq!(
            ProductActionIntent::from_action(ids::WORKSPACE_FLOAT_OR_DOCK),
            Some(ProductActionIntent::Workspace(
                WorkspaceActionIntent::FloatOrDock
            ))
        );
        assert_eq!(
            ProductActionIntent::from_action(ActionId("audec.unknown.action")),
            None
        );
    }

    #[test]
    fn every_registered_product_action_has_a_typed_application_intent() {
        let unmapped = ActionRegistry::audec_product_defaults()
            .descriptors()
            .filter_map(|descriptor| {
                ProductActionIntent::from_action(descriptor.id)
                    .is_none()
                    .then_some(descriptor.id.0)
            })
            .collect::<Vec<_>>();
        assert!(
            unmapped.is_empty(),
            "registered actions without typed application intents: {unmapped:?}"
        );

        for menu in PRODUCT_MENU_LAYOUT {
            for entry in menu.entries {
                let ActionMenuEntry::Action(action) = entry else {
                    continue;
                };
                assert!(
                    ProductActionIntent::from_action(*action).is_some(),
                    "{} menu exposes unmapped action {}",
                    menu.name,
                    action.0
                );
            }
        }
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

    #[test]
    fn shortcut_parser_normalizes_aliases_and_modifier_order() {
        assert_eq!(
            KeyChord::parse("Shift-Alt-CMD-E").unwrap().to_string(),
            "cmd-option-shift-e"
        );
        assert_eq!(
            KeyChord::parse("control-control-z"),
            Err(ShortcutParseError::DuplicateModifier("control".into()))
        );
        assert_eq!(
            KeyChord::parse("cmd-a-b"),
            Err(ShortcutParseError::MultipleKeys)
        );
        assert_eq!(KeyChord::parse("-").unwrap().to_string(), "-");
        assert_eq!(KeyChord::parse("cmd--").unwrap().to_string(), "cmd--");
    }

    #[test]
    fn projection_is_one_authoritative_snapshot_for_every_surface() {
        let registry = ActionRegistry::audec_defaults();
        let context = ActionContext {
            epoch: ContextEpoch(41),
            has_project: true,
            loop_enabled: true,
            ..ActionContext::default()
        };
        let snapshot = registry.project(&context, &UserKeymap::default());
        assert_eq!(snapshot.epoch.context, ContextEpoch(41));

        let menu_loop = snapshot
            .menu_sections()
            .into_iter()
            .flat_map(|section| section.items)
            .find(|item| item.action == ids::LOOP_TOGGLE)
            .unwrap();
        let palette_loop = snapshot
            .palette("loop")
            .into_iter()
            .find(|item| item.action == ids::LOOP_TOGGLE)
            .unwrap();
        let context_loop = snapshot.context_menu(&[ids::LOOP_TOGGLE]).remove(0);
        let ax_loop = snapshot.accessibility_item(ids::LOOP_TOGGLE).unwrap();
        assert_eq!(menu_loop, palette_loop);
        assert_eq!(menu_loop, context_loop);
        assert_eq!(menu_loop, ax_loop);
        assert!(menu_loop.checked);
        assert_eq!(menu_loop.shortcuts, ["l"]);
    }

    #[test]
    fn semantic_target_request_retains_target_and_projection_epoch() {
        let registry = ActionRegistry::audec_defaults();
        let view = WorkspaceViewId(71);
        let context = ActionContext {
            epoch: ContextEpoch(9),
            has_project: true,
            has_selection: true,
            active_view: Some(view),
            target: Some(EditorTarget::Arrangement),
            ..ActionContext::default()
        };
        let snapshot = registry.project(&context, &UserKeymap::default());
        let request = snapshot
            .request_for_target(
                ids::EDIT_DELETE,
                InvocationOrigin::Accessibility,
                InvocationModifiers::default(),
                ActionParameters::default(),
                Some(view),
                Some(EditorTarget::Arrangement),
            )
            .unwrap();
        assert_eq!(request.projected_at, snapshot.epoch);
        assert_eq!(request.invocation.view, Some(view));
        assert_eq!(request.invocation.target, Some(EditorTarget::Arrangement));
        assert_eq!(
            snapshot.resolve_projected_shortcut(&KeyChord::parse("delete").unwrap()),
            ShortcutResolution::Invoke(ids::EDIT_DELETE)
        );
    }

    #[test]
    fn focused_editor_binding_wins_over_project_binding() {
        let mut registry = ActionRegistry::new();
        registry
            .register(test_action(
                "test.project.do",
                "Project action",
                ActionScope::Project,
                &["x"],
            ))
            .unwrap();
        registry
            .register(test_action(
                "test.editor.do",
                "Editor action",
                ActionScope::Editor(EditorClass::Arrangement),
                &["x"],
            ))
            .unwrap();
        let context = ActionContext {
            has_project: true,
            active_kind: Some(WorkspaceItemKind::Arrangement),
            ..ActionContext::default()
        };
        assert_eq!(
            registry.resolve_shortcut(
                &KeyChord::parse("x").unwrap(),
                &context,
                &UserKeymap::default()
            ),
            ShortcutResolution::Invoke(ActionId("test.editor.do"))
        );
    }

    #[test]
    fn equal_specificity_conflict_is_visible_and_stably_ordered() {
        let mut registry = ActionRegistry::new();
        for (id, label) in [("test.tie.zed", "Zed"), ("test.tie.alpha", "Alpha")] {
            registry
                .register(test_action(id, label, ActionScope::Application, &["q"]))
                .unwrap();
        }
        let resolution = registry.resolve_shortcut(
            &KeyChord::parse("q").unwrap(),
            &ActionContext::default(),
            &UserKeymap::default(),
        );
        let ShortcutResolution::Ambiguous(candidates) = resolution else {
            panic!("same-rank collision should be explicit")
        };
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.action.0)
                .collect::<Vec<_>>(),
            ["test.tie.alpha", "test.tie.zed"]
        );
    }

    #[test]
    fn user_keymap_can_override_and_unbind_defaults() {
        let registry = ActionRegistry::audec_defaults();
        let context = ActionContext {
            has_project: true,
            ..ActionContext::default()
        };
        let mut keymap = UserKeymap::default();
        keymap.set(ids::TRANSPORT_TOGGLE.0, vec![KeyChord::parse("p").unwrap()]);
        keymap.set(ids::TRANSPORT_STOP.0, Vec::new());
        assert_eq!(
            registry.resolve_shortcut(&KeyChord::parse("p").unwrap(), &context, &keymap),
            ShortcutResolution::Invoke(ids::TRANSPORT_TOGGLE)
        );
        assert_eq!(
            registry.resolve_shortcut(&KeyChord::parse("shift-space").unwrap(), &context, &keymap),
            ShortcutResolution::Unbound
        );
    }

    #[test]
    fn stale_projection_is_rejected_after_context_or_registry_change() {
        let mut registry = ActionRegistry::audec_defaults();
        let context = ActionContext {
            epoch: ContextEpoch(7),
            has_project: true,
            active_view: Some(WorkspaceViewId(9)),
            target: Some(EditorTarget::Arrangement),
            ..ActionContext::default()
        };
        let snapshot = registry.project(&context, &UserKeymap::default());
        let request = snapshot
            .request(
                ids::TRANSPORT_TOGGLE,
                InvocationOrigin::Accessibility,
                InvocationModifiers::default(),
                ActionParameters::default(),
            )
            .unwrap();
        let mut newer_context = context.clone();
        newer_context.epoch = ContextEpoch(8);
        assert!(matches!(
            registry.validate_request(&request, &newer_context),
            Err(ActionDispatchError::StaleContext { .. })
        ));

        registry
            .register(test_action(
                "test.dynamic.action",
                "Dynamic",
                ActionScope::Application,
                &[],
            ))
            .unwrap();
        assert!(matches!(
            registry.validate_request(&request, &context),
            Err(ActionDispatchError::StaleRegistry { .. })
        ));
    }

    #[test]
    fn request_carries_typed_parameters_and_current_semantic_target() {
        let registry = ActionRegistry::audec_defaults();
        let context = ActionContext {
            epoch: ContextEpoch(12),
            has_project: true,
            has_selection: true,
            active_view: Some(WorkspaceViewId(22)),
            target: Some(EditorTarget::Arrangement),
            ..ActionContext::default()
        };
        let mut parameters = ActionParameters::new();
        parameters.insert("frame", ActionParameterValue::Signed(48_000));
        parameters.insert("after", ActionParameterValue::Choice("pattern".into()));
        let request = registry
            .project(&context, &UserKeymap::default())
            .request(
                ids::EDIT_DUPLICATE,
                InvocationOrigin::ContextMenu,
                InvocationModifiers::default(),
                parameters,
            )
            .unwrap();
        assert_eq!(request.invocation.view, Some(WorkspaceViewId(22)));
        assert_eq!(request.invocation.target, Some(EditorTarget::Arrangement));
        assert_eq!(
            request.parameters.get("frame"),
            Some(&ActionParameterValue::Signed(48_000))
        );
        assert_eq!(
            registry.validate_request(&request, &context),
            Ok(request.invocation)
        );
    }

    #[test]
    fn malformed_ids_and_shortcuts_are_refused_at_registration() {
        let mut registry = ActionRegistry::new();
        assert!(matches!(
            registry.register(test_action(
                "Not an id",
                "Bad",
                ActionScope::Application,
                &[]
            )),
            Err(ActionRegistryError::InvalidId(_))
        ));
        assert!(matches!(
            registry.register(test_action(
                "test.bad.shortcut",
                "Bad shortcut",
                ActionScope::Application,
                &["cmd-a-b"]
            )),
            Err(ActionRegistryError::InvalidShortcut { .. })
        ));
    }
}
