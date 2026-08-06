//! UI settings persisted to a small JSON file in the data dir — pane widths and
//! collapse flags (jolt persisted the same set in localStorage).
//!
//! Loaded once at boot; saved debounced by the shell ([`SAVE_DEBOUNCE_MS`]).
//! Corrupt or missing files fall back to defaults; loaded values are clamped so a
//! hand-edited file can't wedge the layout.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub mod accounts;
pub mod appearance;
pub mod archived;
pub mod composer;
mod device_switcher;
pub mod devices;
pub mod hotkeys;
pub mod notifications;
pub mod secrets;
pub mod terminal;
pub mod vcs;
pub mod widgets;

/// Sidebar drag-resize bounds (px).
pub const SIDEBAR_MIN: f32 = 208.0;
pub const SIDEBAR_MAX: f32 = 400.0;
pub const SIDEBAR_DEFAULT: f32 = 256.0;

/// Right ("Changes") pane drag-resize bounds (px).
pub const RIGHT_PANE_MIN: f32 = 360.0;
pub const RIGHT_PANE_MAX: f32 = 760.0;
pub const RIGHT_PANE_DEFAULT: f32 = 520.0;

/// Terminal panel height bounds: 160px … 55% of the viewport (§1.10). The
/// viewport-relative cap applies at runtime; the absolute cap here only heals
/// hand-edited files.
pub const TERMINAL_MIN_HEIGHT: f32 = 160.0;
pub const TERMINAL_MAX_VH: f32 = 0.55;
pub const TERMINAL_ABS_MAX_HEIGHT: f32 = 2000.0;
pub const TERMINAL_DEFAULT_HEIGHT: f32 = 280.0;

/// Debounce for settings writes after a drag/toggle.
pub const SAVE_DEBOUNCE_MS: u64 = 400;

