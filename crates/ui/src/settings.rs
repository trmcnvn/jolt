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
#[serde(rename_all = "camelCase")]
pub struct ScopeNavigation {
    pub last_space_id: Option<String>,
    pub space_filter: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiSettings {
    pub sidebar_width: f32,
    pub sidebar_collapsed: bool,
    /// The last active space — restored on boot and used as the new-session
    /// fallback when the sidebar filter is "All spaces".
    pub last_space_id: Option<String>,
    /// Sidebar session filter (`None` = All spaces).
    pub space_filter: Option<String>,
    /// Navigation snapshots partitioned by Local/Account scope.
    pub scope_navigation: std::collections::HashMap<String, ScopeNavigation>,
    /// Deliver app-wide alerts through the operating system instead of in-app
    /// toasts.
    pub system_notifications_enabled: bool,
    pub right_pane_width: f32,
    pub terminal_height: f32,
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
    pub prompt_font: String,
    /// Font family for code, diffs, and hotkey chips.
    pub code_font: String,
    /// Font family used by terminal grids.
    pub terminal_font: String,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            sidebar_width: SIDEBAR_DEFAULT,
            sidebar_collapsed: false,
            last_space_id: None,
            space_filter: None,
            scope_navigation: std::collections::HashMap::new(),
            system_notifications_enabled: false,
            right_pane_width: RIGHT_PANE_DEFAULT,
            terminal_height: TERMINAL_DEFAULT_HEIGHT,
            keymap: KeymapConfig::default(),
            appearance: crate::appearance::AppearanceMode::default(),
            light_theme: crate::themes::JOLT_THEME_ID.into(),
            dark_theme: crate::themes::JOLT_THEME_ID.into(),
            ui_font: crate::theme::DEFAULT_UI_FONT.into(),
            prompt_font: crate::theme::DEFAULT_UI_FONT.into(),
            code_font: crate::theme::DEFAULT_CODE_FONT.into(),
            terminal_font: crate::theme::DEFAULT_CODE_FONT.into(),
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
    PreviousTranscriptTurn,
    NextTranscriptTurn,
    SearchTranscript,
    OpenSettings,
    OpenSpacesDropdown,
    AddSpace,
    SearchThreads,
    ToggleSidebar,
    ToggleChanges,
    ToggleTerminal,
    NewTerminalTab,
    CloseTerminalTab,
    SelectSession1,
    SelectSession2,
    SelectSession3,
    SelectSession4,
    SelectSession5,
    SelectSession6,
    SelectSession7,
    SelectSession8,
    SelectSession9,
    Quit,
    Hide,
    HideOthers,
    Minimize,
    CloseWindow,
    PerformanceHud,
}

impl ShortcutId {
    pub const SESSION_SELECTION: [ShortcutId; 9] = [
        ShortcutId::SelectSession1,
        ShortcutId::SelectSession2,
        ShortcutId::SelectSession3,
        ShortcutId::SelectSession4,
        ShortcutId::SelectSession5,
        ShortcutId::SelectSession6,
        ShortcutId::SelectSession7,
        ShortcutId::SelectSession8,
        ShortcutId::SelectSession9,
    ];