const FILE_NAME: &str = "ui-settings.json";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ScopeNavigation {
    pub last_space_id: Option<String>,
    pub open_tabs: Option<Vec<String>>,
    pub active_tab_id: Option<String>,
    pub space_filter: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UiSettings {
    pub sidebar_width: f32,
    pub sidebar_collapsed: bool,
    /// Legacy: the grouped-by-project toggle predates spaces (which group by
    /// folder inherently). Kept for file compatibility; no longer read.
    pub sidebar_grouped: bool,
    /// The last active space — restored on boot and used as the new-session
    /// fallback when the sidebar filter is "All spaces".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_space_id: Option<String>,
    /// Device-local open session tabs in drag order. `None` identifies a
    /// pre-tabs settings file and triggers a one-time legacy-order migration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_tabs: Option<Vec<String>>,
    /// Last active session tab, restored when it is still open and live.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_tab_id: Option<String>,
    /// Sidebar session filter (`None` = All spaces).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space_filter: Option<String>,
    /// Navigation snapshots partitioned by Local/Account scope. The top-level
    /// fields above are the currently active snapshot for compatibility.
    #[serde(skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub scope_navigation: std::collections::HashMap<String, ScopeNavigation>,
    /// Legacy per-space tab order. Read only by the open-tabs migration.
    #[serde(skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub tab_order: std::collections::HashMap<String, Vec<String>>,
    /// Legacy manual space order; retained for settings-file compatibility.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub space_order: Vec<String>,
    /// Deliver app-wide alerts through the operating system instead of in-app
    /// toasts.
    pub system_notifications_enabled: bool,
    pub right_pane_width: f32,
    /// Legacy: panel open flags are session-scoped in-memory state now via
    /// `shell::SessionPanels`. Kept for file compatibility; no longer read or
    /// written by the shell.
    pub right_pane_open: bool,
    pub terminal_height: f32,
    /// Legacy — see [`Self::right_pane_open`].
    pub terminal_open: bool,
    /// Customizable shortcut combinations.
    pub keymap: KeymapConfig,
    /// Light/dark preference. Defaults to following the OS.
    pub appearance: crate::appearance::AppearanceMode,
    /// Theme family used whenever the effective appearance is light.
    pub light_theme: String,
    /// Theme family used whenever the effective appearance is dark.
    pub dark_theme: String,
    /// Font family for application chrome, prose, and controls.
    pub ui_font: String,
    /// Font family used only by the chat composer.
    #[serde(default)]
    pub prompt_font: String,
    /// Font family for code, diffs, and hotkey chips.
    pub code_font: String,
    /// Font family used by terminal grids.
    pub terminal_font: String,
    /// Interface and prose font size in logical pixels.
    pub font_size_interface: u8,
    /// Composer input font size in logical pixels.
    pub font_size_prompt: u8,
    /// Code block and diff font size in logical pixels.
    pub font_size_code: u8,
    /// Terminal grid font size in logical pixels.
    pub font_size_terminal: u8,
    /// Shell command run when a new terminal opens. Empty uses the default
    /// interactive login shell.
    pub terminal_command: String,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            sidebar_width: SIDEBAR_DEFAULT,
            sidebar_collapsed: false,
            sidebar_grouped: false,
            last_space_id: None,
            open_tabs: None,
            active_tab_id: None,
            space_filter: None,
            scope_navigation: std::collections::HashMap::new(),
            tab_order: std::collections::HashMap::new(),
            space_order: Vec::new(),
            system_notifications_enabled: false,
            right_pane_width: RIGHT_PANE_DEFAULT,
            right_pane_open: false,
            terminal_height: TERMINAL_DEFAULT_HEIGHT,
            terminal_open: false,
            keymap: KeymapConfig::default(),
            appearance: crate::appearance::AppearanceMode::default(),
            light_theme: crate::themes::JOLT_THEME_ID.into(),
            dark_theme: crate::themes::JOLT_THEME_ID.into(),
            ui_font: crate::theme::DEFAULT_UI_FONT.into(),
            prompt_font: crate::theme::DEFAULT_UI_FONT.into(),
            code_font: crate::theme::DEFAULT_CODE_FONT.into(),
            terminal_font: crate::theme::DEFAULT_CODE_FONT.into(),
            font_size_interface: crate::theme::DEFAULT_INTERFACE_FONT_SIZE,
            font_size_prompt: crate::theme::DEFAULT_PROMPT_FONT_SIZE,
            font_size_code: crate::theme::DEFAULT_CODE_FONT_SIZE,
            font_size_terminal: crate::theme::DEFAULT_TERMINAL_FONT_SIZE,
            terminal_command: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Keymap (customizable hotkeys, §1.4)
// ---------------------------------------------------------------------------

/// A customizable app hotkey.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShortcutId {
    NewSession,
    ClearInput,
    CloseTab,
    PreviousTranscriptTurn,
    NextTranscriptTurn,
    OpenSettings,
    OpenSpacesDropdown,
    AddSpace,
    SearchSessions,
    ToggleSidebar,
    ToggleChanges,
    ToggleTerminal,
    NewTerminalTab,
    CloseTerminalTab,
    SelectTab1,
    SelectTab2,
    SelectTab3,
    SelectTab4,
    SelectTab5,
    SelectTab6,
    SelectTab7,
    SelectTab8,
    SelectLastTab,
    Quit,
    Hide,
    HideOthers,
    Minimize,
    CloseWindow,
    PerformanceHud,
}

impl ShortcutId {
    pub const TAB_SELECTION: [ShortcutId; 9] = [
        ShortcutId::SelectTab1,
        ShortcutId::SelectTab2,
        ShortcutId::SelectTab3,
        ShortcutId::SelectTab4,
        ShortcutId::SelectTab5,
        ShortcutId::SelectTab6,
        ShortcutId::SelectTab7,
        ShortcutId::SelectTab8,
        ShortcutId::SelectLastTab,
    ];

    /// Hotkeys available in this build and on this platform.
    pub fn all() -> Vec<ShortcutId> {
        let mut ids = vec![
            ShortcutId::NewSession,
            ShortcutId::ClearInput,
            ShortcutId::CloseTab,
            ShortcutId::PreviousTranscriptTurn,
            ShortcutId::NextTranscriptTurn,
            ShortcutId::OpenSettings,
            ShortcutId::OpenSpacesDropdown,
            ShortcutId::AddSpace,
            ShortcutId::SearchSessions,
            ShortcutId::ToggleSidebar,
            ShortcutId::ToggleChanges,
            ShortcutId::ToggleTerminal,
            ShortcutId::NewTerminalTab,
            ShortcutId::CloseTerminalTab,
        ];
        ids.extend(Self::TAB_SELECTION);
        if cfg!(target_os = "macos") {
            ids.extend([
                ShortcutId::Quit,
                ShortcutId::Hide,
                ShortcutId::HideOthers,
                ShortcutId::Minimize,
                ShortcutId::CloseWindow,
            ]);
        }
        if cfg!(any(debug_assertions, feature = "debug-ui")) {
            ids.push(ShortcutId::PerformanceHud);
        }
        ids
    }

    /// Row label.
    pub fn label(self) -> &'static str {
        match self {
            ShortcutId::NewSession => "New session",
            ShortcutId::ClearInput => "Clear input",
            ShortcutId::CloseTab => "Close current tab",
            ShortcutId::PreviousTranscriptTurn => "Previous transcript prompt",
            ShortcutId::NextTranscriptTurn => "Next transcript prompt",
            ShortcutId::OpenSettings => "Open settings",
            ShortcutId::OpenSpacesDropdown => "Open spaces dropdown",
            ShortcutId::AddSpace => "Add space",
            ShortcutId::SearchSessions => "Search sessions",
            ShortcutId::ToggleSidebar => "Toggle left sidebar",
            ShortcutId::ToggleChanges => "Toggle right sidebar",
            ShortcutId::ToggleTerminal => "Toggle terminal",
            ShortcutId::NewTerminalTab => "New terminal tab",
            ShortcutId::CloseTerminalTab => "Close terminal tab",
            ShortcutId::SelectTab1 => "Select tab 1",
            ShortcutId::SelectTab2 => "Select tab 2",
            ShortcutId::SelectTab3 => "Select tab 3",
            ShortcutId::SelectTab4 => "Select tab 4",
            ShortcutId::SelectTab5 => "Select tab 5",
            ShortcutId::SelectTab6 => "Select tab 6",
            ShortcutId::SelectTab7 => "Select tab 7",
            ShortcutId::SelectTab8 => "Select tab 8",
            ShortcutId::SelectLastTab => "Select last tab",
            ShortcutId::Quit => "Quit Jolt",
            ShortcutId::Hide => "Hide Jolt",
            ShortcutId::HideOthers => "Hide other applications",
            ShortcutId::Minimize => "Minimize window",
            ShortcutId::CloseWindow => "Close window",
            ShortcutId::PerformanceHud => "Toggle performance HUD",
        }
    }

    pub fn default_combo(self) -> &'static str {
        match self {
            ShortcutId::NewSession => "mod-n",
            ShortcutId::ClearInput => "mod-c",
            ShortcutId::CloseTab => "mod-w",
            ShortcutId::PreviousTranscriptTurn => "mod-shift-up",
            ShortcutId::NextTranscriptTurn => "mod-shift-down",
            ShortcutId::OpenSettings => "mod-,",
            ShortcutId::OpenSpacesDropdown => "mod-shift-k",
            ShortcutId::AddSpace => "mod-k",
            ShortcutId::SearchSessions => "mod-shift-f",
            ShortcutId::ToggleSidebar => "mod-e",
            ShortcutId::ToggleChanges => "mod-b",
            ShortcutId::ToggleTerminal => "mod-`",
            ShortcutId::NewTerminalTab => "mod-t",
            ShortcutId::CloseTerminalTab => "mod-shift-w",
            ShortcutId::SelectTab1 => "mod-1",
            ShortcutId::SelectTab2 => "mod-2",
            ShortcutId::SelectTab3 => "mod-3",
            ShortcutId::SelectTab4 => "mod-4",
            ShortcutId::SelectTab5 => "mod-5",
            ShortcutId::SelectTab6 => "mod-6",
            ShortcutId::SelectTab7 => "mod-7",
            ShortcutId::SelectTab8 => "mod-8",
            ShortcutId::SelectLastTab => "mod-9",
            ShortcutId::Quit => "mod-q",
            ShortcutId::Hide => "mod-h",
            ShortcutId::HideOthers => "mod-alt-h",
            ShortcutId::Minimize => "mod-m",
            ShortcutId::CloseWindow => "mod-w",
            ShortcutId::PerformanceHud => "mod-shift-f12",
        }
    }
}

/// Persisted hotkey combinations. Stored platform-neutral (for example,
/// "mod-e"); translated to "cmd-e"/"ctrl-e" at bind time by
/// [`platform_combo`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct KeymapConfig {
    pub new_session: String,
    pub clear_input: String,
    pub close_tab: String,
    pub previous_transcript_turn: String,
    pub next_transcript_turn: String,
    pub open_settings: String,
    pub open_spaces_dropdown: String,
    pub add_space: String,
    pub search_sessions: String,
    pub toggle_sidebar: String,
    pub toggle_changes: String,
    pub toggle_terminal: String,
    pub new_terminal_tab: String,
    pub close_terminal_tab: String,
    pub select_tab_1: String,
    pub select_tab_2: String,
    pub select_tab_3: String,
    pub select_tab_4: String,
    pub select_tab_5: String,
    pub select_tab_6: String,
    pub select_tab_7: String,
    pub select_tab_8: String,
    pub select_last_tab: String,
    pub quit: String,
    pub hide: String,
    pub hide_others: String,
    pub minimize: String,
    pub close_window: String,
    pub performance_hud: String,
}

impl Default for KeymapConfig {
    fn default() -> Self {
        Self {
            new_session: ShortcutId::NewSession.default_combo().into(),
            clear_input: ShortcutId::ClearInput.default_combo().into(),
            close_tab: ShortcutId::CloseTab.default_combo().into(),
            previous_transcript_turn: ShortcutId::PreviousTranscriptTurn.default_combo().into(),
            next_transcript_turn: ShortcutId::NextTranscriptTurn.default_combo().into(),
            open_settings: ShortcutId::OpenSettings.default_combo().into(),
            open_spaces_dropdown: ShortcutId::OpenSpacesDropdown.default_combo().into(),
            add_space: ShortcutId::AddSpace.default_combo().into(),
            search_sessions: ShortcutId::SearchSessions.default_combo().into(),
            toggle_sidebar: ShortcutId::ToggleSidebar.default_combo().into(),
            toggle_changes: ShortcutId::ToggleChanges.default_combo().into(),
            toggle_terminal: ShortcutId::ToggleTerminal.default_combo().into(),
            new_terminal_tab: ShortcutId::NewTerminalTab.default_combo().into(),
            close_terminal_tab: ShortcutId::CloseTerminalTab.default_combo().into(),
            select_tab_1: ShortcutId::SelectTab1.default_combo().into(),
            select_tab_2: ShortcutId::SelectTab2.default_combo().into(),
            select_tab_3: ShortcutId::SelectTab3.default_combo().into(),
            select_tab_4: ShortcutId::SelectTab4.default_combo().into(),
            select_tab_5: ShortcutId::SelectTab5.default_combo().into(),
            select_tab_6: ShortcutId::SelectTab6.default_combo().into(),
            select_tab_7: ShortcutId::SelectTab7.default_combo().into(),
            select_tab_8: ShortcutId::SelectTab8.default_combo().into(),
            select_last_tab: ShortcutId::SelectLastTab.default_combo().into(),
            quit: ShortcutId::Quit.default_combo().into(),
            hide: ShortcutId::Hide.default_combo().into(),
            hide_others: ShortcutId::HideOthers.default_combo().into(),
            minimize: ShortcutId::Minimize.default_combo().into(),
            close_window: ShortcutId::CloseWindow.default_combo().into(),
            performance_hud: ShortcutId::PerformanceHud.default_combo().into(),
        }
    }
}