    /// Hotkeys available in this build and on this platform.
    pub fn all() -> Vec<ShortcutId> {
        let mut ids = vec![
            ShortcutId::NewSession,
            ShortcutId::ClearInput,
            ShortcutId::PreviousTranscriptTurn,
            ShortcutId::NextTranscriptTurn,
            ShortcutId::SearchTranscript,
            ShortcutId::OpenSettings,
            ShortcutId::OpenSpacesDropdown,
            ShortcutId::AddSpace,
            ShortcutId::SearchThreads,
            ShortcutId::ToggleSidebar,
            ShortcutId::ToggleChanges,
            ShortcutId::ToggleTerminal,
            ShortcutId::NewTerminalTab,
            ShortcutId::CloseTerminalTab,
        ];
        ids.extend(Self::SESSION_SELECTION);
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
            ShortcutId::NewSession => "New thread",
            ShortcutId::ClearInput => "Clear input",
            ShortcutId::PreviousTranscriptTurn => "Previous transcript prompt",
            ShortcutId::NextTranscriptTurn => "Next transcript prompt",
            ShortcutId::SearchTranscript => "Search transcript",
            ShortcutId::OpenSettings => "Open settings",
            ShortcutId::OpenSpacesDropdown => "Open spaces dropdown",
            ShortcutId::AddSpace => "Add space",
            ShortcutId::SearchThreads => "Search threads",
            ShortcutId::ToggleSidebar => "Toggle left sidebar",
            ShortcutId::ToggleChanges => "Toggle right sidebar",
            ShortcutId::ToggleTerminal => "Toggle terminal",
            ShortcutId::NewTerminalTab => "New terminal tab",
            ShortcutId::CloseTerminalTab => "Close terminal tab",
            ShortcutId::SelectSession1 => "Select thread 1",
            ShortcutId::SelectSession2 => "Select thread 2",
            ShortcutId::SelectSession3 => "Select thread 3",
            ShortcutId::SelectSession4 => "Select thread 4",
            ShortcutId::SelectSession5 => "Select thread 5",
            ShortcutId::SelectSession6 => "Select thread 6",
            ShortcutId::SelectSession7 => "Select thread 7",
            ShortcutId::SelectSession8 => "Select thread 8",
            ShortcutId::SelectSession9 => "Select thread 9",
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
            ShortcutId::PreviousTranscriptTurn => "mod-shift-up",
            ShortcutId::NextTranscriptTurn => "mod-shift-down",
            ShortcutId::SearchTranscript => "mod-f",
            ShortcutId::OpenSettings => "mod-,",
            ShortcutId::OpenSpacesDropdown => "mod-shift-k",
            ShortcutId::AddSpace => "mod-k",
            ShortcutId::SearchThreads => "mod-shift-f",
            ShortcutId::ToggleSidebar => "mod-e",
            ShortcutId::ToggleChanges => "mod-b",
            ShortcutId::ToggleTerminal => "mod-`",
            ShortcutId::NewTerminalTab => "mod-t",
            ShortcutId::CloseTerminalTab => "mod-shift-w",
            ShortcutId::SelectSession1 => "mod-1",
            ShortcutId::SelectSession2 => "mod-2",
            ShortcutId::SelectSession3 => "mod-3",
            ShortcutId::SelectSession4 => "mod-4",
            ShortcutId::SelectSession5 => "mod-5",
            ShortcutId::SelectSession6 => "mod-6",
            ShortcutId::SelectSession7 => "mod-7",
            ShortcutId::SelectSession8 => "mod-8",
            ShortcutId::SelectSession9 => "mod-9",
            ShortcutId::Quit => "mod-q",
            ShortcutId::Hide => "mod-h",
            ShortcutId::HideOthers => "mod-alt-h",
            ShortcutId::Minimize => "mod-m",
            ShortcutId::CloseWindow => "mod-w",
            ShortcutId::PerformanceHud => "mod-shift-f12",
        }
    }
}

fn default_search_transcript() -> String {
    ShortcutId::SearchTranscript.default_combo().into()
}

/// Persisted hotkey combinations. Stored platform-neutral (for example,
/// "mod-e"); translated to "cmd-e"/"ctrl-e" at bind time by
/// [`platform_combo`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct KeymapConfig {
    pub new_session: String,
    pub clear_input: String,
    pub previous_transcript_turn: String,
    pub next_transcript_turn: String,
    #[serde(default = "default_search_transcript")]
    pub search_transcript: String,
    pub open_settings: String,
    pub open_spaces_dropdown: String,
    pub add_space: String,
    pub search_sessions: String,
    pub toggle_sidebar: String,
    pub toggle_changes: String,
    pub toggle_terminal: String,
    pub new_terminal_tab: String,
    pub close_terminal_tab: String,
    pub select_session_1: String,
    pub select_session_2: String,
    pub select_session_3: String,
    pub select_session_4: String,
    pub select_session_5: String,
    pub select_session_6: String,
    pub select_session_7: String,
    pub select_session_8: String,
    pub select_session_9: String,
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
            previous_transcript_turn: ShortcutId::PreviousTranscriptTurn.default_combo().into(),
            next_transcript_turn: ShortcutId::NextTranscriptTurn.default_combo().into(),
            search_transcript: default_search_transcript(),
            open_settings: ShortcutId::OpenSettings.default_combo().into(),
            open_spaces_dropdown: ShortcutId::OpenSpacesDropdown.default_combo().into(),
            add_space: ShortcutId::AddSpace.default_combo().into(),
            search_sessions: ShortcutId::SearchThreads.default_combo().into(),
            toggle_sidebar: ShortcutId::ToggleSidebar.default_combo().into(),
            toggle_changes: ShortcutId::ToggleChanges.default_combo().into(),
            toggle_terminal: ShortcutId::ToggleTerminal.default_combo().into(),
            new_terminal_tab: ShortcutId::NewTerminalTab.default_combo().into(),
            close_terminal_tab: ShortcutId::CloseTerminalTab.default_combo().into(),
            select_session_1: ShortcutId::SelectSession1.default_combo().into(),
            select_session_2: ShortcutId::SelectSession2.default_combo().into(),
            select_session_3: ShortcutId::SelectSession3.default_combo().into(),
            select_session_4: ShortcutId::SelectSession4.default_combo().into(),
            select_session_5: ShortcutId::SelectSession5.default_combo().into(),
            select_session_6: ShortcutId::SelectSession6.default_combo().into(),
            select_session_7: ShortcutId::SelectSession7.default_combo().into(),
            select_session_8: ShortcutId::SelectSession8.default_combo().into(),
            select_session_9: ShortcutId::SelectSession9.default_combo().into(),
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
            ShortcutId::PreviousTranscriptTurn => &self.previous_transcript_turn,
            ShortcutId::NextTranscriptTurn => &self.next_transcript_turn,
            ShortcutId::SearchTranscript => &self.search_transcript,
            ShortcutId::OpenSettings => &self.open_settings,
            ShortcutId::OpenSpacesDropdown => &self.open_spaces_dropdown,
            ShortcutId::AddSpace => &self.add_space,
            ShortcutId::SearchThreads => &self.search_sessions,
            ShortcutId::ToggleSidebar => &self.toggle_sidebar,
            ShortcutId::ToggleChanges => &self.toggle_changes,
            ShortcutId::ToggleTerminal => &self.toggle_terminal,
            ShortcutId::NewTerminalTab => &self.new_terminal_tab,
            ShortcutId::CloseTerminalTab => &self.close_terminal_tab,
            ShortcutId::SelectSession1 => &self.select_session_1,
            ShortcutId::SelectSession2 => &self.select_session_2,
            ShortcutId::SelectSession3 => &self.select_session_3,
            ShortcutId::SelectSession4 => &self.select_session_4,
            ShortcutId::SelectSession5 => &self.select_session_5,
            ShortcutId::SelectSession6 => &self.select_session_6,
            ShortcutId::SelectSession7 => &self.select_session_7,
            ShortcutId::SelectSession8 => &self.select_session_8,
            ShortcutId::SelectSession9 => &self.select_session_9,
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
            ShortcutId::PreviousTranscriptTurn => self.previous_transcript_turn = combo,
            ShortcutId::NextTranscriptTurn => self.next_transcript_turn = combo,
            ShortcutId::SearchTranscript => self.search_transcript = combo,
            ShortcutId::OpenSettings => self.open_settings = combo,
            ShortcutId::OpenSpacesDropdown => self.open_spaces_dropdown = combo,
            ShortcutId::AddSpace => self.add_space = combo,
            ShortcutId::SearchThreads => self.search_sessions = combo,
            ShortcutId::ToggleSidebar => self.toggle_sidebar = combo,
            ShortcutId::ToggleChanges => self.toggle_changes = combo,
            ShortcutId::ToggleTerminal => self.toggle_terminal = combo,
            ShortcutId::NewTerminalTab => self.new_terminal_tab = combo,
            ShortcutId::CloseTerminalTab => self.close_terminal_tab = combo,
            ShortcutId::SelectSession1 => self.select_session_1 = combo,
            ShortcutId::SelectSession2 => self.select_session_2 = combo,
            ShortcutId::SelectSession3 => self.select_session_3 = combo,
            ShortcutId::SelectSession4 => self.select_session_4 = combo,
            ShortcutId::SelectSession5 => self.select_session_5 = combo,
            ShortcutId::SelectSession6 => self.select_session_6 = combo,
            ShortcutId::SelectSession7 => self.select_session_7 = combo,
            ShortcutId::SelectSession8 => self.select_session_8 = combo,
            ShortcutId::SelectSession9 => self.select_session_9 = combo,
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