impl KeymapConfig {
    pub fn get(&self, id: ShortcutId) -> &str {
        match id {
            ShortcutId::NewSession => &self.new_session,
            ShortcutId::ClearInput => &self.clear_input,
            ShortcutId::CloseTab => &self.close_tab,
            ShortcutId::PreviousTranscriptTurn => &self.previous_transcript_turn,
            ShortcutId::NextTranscriptTurn => &self.next_transcript_turn,
            ShortcutId::OpenSettings => &self.open_settings,
            ShortcutId::OpenSpacesDropdown => &self.open_spaces_dropdown,
            ShortcutId::AddSpace => &self.add_space,
            ShortcutId::SearchSessions => &self.search_sessions,
            ShortcutId::ToggleSidebar => &self.toggle_sidebar,
            ShortcutId::ToggleChanges => &self.toggle_changes,
            ShortcutId::ToggleTerminal => &self.toggle_terminal,
            ShortcutId::NewTerminalTab => &self.new_terminal_tab,
            ShortcutId::CloseTerminalTab => &self.close_terminal_tab,
            ShortcutId::SelectTab1 => &self.select_tab_1,
            ShortcutId::SelectTab2 => &self.select_tab_2,
            ShortcutId::SelectTab3 => &self.select_tab_3,
            ShortcutId::SelectTab4 => &self.select_tab_4,
            ShortcutId::SelectTab5 => &self.select_tab_5,
            ShortcutId::SelectTab6 => &self.select_tab_6,
            ShortcutId::SelectTab7 => &self.select_tab_7,
            ShortcutId::SelectTab8 => &self.select_tab_8,
            ShortcutId::SelectLastTab => &self.select_last_tab,
            ShortcutId::Quit => &self.quit,
            ShortcutId::Hide => &self.hide,
            ShortcutId::HideOthers => &self.hide_others,
            ShortcutId::Minimize => &self.minimize,
            ShortcutId::CloseWindow => &self.close_window,
            ShortcutId::PerformanceHud => &self.performance_hud,
        }
    }

    pub fn set(&mut self, id: ShortcutId, combo: String) {
        match id {
            ShortcutId::NewSession => self.new_session = combo,
            ShortcutId::ClearInput => self.clear_input = combo,
            ShortcutId::CloseTab => self.close_tab = combo,
            ShortcutId::PreviousTranscriptTurn => self.previous_transcript_turn = combo,
            ShortcutId::NextTranscriptTurn => self.next_transcript_turn = combo,
            ShortcutId::OpenSettings => self.open_settings = combo,
            ShortcutId::OpenSpacesDropdown => self.open_spaces_dropdown = combo,
            ShortcutId::AddSpace => self.add_space = combo,
            ShortcutId::SearchSessions => self.search_sessions = combo,
            ShortcutId::ToggleSidebar => self.toggle_sidebar = combo,
            ShortcutId::ToggleChanges => self.toggle_changes = combo,
            ShortcutId::ToggleTerminal => self.toggle_terminal = combo,
            ShortcutId::NewTerminalTab => self.new_terminal_tab = combo,
            ShortcutId::CloseTerminalTab => self.close_terminal_tab = combo,
            ShortcutId::SelectTab1 => self.select_tab_1 = combo,
            ShortcutId::SelectTab2 => self.select_tab_2 = combo,
            ShortcutId::SelectTab3 => self.select_tab_3 = combo,
            ShortcutId::SelectTab4 => self.select_tab_4 = combo,
            ShortcutId::SelectTab5 => self.select_tab_5 = combo,
            ShortcutId::SelectTab6 => self.select_tab_6 = combo,
            ShortcutId::SelectTab7 => self.select_tab_7 = combo,
            ShortcutId::SelectTab8 => self.select_tab_8 = combo,
            ShortcutId::SelectLastTab => self.select_last_tab = combo,
            ShortcutId::Quit => self.quit = combo,
            ShortcutId::Hide => self.hide = combo,
            ShortcutId::HideOthers => self.hide_others = combo,
            ShortcutId::Minimize => self.minimize = combo,
            ShortcutId::CloseWindow => self.close_window = combo,
            ShortcutId::PerformanceHud => self.performance_hud = combo,
        }
    }