/// Hotkey ids whose combinations collide with another action.
pub fn conflicted_shortcuts(keymap: &KeymapConfig) -> Vec<ShortcutId> {
    let ids = ShortcutId::all();
    ids.iter()
        .copied()
        .filter(|&id| {
            let combo = keymap.get(id);
            !combo.is_empty()
                && ids
                    .iter()
                    .copied()
                    .any(|other| other != id && keymap.get(other) == combo)
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
        self
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
            last_space_id: Some("space-1".into()),
            space_filter: Some("space-1".into()),
            scope_navigation: std::collections::HashMap::from([(
                "account".into(),
                ScopeNavigation {
                    last_space_id: Some("space-1".into()),
                    space_filter: Some("space-1".into()),
                },
            )]),
            system_notifications_enabled: true,
            right_pane_width: 700.0,
            terminal_height: 320.0,
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
        };
        settings.save(dir.path()).unwrap();
        assert_eq!(UiSettings::load(dir.path()), settings);
    }

    #[test]
    fn missing_and_corrupt_files_yield_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(UiSettings::load(dir.path()), UiSettings::default());
        std::fs::write(UiSettings::path(dir.path()), "{not json").unwrap();
        assert_eq!(UiSettings::load(dir.path()), UiSettings::default());
    }

    #[test]
    fn missing_hotkeys_use_defaults_and_preserve_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = UiSettings::default();
        settings.keymap.toggle_sidebar = "mod-shift-s".into();
        settings.save(dir.path()).unwrap();

        let path = UiSettings::path(dir.path());
        let mut json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let keymap = json["keymap"].as_object_mut().unwrap();
        for field in [
            "searchTranscript",
            "selectSession1",
            "selectSession2",
            "selectSession3",
            "selectSession4",
            "selectSession5",
            "selectSession6",
            "selectSession7",
            "selectSession8",
            "selectSession9",
        ] {
            keymap.remove(field);
        }
        std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();

        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.keymap.search_transcript, "mod-f");
        for id in ShortcutId::SESSION_SELECTION {
            assert_eq!(loaded.keymap.get(id), id.default_combo());
        }
        assert_eq!(loaded.keymap.toggle_sidebar, "mod-shift-s");
    }

    #[test]
    fn loaded_values_are_clamped() {
        let dir = tempfile::tempdir().unwrap();
        UiSettings {
            sidebar_width: 10_000.0,
            right_pane_width: 1.0,
            ..Default::default()
        }
        .save(dir.path())
        .unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.sidebar_width, SIDEBAR_MAX);
        assert_eq!(loaded.right_pane_width, RIGHT_PANE_MIN);
    }

    #[test]
    fn persisted_font_sizes_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let expected = UiSettings::default();
        expected.save(dir.path()).unwrap();

        let path = UiSettings::path(dir.path());
        let mut json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let object = json.as_object_mut().unwrap();
        object.insert("fontSizeInterface".into(), 20.into());
        object.insert("fontSizePrompt".into(), 20.into());
        object.insert("fontSizeCode".into(), 18.into());
        object.insert("fontSizeTerminal".into(), 20.into());
        std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();

        assert_eq!(UiSettings::load(dir.path()), expected);
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
        assert!(!d.sidebar_collapsed);
        assert_eq!(d.ui_font, crate::theme::DEFAULT_UI_FONT);
        assert_eq!(d.prompt_font, crate::theme::DEFAULT_UI_FONT);
        assert_eq!(d.code_font, crate::theme::DEFAULT_CODE_FONT);
        assert_eq!(d.terminal_font, crate::theme::DEFAULT_CODE_FONT);
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
        assert_eq!(keymap.get(ShortcutId::SelectSession9), "mod-9");
        assert_eq!(
            keymap.get(ShortcutId::PreviousTranscriptTurn),
            "mod-shift-up"
        );
        assert_eq!(keymap.get(ShortcutId::NextTranscriptTurn), "mod-shift-down");
        assert_eq!(keymap.get(ShortcutId::SearchTranscript), "mod-f");
        assert_eq!(keymap.get(ShortcutId::OpenSettings), "mod-,");
        assert_eq!(keymap.get(ShortcutId::OpenSpacesDropdown), "mod-shift-k");
        assert_eq!(keymap.get(ShortcutId::AddSpace), "mod-k");
        assert_eq!(keymap.get(ShortcutId::SearchThreads), "mod-shift-f");
        assert_eq!(keymap.get(ShortcutId::ToggleSidebar), "mod-e");
        assert_eq!(keymap.get(ShortcutId::ToggleChanges), "mod-b");
        assert_eq!(keymap.get(ShortcutId::ToggleTerminal), "mod-`");
        assert_eq!(keymap.get(ShortcutId::NewTerminalTab), "mod-t");
        assert_eq!(keymap.get(ShortcutId::CloseTerminalTab), "mod-shift-w");
        keymap.set(ShortcutId::ClearInput, "mod-shift-c".into());
        assert_eq!(keymap.get(ShortcutId::ClearInput), "mod-shift-c");
        keymap.reset(ShortcutId::ClearInput);
        assert_eq!(keymap.get(ShortcutId::ClearInput), "mod-c");
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
        assert!(!conflicts.contains(&ShortcutId::SearchThreads));
        assert!(!conflicts.contains(&ShortcutId::OpenSpacesDropdown));
        assert!(!conflicts.contains(&ShortcutId::OpenSettings));
        assert!(!conflicts.contains(&ShortcutId::NewSession));
        assert!(!conflicts.contains(&ShortcutId::ClearInput));
        assert!(!conflicts.contains(&ShortcutId::SelectSession1));
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
    fn terminal_height_clamps_on_load() {
        let dir = tempfile::tempdir().unwrap();
        UiSettings {
            terminal_height: 5.0,
            ..Default::default()
        }
        .save(dir.path())
        .unwrap();
        assert_eq!(
            UiSettings::load(dir.path()).terminal_height,
            TERMINAL_MIN_HEIGHT
        );
        UiSettings {
            terminal_height: 99_999.0,
            ..Default::default()
        }
        .save(dir.path())
        .unwrap();
        assert_eq!(
            UiSettings::load(dir.path()).terminal_height,
            TERMINAL_ABS_MAX_HEIGHT
        );
    }
}