    pub fn reset(&mut self, id: ShortcutId) {
        self.set(id, id.default_combo().to_string());
    }
}

/// Build a combo string from a recorded keystroke. The primary modifier
/// (cmd on macOS, ctrl elsewhere — either recorded key maps in) becomes "mod";
/// bare modifier presses record nothing.
pub fn combo_from_keystroke(
    ctrl: bool,
    alt: bool,
    shift: bool,
    cmd: bool,
    key: &str,
) -> Option<String> {
    let key = key.trim().to_lowercase();
    if key.is_empty()
        || matches!(
            key.as_str(),
            "ctrl" | "control" | "alt" | "shift" | "cmd" | "platform" | "fn"
        )
    {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    if ctrl || cmd {
        parts.push("mod");
    }
    if alt {
        parts.push("alt");
    }
    if shift {
        parts.push("shift");
    }
    parts.push(&key);
    Some(parts.join("-"))
}

/// Whether two actions intentionally share one hotkey. Cmd+W closes the
/// current tab in chat mode and falls through to Close Window in Settings.
pub fn hotkeys_can_overlap(first: ShortcutId, second: ShortcutId) -> bool {
    matches!(
        (first, second),
        (ShortcutId::CloseTab, ShortcutId::CloseWindow)
            | (ShortcutId::CloseWindow, ShortcutId::CloseTab)
    )
}

/// Hotkey ids whose combinations collide with another action.
pub fn conflicted_shortcuts(keymap: &KeymapConfig) -> Vec<ShortcutId> {
    let ids = ShortcutId::all();
    ids.iter()
        .copied()
        .filter(|&id| {
            let combo = keymap.get(id);
            !combo.is_empty()
                && ids.iter().copied().any(|other| {
                    other != id && !hotkeys_can_overlap(id, other) && keymap.get(other) == combo
                })
        })
        .collect()
}

/// Translate a stored combo into a bindable keystroke for this platform.
pub fn platform_combo(combo: &str) -> String {
    let primary = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    combo
        .split('-')
        .map(|part| if part == "mod" { primary } else { part })
        .collect::<Vec<_>>()
        .join("-")
}

/// Human-readable combo for hotkey chips ("mod-e" → "⌘+E"/"Ctrl+E").
pub fn display_combo(combo: &str) -> String {
    combo
        .split('-')
        .map(|part| match part {
            "mod" => {
                if cfg!(target_os = "macos") {
                    "⌘".to_string()
                } else {
                    "Ctrl".to_string()
                }
            }
            "alt" => "Alt".to_string(),
            "shift" => "Shift".to_string(),
            other => {
                let mut chars = other.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

impl UiSettings {
    /// Clamp widths into their legal ranges (also heals NaN to defaults).
    pub fn clamped(mut self) -> Self {
        self.sidebar_width = clamp_or(
            self.sidebar_width,
            SIDEBAR_MIN,
            SIDEBAR_MAX,
            SIDEBAR_DEFAULT,
        );
        self.right_pane_width = clamp_or(
            self.right_pane_width,
            RIGHT_PANE_MIN,
            RIGHT_PANE_MAX,
            RIGHT_PANE_DEFAULT,
        );
        self.terminal_height = clamp_or(
            self.terminal_height,
            TERMINAL_MIN_HEIGHT,
            TERMINAL_ABS_MAX_HEIGHT,
            TERMINAL_DEFAULT_HEIGHT,
        );
        if self.prompt_font.trim().is_empty() {
            self.prompt_font = self.ui_font.clone();
        }
        if self.light_theme.trim().is_empty() {
            self.light_theme = crate::themes::JOLT_THEME_ID.into();
        }
        if self.dark_theme.trim().is_empty() {
            self.dark_theme = crate::themes::JOLT_THEME_ID.into();
        }
        let sizes = self.font_sizes().clamped();
        self.font_size_interface = sizes.interface;
        self.font_size_prompt = sizes.prompt;
        self.font_size_code = sizes.code;
        self.font_size_terminal = sizes.terminal;
        self
    }

    pub fn font_sizes(&self) -> crate::theme::FontSizes {
        crate::theme::FontSizes {
            interface: self.font_size_interface,
            prompt: self.font_size_prompt,
            code: self.font_size_code,
            terminal: self.font_size_terminal,
        }
    }

    /// Load from `{data_dir}/ui-settings.json`; defaults on any failure.
    pub fn load(data_dir: &Path) -> Self {
        match std::fs::read_to_string(Self::path(data_dir)) {
            Ok(text) => match serde_json::from_str::<UiSettings>(&text) {
                Ok(settings) => settings.clamped(),
                Err(err) => {
                    tracing::warn!(error = %err, "ui-settings corrupt; using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Write atomically (temp file + rename) so a crash mid-write never corrupts.
    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let path = Self::path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }
}

fn clamp_or(value: f32, min: f32, max: f32, default: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let settings = UiSettings {
            sidebar_width: 300.0,
            sidebar_collapsed: true,
            sidebar_grouped: true,
            last_space_id: Some("space-1".into()),
            open_tabs: Some(vec!["b".into(), "a".into()]),
            active_tab_id: Some("a".into()),
            space_filter: Some("space-1".into()),
            scope_navigation: std::collections::HashMap::from([(
                "account".into(),
                ScopeNavigation {
                    active_tab_id: Some("a".into()),
                    ..ScopeNavigation::default()
                },
            )]),
            tab_order: std::collections::HashMap::from([(
                "space-1".to_string(),
                vec!["b".to_string(), "a".to_string()],
            )]),
            space_order: vec!["space-2".to_string(), "space-1".to_string()],
            system_notifications_enabled: true,
            right_pane_width: 700.0,
            right_pane_open: true,
            terminal_height: 320.0,
            terminal_open: true,
            keymap: KeymapConfig {
                toggle_sidebar: "mod-shift-s".into(),
                ..KeymapConfig::default()
            },
            appearance: crate::appearance::AppearanceMode::Light,
            light_theme: crate::themes::CATPPUCCIN_THEME_ID.into(),
            dark_theme: crate::themes::ROSE_PINE_THEME_ID.into(),
            ui_font: "Avenir Next".into(),
            prompt_font: "Iosevka".into(),
            code_font: "Menlo".into(),
            terminal_font: "Berkeley Mono".into(),
            font_size_interface: 16,
            font_size_prompt: 15,
            font_size_code: 14,
            font_size_terminal: 12,
            terminal_command: "exec fish".into(),
        };
        settings.save(dir.path()).unwrap();
        assert_eq!(UiSettings::load(dir.path()), settings);
    }

    /// A settings file written before light mode existed has no `appearance`
    /// key; it must load as "follow the OS" rather than failing the whole parse
    /// and resetting every other preference to defaults.
    #[test]
    fn settings_without_appearance_default_to_system() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(dir.path()),
            r#"{
                "sidebarWidth": 300,
                "systemNotificationsEnabled": true,
                "uiFont": "Avenir Next"
            }"#,
        )
        .unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.appearance, crate::appearance::AppearanceMode::System);
        assert_eq!(loaded.ui_font, "Avenir Next");
        assert_eq!(loaded.prompt_font, "Avenir Next");
        assert_eq!(loaded.code_font, crate::theme::DEFAULT_CODE_FONT);
        assert_eq!(loaded.terminal_font, crate::theme::DEFAULT_CODE_FONT);
        assert_eq!(loaded.font_sizes(), crate::theme::FontSizes::default());
        assert!(loaded.terminal_command.is_empty());
        assert_eq!(loaded.sidebar_width, 300.0);
        assert!(
            loaded.system_notifications_enabled,
            "other keys still parse"
        );
    }

    #[test]
    fn missing_and_corrupt_files_yield_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(UiSettings::load(dir.path()), UiSettings::default());
        std::fs::write(UiSettings::path(dir.path()), "{not json").unwrap();
        assert_eq!(UiSettings::load(dir.path()), UiSettings::default());
    }

    #[test]
    fn loaded_values_are_clamped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            UiSettings::path(dir.path()),
            r#"{
                "sidebarWidth": 10000,
                "rightPaneWidth": 1,
                "fontSizeInterface": 255,
                "fontSizePrompt": 1,
                "fontSizeCode": 255,
                "fontSizeTerminal": 1
            }"#,
        )
        .unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.sidebar_width, SIDEBAR_MAX);
        assert_eq!(loaded.right_pane_width, RIGHT_PANE_MIN);
        assert_eq!(
            loaded.font_size_interface,
            crate::theme::MAX_INTERFACE_FONT_SIZE
        );
        assert_eq!(loaded.font_size_prompt, crate::theme::MIN_PROMPT_FONT_SIZE);
        assert_eq!(loaded.font_size_code, crate::theme::MAX_CODE_FONT_SIZE);
        assert_eq!(
            loaded.font_size_terminal,
            crate::theme::MIN_TERMINAL_FONT_SIZE
        );
    }

    #[test]
    fn nan_heals_to_default() {
        let healed = UiSettings {
            sidebar_width: f32::NAN,
            ..Default::default()
        }
        .clamped();
        assert_eq!(healed.sidebar_width, SIDEBAR_DEFAULT);
    }

    #[test]
    fn defaults_match_jolt() {
        let d = UiSettings::default();
        assert_eq!(d.sidebar_width, 256.0);
        assert_eq!(d.right_pane_width, 520.0);
        assert_eq!(d.terminal_height, 280.0);
        assert!(!d.sidebar_collapsed && !d.right_pane_open && !d.terminal_open);
        assert_eq!(d.ui_font, crate::theme::DEFAULT_UI_FONT);
        assert_eq!(d.prompt_font, crate::theme::DEFAULT_UI_FONT);
        assert_eq!(d.code_font, crate::theme::DEFAULT_CODE_FONT);
        assert_eq!(d.terminal_font, crate::theme::DEFAULT_CODE_FONT);
        assert_eq!(d.font_sizes(), crate::theme::FontSizes::default());
        assert!(d.terminal_command.is_empty());
        assert!(!d.system_notifications_enabled);
    }

    #[test]
    fn keymap_defaults_and_reset() {
        let mut keymap = KeymapConfig::default();
        for id in ShortcutId::all() {
            assert_eq!(keymap.get(id), id.default_combo());
            assert!(gpui::Keystroke::parse(&platform_combo(keymap.get(id))).is_ok());
        }
        assert_eq!(keymap.get(ShortcutId::NewSession), "mod-n");
        assert_eq!(keymap.get(ShortcutId::ClearInput), "mod-c");
        assert_eq!(keymap.get(ShortcutId::CloseTab), "mod-w");
        assert_eq!(
            keymap.get(ShortcutId::PreviousTranscriptTurn),
            "mod-shift-up"
        );
        assert_eq!(keymap.get(ShortcutId::NextTranscriptTurn), "mod-shift-down");
        assert_eq!(keymap.get(ShortcutId::OpenSettings), "mod-,");
        assert_eq!(keymap.get(ShortcutId::OpenSpacesDropdown), "mod-shift-k");
        assert_eq!(keymap.get(ShortcutId::AddSpace), "mod-k");
        assert_eq!(keymap.get(ShortcutId::SearchSessions), "mod-shift-f");
        assert_eq!(keymap.get(ShortcutId::ToggleSidebar), "mod-e");
        assert_eq!(keymap.get(ShortcutId::ToggleChanges), "mod-b");
        assert_eq!(keymap.get(ShortcutId::ToggleTerminal), "mod-`");
        assert_eq!(keymap.get(ShortcutId::NewTerminalTab), "mod-t");
        assert_eq!(keymap.get(ShortcutId::CloseTerminalTab), "mod-shift-w");
        keymap.set(ShortcutId::ClearInput, "mod-shift-c".into());
        assert_eq!(keymap.get(ShortcutId::ClearInput), "mod-shift-c");
        keymap.reset(ShortcutId::ClearInput);
        assert_eq!(keymap.get(ShortcutId::ClearInput), "mod-c");
        keymap.set(ShortcutId::CloseTab, "mod-shift-w".into());
        assert_eq!(keymap.get(ShortcutId::CloseTab), "mod-shift-w");
        keymap.reset(ShortcutId::CloseTab);
        assert_eq!(keymap.get(ShortcutId::CloseTab), "mod-w");
        keymap.set(ShortcutId::ToggleSidebar, "mod-shift-x".into());
        assert_eq!(keymap.get(ShortcutId::ToggleSidebar), "mod-shift-x");
        keymap.reset(ShortcutId::ToggleSidebar);
        assert_eq!(keymap.get(ShortcutId::ToggleSidebar), "mod-e");
    }

    #[test]
    fn combo_recording() {
        // Primary modifier (ctrl or cmd) normalizes to "mod".
        assert_eq!(
            combo_from_keystroke(true, false, false, false, "s"),
            Some("mod-s".into())
        );
        assert_eq!(
            combo_from_keystroke(false, false, false, true, "s"),
            Some("mod-s".into())
        );
        assert_eq!(
            combo_from_keystroke(true, true, true, false, "K"),
            Some("mod-alt-shift-k".into())
        );
        // Plain keys record without modifiers (Esc is filtered by the caller).
        assert_eq!(
            combo_from_keystroke(false, false, false, false, "f5"),
            Some("f5".into())
        );
        // Bare modifier presses record nothing.
        assert_eq!(
            combo_from_keystroke(true, false, false, false, "ctrl"),
            None
        );
        assert_eq!(
            combo_from_keystroke(false, false, true, false, "shift"),
            None
        );
        assert_eq!(combo_from_keystroke(false, false, false, false, ""), None);
    }

    #[test]
    fn conflict_detection() {
        let mut keymap = KeymapConfig::default();
        assert!(conflicted_shortcuts(&keymap).is_empty());
        keymap.set(ShortcutId::ToggleChanges, "mod-e".into());
        let conflicts = conflicted_shortcuts(&keymap);
        assert!(conflicts.contains(&ShortcutId::ToggleSidebar));
        assert!(conflicts.contains(&ShortcutId::ToggleChanges));
        assert!(!conflicts.contains(&ShortcutId::ToggleTerminal));
        assert!(!conflicts.contains(&ShortcutId::AddSpace));
        assert!(!conflicts.contains(&ShortcutId::SearchSessions));
        assert!(!conflicts.contains(&ShortcutId::OpenSpacesDropdown));
        assert!(!conflicts.contains(&ShortcutId::OpenSettings));
        assert!(!conflicts.contains(&ShortcutId::NewSession));
        assert!(!conflicts.contains(&ShortcutId::ClearInput));
        assert!(!conflicts.contains(&ShortcutId::CloseTab));
        keymap.reset(ShortcutId::ToggleChanges);
        assert!(conflicted_shortcuts(&keymap).is_empty());
    }

    #[test]
    fn combo_translation() {
        let primary = if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "ctrl"
        };
        assert_eq!(platform_combo("mod-s"), format!("{primary}-s"));
        assert_eq!(platform_combo("mod-,"), format!("{primary}-,"));
        assert_eq!(platform_combo("mod-`"), format!("{primary}-`"));
        assert!(gpui::Keystroke::parse(&platform_combo("mod-,")).is_ok());
        assert!(gpui::Keystroke::parse(&platform_combo("mod-`")).is_ok());
        assert_eq!(platform_combo("alt-f4"), "alt-f4");
        let display_primary = if cfg!(target_os = "macos") {
            "⌘"
        } else {
            "Ctrl"
        };
        assert_eq!(
            display_combo("mod-shift-s"),
            format!("{display_primary}+Shift+S")
        );
        assert_eq!(display_combo("f5"), "F5");
        assert_eq!(display_combo("mod-,"), format!("{display_primary}+,"));
        assert_eq!(display_combo("mod-`"), format!("{display_primary}+`"));
    }

    #[test]
    fn keymap_survives_old_settings_files() {
        // Files written before the keymap existed load with defaults.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(UiSettings::path(dir.path()), r#"{"sidebarWidth": 300}"#).unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.keymap, KeymapConfig::default());
        assert!(!loaded.sidebar_grouped);
    }

    #[test]
    fn old_keymap_gains_new_hotkey_defaults() {
        let keymap: KeymapConfig = serde_json::from_str(
            r#"{
                "toggleSidebar": "mod-shift-e",
                "toggleChanges": "mod-b",
                "toggleTerminal": "mod-`"
            }"#,
        )
        .unwrap();
        assert_eq!(keymap.get(ShortcutId::NewSession), "mod-n");
        assert_eq!(keymap.get(ShortcutId::ClearInput), "mod-c");
        assert_eq!(keymap.get(ShortcutId::CloseTab), "mod-w");
        assert_eq!(
            keymap.get(ShortcutId::PreviousTranscriptTurn),
            "mod-shift-up"
        );
        assert_eq!(keymap.get(ShortcutId::NextTranscriptTurn), "mod-shift-down");
        assert_eq!(keymap.get(ShortcutId::OpenSettings), "mod-,");
        assert_eq!(keymap.get(ShortcutId::OpenSpacesDropdown), "mod-shift-k");
        assert_eq!(keymap.get(ShortcutId::AddSpace), "mod-k");
        assert_eq!(keymap.get(ShortcutId::SearchSessions), "mod-shift-f");
        assert_eq!(keymap.get(ShortcutId::ToggleSidebar), "mod-shift-e");
        assert_eq!(keymap.get(ShortcutId::NewTerminalTab), "mod-t");
        assert_eq!(keymap.get(ShortcutId::CloseTerminalTab), "mod-shift-w");
        assert_eq!(keymap.get(ShortcutId::SelectTab1), "mod-1");
        assert_eq!(keymap.get(ShortcutId::SelectLastTab), "mod-9");
        assert_eq!(keymap.get(ShortcutId::Quit), "mod-q");
    }

    #[test]
    fn terminal_height_clamps_on_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(UiSettings::path(dir.path()), r#"{"terminalHeight": 5}"#).unwrap();
        assert_eq!(
            UiSettings::load(dir.path()).terminal_height,
            TERMINAL_MIN_HEIGHT
        );
        std::fs::write(UiSettings::path(dir.path()), r#"{"terminalHeight": 99999}"#).unwrap();
        assert_eq!(
            UiSettings::load(dir.path()).terminal_height,
            TERMINAL_ABS_MAX_HEIGHT
        );
    }
}
