//! The app shell: sidebar column, main panel, optional right "Changes" pane,
//! boot splash, and connection gate.
//!
//! Layout: collapsible drag-resizable sidebar (208–400px, default
//! 256) with a 200ms ease-out width transition; main panel with an h-11 header,
//! content outlet, and a reserved h-6 status strip so later content never
//! shifts; right pane scaffold (360–760px, default 520), hidden by default.
//! Widths/collapsed state persist to `ui-settings.json` (debounced).
//!
//! Resize handles use gpui's drag-and-drop pattern (an `on_drag` with an empty
//! ghost view + `on_drag_move::<Marker>` on the root), the same idiom as Zed's
//! dock. Double-clicking a handle resets that pane to its default width.

use std::path::PathBuf;
use std::time::Duration;

use chrono::Utc;
use gpui::{
    AnyElement, App, Context, Empty, Entity, FocusHandle, Focusable as _, IntoElement, KeyBinding,
    Keystroke, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Point, Render, SharedString,
    Subscription, Task, UniformListScrollHandle, Window, WindowControlArea, actions, div,
    prelude::*, px, uniform_list,
};

use gpui_tokio::Tokio;
use jolt_proto::{HarnessId, UsageBreakdown, UsageBreakdownRow, UsageDay};
use jolt_rpc::methods;

use crate::archived::ArchivedPage;
use crate::changes::{Changes, ChangesEvent};
use crate::composer::{Composer, ComposerEvent, ComposerInput, ComposerInputEvent};
#[cfg(any(debug_assertions, feature = "debug-ui"))]
use crate::debug::{PerformanceHud, TogglePerformanceHud};
use crate::icons::{self, icon};
use crate::loaders;
use crate::motion::{self, AnimationExt as _, MotionSpec, RESIZE, SPLASH_OUT};
use crate::popover::{self, Loadable};
use crate::rail;
use crate::settings::accounts::AccountsPage;
use crate::settings::appearance::AppearancePage;
use crate::settings::devices::DevicesPage;
use crate::settings::hotkeys::{HotkeysEvent, HotkeysPage};
use crate::settings::notifications::{NotificationsEvent, NotificationsPage};
use crate::settings::secrets::SecretsPage;
use crate::settings::terminal::{TerminalPage, TerminalSettingsEvent};
use crate::settings::vcs::VcsPage;
use crate::settings::{
    KeymapConfig, RIGHT_PANE_DEFAULT, RIGHT_PANE_MAX, RIGHT_PANE_MIN, SAVE_DEBOUNCE_MS,
    SIDEBAR_DEFAULT, SIDEBAR_MAX, SIDEBAR_MIN, ScopeNavigation, ShortcutId,
    TERMINAL_DEFAULT_HEIGHT, UiSettings, platform_combo,
};
use crate::state::{
    AppState, ConnectionStatus, EngineBootConfig, GatePhase, Indicator, format_time_ago,
};
use crate::terminal::panel::{
    CloseTerminalTab, NewTerminalTab, TerminalPanel, TerminalPanelEvent, ToggleTerminal,
    clamp_terminal_height,
};
use crate::theme::Theme;
use crate::toast::{Toast, ToastAction, ToastKind};
use crate::transcript::{self, Transcript, TranscriptEvent};

mod spaces;
mod tabs;

use spaces::{AddSpaceFlow, RenameSpaceDialog, SessionSearchFlow, SpacesMenu};

actions!(
    shell,
    [
        NewSession,
        ClearInput,
        CloseCurrentTab,
        PreviousTranscriptTurn,
        NextTranscriptTurn,
        OpenSettings,
        OpenSpacesDropdown,
        ToggleSidebar,
        ToggleChanges,
        AddSpacePalette,
        SearchSessionsPalette
    ]
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, gpui::Action)]
#[action(namespace = shell, no_json, no_register)]
struct SelectTab(usize);

// ---------------------------------------------------------------------------
// Traffic-light-aware titlebar layout
// ---------------------------------------------------------------------------

/// Where the top-left window-control cluster starts, in px from the window's
/// left edge. The
/// frameless hiddenInset chrome puts the macOS traffic lights at {14,15};
/// fullscreen hides them and the cluster reclaims the inset.
pub fn titlebar_cluster_start(fullscreen: bool) -> f32 {
    if fullscreen { 12.0 } else { 88.0 }
}

/// Width of the spacer ahead of the control cluster for a strip that already
/// carries `container_pad` px of its own left padding. macOS only — on
/// Linux/Windows there are no traffic lights and the cluster hugs the edge.
pub fn titlebar_spacer_width(is_macos: bool, fullscreen: bool, container_pad: f32) -> f32 {
    if !is_macos {
        return 0.0;
    }
    (titlebar_cluster_start(fullscreen) - container_pad).max(0.0)
}

/// Width of the persistent top-left button cluster itself (sidebar toggle +
/// back/forward: three 24px buttons, 2px gaps).
pub const CLUSTER_BUTTONS_WIDTH: f32 = 24.0 * 3.0 + 2.0 * 2.0;

/// Where the cluster's first button starts, from the window's left edge.
pub fn cluster_buttons_start(is_macos: bool, fullscreen: bool) -> f32 {
    if is_macos {
        titlebar_cluster_start(fullscreen)
    } else {
        10.0
    }
}

/// Left clearance a full-bleed header (collapsed sidebar) needs so its content
/// starts past the overlay cluster, given the header's own `container_pad`.
pub fn cluster_clearance(is_macos: bool, fullscreen: bool, container_pad: f32) -> f32 {
    (cluster_buttons_start(is_macos, fullscreen) + CLUSTER_BUTTONS_WIDTH + 8.0 - container_pad)
        .max(0.0)
}

/// (Re-)apply the whole app keymap: clears every binding, restores the composer
/// map, then binds every customizable app hotkey from `keymap`. Invalid
/// persisted combinations fall back to that hotkey's default.
pub fn apply_keymap(cx: &mut App, keymap: &KeymapConfig) {
    fn valid_or_default(combo: &str, fallback: &str) -> String {
        let candidate = platform_combo(combo);
        if Keystroke::parse(&candidate).is_ok() {
            candidate
        } else {
            tracing::warn!(%combo, "unparseable shortcut combo; using default");
            platform_combo(fallback)
        }
    }
    cx.clear_key_bindings();
    crate::composer::init(cx);
    // App-menu hotkeys back the native menu key equivalents and must survive
    // keymap re-application.
    crate::app_menus::bind_keys(cx, keymap);
    cx.bind_keys([
        KeyBinding::new(
            &valid_or_default(&keymap.new_session, ShortcutId::NewSession.default_combo()),
            NewSession,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.clear_input, ShortcutId::ClearInput.default_combo()),
            ClearInput,
            Some("Composer"),
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.close_tab, ShortcutId::CloseTab.default_combo()),
            CloseCurrentTab,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(
                &keymap.previous_transcript_turn,
                ShortcutId::PreviousTranscriptTurn.default_combo(),
            ),
            PreviousTranscriptTurn,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(
                &keymap.next_transcript_turn,
                ShortcutId::NextTranscriptTurn.default_combo(),
            ),
            NextTranscriptTurn,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(
                &keymap.open_settings,
                ShortcutId::OpenSettings.default_combo(),
            ),
            OpenSettings,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(
                &keymap.open_spaces_dropdown,
                ShortcutId::OpenSpacesDropdown.default_combo(),
            ),
            OpenSpacesDropdown,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(
                &keymap.toggle_sidebar,
                ShortcutId::ToggleSidebar.default_combo(),
            ),
            ToggleSidebar,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(
                &keymap.toggle_changes,
                ShortcutId::ToggleChanges.default_combo(),
            ),
            ToggleChanges,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(
                &keymap.toggle_terminal,
                ShortcutId::ToggleTerminal.default_combo(),
            ),
            ToggleTerminal,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(
                &keymap.new_terminal_tab,
                ShortcutId::NewTerminalTab.default_combo(),
            ),
            NewTerminalTab,
            Some("Terminal"),
        ),
        KeyBinding::new(
            &valid_or_default(
                &keymap.close_terminal_tab,
                ShortcutId::CloseTerminalTab.default_combo(),
            ),
            CloseTerminalTab,
            Some("Terminal"),
        ),
        // The add-space command center reflects this customizable binding in
        // its leading hotkey chip; pressing it again dismisses.
        KeyBinding::new(
            &valid_or_default(&keymap.add_space, ShortcutId::AddSpace.default_combo()),
            AddSpacePalette,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(
                &keymap.search_sessions,
                ShortcutId::SearchSessions.default_combo(),
            ),
            SearchSessionsPalette,
            None,
        ),
    ]);
    cx.bind_keys(tab_key_bindings(keymap));
    #[cfg(any(debug_assertions, feature = "debug-ui"))]
    cx.bind_keys([KeyBinding::new(
        &valid_or_default(
            keymap.get(ShortcutId::PerformanceHud),
            ShortcutId::PerformanceHud.default_combo(),
        ),
        TogglePerformanceHud,
        None,
    )]);
}

/// Customizable hotkeys for selecting open tabs. Cmd-9 targets the last tab.
fn tab_key_bindings(keymap: &KeymapConfig) -> Vec<KeyBinding> {
    ShortcutId::TAB_SELECTION
        .into_iter()
        .enumerate()
        .map(|(index, id)| {
            let combo = platform_combo(keymap.get(id));
            let combo = if Keystroke::parse(&combo).is_ok() {
                combo
            } else {
                platform_combo(id.default_combo())
            };
            KeyBinding::new(&combo, SelectTab(index), None)
        })
        .collect()
}

/// The settings sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Devices,
    Agents,
    Secrets,
    VersionControl,
    Terminal,
    Appearance,
    Notifications,
    Hotkeys,
}

impl SettingsSection {
    pub const ALL: [SettingsSection; 8] = [
        SettingsSection::Devices,
        SettingsSection::Agents,
        SettingsSection::Secrets,
        SettingsSection::VersionControl,
        SettingsSection::Terminal,
        SettingsSection::Appearance,
        SettingsSection::Notifications,
        SettingsSection::Hotkeys,
    ];

    /// Label shared by the settings sidebar and header.
    pub fn label(self) -> &'static str {
        match self {
            SettingsSection::Devices => "Devices",
            SettingsSection::Agents => "Accounts",
            SettingsSection::Secrets => "Secrets",
            SettingsSection::VersionControl => "Version control",
            SettingsSection::Terminal => "Terminal",
            SettingsSection::Appearance => "Appearance",
            SettingsSection::Notifications => "Notifications",
            SettingsSection::Hotkeys => "Hotkeys",
        }
    }
}

/// What the main outlet shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Chat,
    Archived,
    Settings(SettingsSection),
}

/// Per-chat panel open flags. The terminal and changes panels open per session
/// in memory only; heights and every other persisted setting stay global.
/// New or unknown chats default to closed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChatPanels {
    pub terminal_open: bool,
    pub changes_open: bool,
}

/// The session-scoped panel map. Keys are chat ids; the new-chat canvas uses
/// the empty key. Not persisted — a fresh app starts with everything closed.
#[derive(Debug, Default)]
pub struct SessionPanels {
    map: std::collections::HashMap<String, ChatPanels>,
}

impl SessionPanels {
    pub fn get(&self, key: &str) -> ChatPanels {
        self.map.get(key).copied().unwrap_or_default()
    }

    /// Flip the terminal flag for `key`; returns the new value.
    pub fn toggle_terminal(&mut self, key: &str) -> bool {
        let entry = self.map.entry(key.to_string()).or_default();
        entry.terminal_open = !entry.terminal_open;
        entry.terminal_open
    }

    /// Close the terminal for `key`; returns whether it had been open.
    pub fn close_terminal(&mut self, key: &str) -> bool {
        let entry = self.map.entry(key.to_string()).or_default();
        std::mem::replace(&mut entry.terminal_open, false)
    }

    /// Flip the changes flag for `key`; returns the new value.
    pub fn toggle_changes(&mut self, key: &str) -> bool {
        let entry = self.map.entry(key.to_string()).or_default();
        entry.changes_open = !entry.changes_open;
        entry.changes_open
    }

    pub fn open_changes(&mut self, key: &str) {
        self.map.entry(key.to_string()).or_default().changes_open = true;
    }
}

/// One browser-style route-history entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavEntry {
    /// A chat route; the id of the selected chat ("" = the new-chat canvas).
    Chat(String),
    Archived,
    Settings(SettingsSection),
}

/// Browser-style navigation history for the titlebar back/forward buttons:
/// every route change pushes an entry;
/// Back/Forward walk the stack without changing it; pushing while behind the
/// tip truncates the entries ahead (a new branch, exactly like a browser).
#[derive(Debug)]
pub struct NavHistory {
    entries: Vec<NavEntry>,
    index: usize,
}

impl NavHistory {
    pub fn new(initial: NavEntry) -> Self {
        Self {
            entries: vec![initial],
            index: 0,
        }
    }

    pub fn current(&self) -> &NavEntry {
        &self.entries[self.index]
    }

    /// Record a route change. Re-navigating to the current route is a no-op
    /// (selecting the already-selected chat never happened as a navigation);
    /// otherwise any forward branch is truncated and the entry appended.
    pub fn push(&mut self, entry: NavEntry) {
        if *self.current() == entry {
            return;
        }
        self.entries.truncate(self.index + 1);
        self.entries.push(entry);
        self.index += 1;
    }

    /// Swap the current entry in place without growing the stack so a boot
    /// redirect into the last-used chat leaves no dead Back target behind.
    pub fn replace(&mut self, entry: NavEntry) {
        self.entries[self.index] = entry;
    }

    pub fn can_back(&self) -> bool {
        self.index > 0
    }

    /// Memory history keeps every entry, so "behind the last entry" means
    /// forward navigation is available.
    pub fn can_forward(&self) -> bool {
        self.index + 1 < self.entries.len()
    }

    pub fn back(&mut self) -> Option<NavEntry> {
        if !self.can_back() {
            return None;
        }
        self.index -= 1;
        Some(self.current().clone())
    }

    pub fn forward(&mut self) -> Option<NavEntry> {
        if !self.can_forward() {
            return None;
        }
        self.index += 1;
        Some(self.current().clone())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Sidebar resort glide: 260ms
/// `cubic-bezier(0.22,1,0.36,1)` per-row translate, the View Transitions
/// equivalent.
pub const RESORT: MotionSpec = MotionSpec::new(260, motion::EASE_RESORT);

/// FLIP diff for a keyed list: given the previously rendered order and the new
/// order (key + row height), return each surviving key's paint-only start
/// offset `old_y - new_y` (only keys whose position actually moved). `gap` is
/// the flex gap between rows. Pure — drives the sidebar resort glide.
pub fn resort_offsets(
    old: &[(String, f32)],
    new: &[(String, f32)],
    gap: f32,
) -> std::collections::HashMap<String, f32> {
    let mut old_y = std::collections::HashMap::new();
    let mut y = 0.0_f32;
    for (key, height) in old {
        old_y.insert(key.as_str(), y);
        y += height + gap;
    }
    let mut offsets = std::collections::HashMap::new();
    let mut y = 0.0_f32;
    for (key, height) in new {
        if let Some(prev) = old_y.get(key.as_str()) {
            let dy = prev - y;
            if dy.abs() > 0.5 {
                offsets.insert(key.clone(), dy);
            }
        }
        y += height + gap;
    }
    offsets
}

/// Estimated sidebar row height for the resort diff (title line 17px inside
/// 6px vertical padding + the location subline's 14px line + 2px gap — Active
/// rows always carry the folder · device subline).
/// Session row height (FLIP estimate): space line + title + meta line
/// (harness mark, plus branch for worktrees).
const CHAT_ROW_HEIGHT: f32 = 61.0;
/// Flex gap between sidebar list items.
const SIDEBAR_LIST_GAP: f32 = 2.0;

/// Ramp height of the glass sidebar's scroll-edge fade (the gpui
/// [`gpui::EdgeFade`] scope — per-primitive, so text fades per glyph).
const SIDEBAR_GLASS_FADE_BAND: f32 = 32.0;

/// Drag marker for the sidebar resize handle.
struct SidebarResize;
/// Drag marker for the right-pane resize handle.
struct RightPaneResize;
/// Drag marker for the terminal-panel height handle.
struct TerminalResize;

/// Invisible drag ghost — resize drags render nothing at the cursor.
struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// A oneshot width tween (200ms ease-out), driven MANUALLY from render via
/// [`Shell::eval_tween`] — never through a `with_animation` wrapper. gpui keys
/// an animation element's start time by its full global element-id path, so a
/// wrapper that mounts/remounts (route swap, or an ancestor animation keyed by
/// a fresh epoch) silently REPLAYS the tween from t=0. Manual evaluation keeps
/// the element tree's shape constant: a finished or stale tween is exactly the
/// steady state, no matter how the tree around it remounts (round-6 §1–3).
#[derive(Debug, Clone, Copy)]
struct WidthTween {
    from: f32,
    to: f32,
    started: std::time::Instant,
}

impl WidthTween {
    fn new(from: f32, to: f32) -> Self {
        Self {
            from,
            to,
            started: std::time::Instant::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplashPhase {
    Visible,
    FadingOut,
    Gone,
}

/// The chat-row Rename dialog.
struct RenameChatDialog {
    chat_id: String,
    input: Entity<ComposerInput>,
    /// Focus the input on the dialog's first paint (opened without window access).
    focus_pending: bool,
    _events: Subscription,
}

/// In-app update lifecycle for macOS bundle installs.
enum UpdateFlow {
    Idle,
    Downloading,
    /// Staged bundle ready to swap in.
    Ready(PathBuf),
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum UsageWarningLevel {
    Normal,
    Warning,
    Danger,
}

fn usage_warning_level(fraction: f32) -> UsageWarningLevel {
    match crate::settings::accounts::usage_level(fraction) {
        crate::settings::accounts::UsageLevel::Normal => UsageWarningLevel::Normal,
        crate::settings::accounts::UsageLevel::Warn => UsageWarningLevel::Warning,
        crate::settings::accounts::UsageLevel::Critical => UsageWarningLevel::Danger,
    }
}

/// Transient automatic setup for the user's hidden Personal organization.
struct OrgGateUi {
    submitting: bool,
    error: Option<SharedString>,
    task: Option<Task<()>>,
}

struct BreakdownDialog {
    days: u16,
    data: Loadable<UsageBreakdown>,
    unavailable_devices: usize,
    task: Option<Task<()>>,
}

fn add_reported_cost(total: &mut Option<f64>, value: Option<f64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or_default() + value);
    }
}

fn merge_breakdowns(days: u16, breakdowns: Vec<UsageBreakdown>) -> UsageBreakdown {
    let mut merged = UsageBreakdown {
        days,
        device_id: "all".into(),
        ..UsageBreakdown::default()
    };
    let mut activity: std::collections::BTreeMap<String, UsageDay> =
        std::collections::BTreeMap::new();
    let mut rows: std::collections::HashMap<(HarnessId, String, String), UsageBreakdownRow> =
        std::collections::HashMap::new();
    for breakdown in breakdowns {
        merged.sessions = merged.sessions.saturating_add(breakdown.sessions);
        merged.calls = merged.calls.saturating_add(breakdown.calls);
        merged.input_tokens = merged.input_tokens.saturating_add(breakdown.input_tokens);
        merged.output_tokens = merged.output_tokens.saturating_add(breakdown.output_tokens);
        merged.cache_read_input_tokens = merged
            .cache_read_input_tokens
            .saturating_add(breakdown.cache_read_input_tokens);
        merged.cache_write_input_tokens = merged
            .cache_write_input_tokens
            .saturating_add(breakdown.cache_write_input_tokens);
        add_reported_cost(&mut merged.cost_usd, breakdown.cost_usd);
        for day in breakdown.activity {
            let entry = activity.entry(day.day.clone()).or_insert_with(|| UsageDay {
                day: day.day,
                ..UsageDay::default()
            });
            entry.tokens = entry.tokens.saturating_add(day.tokens);
            entry.calls = entry.calls.saturating_add(day.calls);
            add_reported_cost(&mut entry.cost_usd, day.cost_usd);
        }
        for row in breakdown.rows {
            let key = (row.harness, row.model.clone(), row.cwd.clone());
            let entry = rows.entry(key).or_insert_with(|| UsageBreakdownRow {
                harness: row.harness,
                model: row.model.clone(),
                cwd: row.cwd.clone(),
                sessions: 0,
                calls: 0,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                cost_usd: None,
            });
            entry.sessions = entry.sessions.saturating_add(row.sessions);
            entry.calls = entry.calls.saturating_add(row.calls);
            entry.input_tokens = entry.input_tokens.saturating_add(row.input_tokens);
            entry.output_tokens = entry.output_tokens.saturating_add(row.output_tokens);
            entry.cache_read_input_tokens = entry
                .cache_read_input_tokens
                .saturating_add(row.cache_read_input_tokens);
            entry.cache_write_input_tokens = entry
                .cache_write_input_tokens
                .saturating_add(row.cache_write_input_tokens);
            add_reported_cost(&mut entry.cost_usd, row.cost_usd);
        }
    }
    merged.activity = activity.into_values().collect();
    merged.rows = rows.into_values().collect();
    merged
        .rows
        .sort_by_key(|row| std::cmp::Reverse(row.total_tokens()));
    merged
}

fn compact_number(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn harness_label(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "Claude Code",
        HarnessId::Codex => "Codex",
        HarnessId::Pi => "Pi",
        HarnessId::Mock => "Mock",
    }
}

struct SessionStatusStrip {
    state: Entity<AppState>,
    composer: Entity<Composer>,
}

struct JumpToBottom {
    transcript: Entity<Transcript>,
}

pub struct Shell {
    state: Entity<AppState>,
    transcript: Entity<Transcript>,
    composer: Entity<Composer>,
    status_strip: Entity<SessionStatusStrip>,
    jump_to_bottom: Entity<JumpToBottom>,
    /// External file drag hovering the conversation column — shows the
    /// "Drop images to attach" veil over the whole chat area; a drop stages
    /// the files in the composer.
    file_drag_active: bool,
    /// Lazy panes: no entity (and no RPC) until first opened.
    terminal: Option<Entity<TerminalPanel>>,
    terminal_expanded: bool,
    changes: Option<Entity<Changes>>,
    changes_expanded: bool,
    changes_sub: Option<Subscription>,
    #[cfg(any(debug_assertions, feature = "debug-ui"))]
    performance_hud: Option<Entity<PerformanceHud>>,
    /// Chat outlet vs secondary app pages.
    route: Route,
    /// Route history behind the titlebar back/forward buttons (§ nav history).
    nav: NavHistory,
    devices_page: Option<Entity<DevicesPage>>,
    archived_page: Option<Entity<ArchivedPage>>,
    appearance_page: Option<Entity<AppearancePage>>,
    notifications_page: Option<Entity<NotificationsPage>>,
    hotkeys_page: Option<Entity<HotkeysPage>>,
    accounts_page: Option<Entity<AccountsPage>>,
    secrets_page: Option<Entity<SecretsPage>>,
    vcs_page: Option<Entity<VcsPage>>,
    terminal_page: Option<Entity<TerminalPage>>,
    notifications_sub: Option<Subscription>,
    hotkeys_sub: Option<Subscription>,
    terminal_settings_sub: Option<Subscription>,
    terminal_panel_sub: Option<Subscription>,
    /// Session-row context menu: (chat id, window position).
    chat_menu: Option<(String, Point<Pixels>)>,
    rename_dialog: Option<RenameChatDialog>,
    breakdown_dialog: Option<BreakdownDialog>,
    /// Chat id awaiting delete confirmation.
    delete_confirm: Option<String>,
    /// Space-row context menu: (space id, window position).
    space_menu: Option<(String, Point<Pixels>)>,
    rename_space_dialog: Option<RenameSpaceDialog>,
    /// Space id awaiting delete confirmation (hard delete + session cascade).
    delete_space_confirm: Option<String>,
    /// The add-space command center (device tabs + folder search), `Some`
    /// while open. Its summon shortcut is configurable.
    add_space: Option<AddSpaceFlow>,
    /// Session-title command center, opened from the sidebar or its hotkey.
    session_search: Option<SessionSearchFlow>,
    /// Searchable sidebar space filter, local to this viewport.
    spaces_menu: Option<SpacesMenu>,
    /// Outside-click dismissal guard for the filter trigger.
    spaces_menu_dismissed_at: Option<std::time::Instant>,
    /// Session tab currently hovered (close button appears on hover).
    tab_hover: Option<String>,
    /// Session-tab context menu: (chat id, window position).
    tab_menu: Option<(String, Point<Pixels>)>,
    /// Session-tab drag-reorder in flight (see `tabs::TabDragState`).
    tab_drag: Option<tabs::TabDragState>,
    /// Scroll position of the session tab region (drives the edge fades and
    /// the drop-index math under horizontal overflow).
    tabs_scroll: gpui::ScrollHandle,
    /// Chat id last auto-scrolled into view — scroll-to-selected fires once per
    /// selection change, not every frame (which would fight manual scrolling).
    tabs_scrolled_to: Option<String>,
    /// Scroll position of the sidebar lists region (drives its edge fades).
    sidebar_scroll: UniformListScrollHandle,
    /// `settings.last_space_id` applied once after the first spaces frame.
    space_boot_applied: bool,
    /// Last scope whose navigation snapshot is loaded into `settings`.
    observed_scope: Option<jolt_engine::ScopeKind>,
    /// Highest provider rate-limit warning delivered per account/window. A
    /// reset below the warning threshold allows that window to notify again.
    usage_warning_levels: std::collections::HashMap<String, UsageWarningLevel>,
    user_menu_open: bool,
    /// Outside-click dismissal instant — suppresses the trigger click that
    /// follows the same mouse-down from instantly reopening the menu.
    user_menu_dismissed_at: Option<std::time::Instant>,
    /// Local lifecycle of a macOS bundle update.
    update_flow: UpdateFlow,
    update_task: Option<Task<()>>,
    update_checking: bool,
    update_check_task: Option<Task<()>>,
    /// Latest release already announced during this process.
    notified_update_version: Option<String>,
    /// How this binary was installed — decides the strip's click behavior.
    /// Cached: `detect_install` stats `current_exe` and this renders per frame.
    install: jolt_update::InstallKind,
    org: Option<OrgGateUi>,
    mutate_task: Option<Task<()>>,
    auth_task: Option<Task<()>>,
    /// Kept for the failed-gate "Retry" action.
    boot: EngineBootConfig,
    data_dir: PathBuf,
    settings: UiSettings,
    /// Session-scoped panel open flags for terminal and changes panes. Heights
    /// stay in [`UiSettings`].
    panels: SessionPanels,
    /// The panel key of the chat currently shown ("" = new-chat canvas).
    active_chat: String,
    /// Last rendered sidebar order (key + estimated height) — the FLIP baseline
    /// for the §1.6 resort glide.
    sidebar_prev_order: Vec<(String, f32)>,
    /// Per-key paint offsets of the resort in flight, keyed elements restart on
    /// `resort_epoch` bumps.
    sidebar_resort: std::collections::HashMap<String, f32>,
    /// Keys that just appeared in a live list (fade in, no glide).
    sidebar_new_keys: std::collections::HashSet<String>,
    resort_epoch: usize,
    /// Last observed `window.is_window_active()` — rising edge fires a
    /// ProbeSync so a broadcast-deaf room heals as the user looks at the app.
    was_window_active: bool,
    /// Dev/testing knobs (`JOLT_OPEN_DIALOG`, `JOLT_FORCE_GATE`) — see
    /// [`Shell::new`].
    debug_dialog: Option<String>,
    debug_gate: Option<GatePhase>,
    sidebar_tween: Option<WidthTween>,
    right_tween: Option<WidthTween>,
    terminal_tween: Option<WidthTween>,
    /// Last observed `window.is_fullscreen()` (`None` before first paint) —
    /// flips key the traffic-light inset tween.
    fullscreen: Option<bool>,
    /// 200ms ease-out tween of the cluster start on fullscreen toggles.
    titlebar_tween: Option<WidthTween>,
    /// Armed by mouse-down on a titlebar strip; the next mouse-move hands the
    /// drag to the compositor (zed's platform-titlebar pattern).
    titlebar_should_move: bool,
    /// Clears the height tween once it completes (so a closed panel unmounts).
    terminal_tween_task: Option<Task<()>>,
    /// Height-drag anchor: (pointer y, height) at mouse-down on the handle.
    terminal_drag_anchor: Option<(f32, f32)>,
    /// `motion::reduced_motion` snapshot, refreshed at the top of each render
    /// pass so [`Shell::eval_tween`] (called from `&self` render helpers) can
    /// snap without a `cx`.
    reduced_motion: bool,
    /// Set by [`Shell::eval_tween`] when any tween is mid-flight this frame;
    /// render schedules the next animation frame off it.
    motion_active: std::cell::Cell<bool>,
    splash: SplashPhase,
    splash_task: Option<Task<()>>,
    save_task: Option<Task<()>>,
    /// Focus target for secondary pages, which otherwise have no consistently
    /// focusable child to receive route-level keyboard events.
    settings_focus: FocusHandle,
    /// Focus fallback (registered on first paint — [`Shell::new`] has no
    /// window): app hotkeys dispatch through the window focus chain, so
    /// with nothing focused they go dead. Initial focus lands on the composer
    /// and focus lost with no successor routes back to the active screen.
    focus_sub: Option<Subscription>,
    /// 1s heartbeat re-rendering the working indicator (elapsed + flavour word).
    _ticker: Task<()>,
    /// Refreshes Claude and ChatGPT/Codex rate-limit windows in the background.
    _account_usage_task: Task<()>,
    _state_observation: Subscription,
    _composer_events: Subscription,
    _transcript_events: Subscription,
}

fn scope_key(scope: jolt_engine::ScopeKind) -> &'static str {
    match scope {
        jolt_engine::ScopeKind::Local => "local",
        jolt_engine::ScopeKind::Account => "account",
    }
}

impl Shell {
    pub fn new(state: Entity<AppState>, boot: EngineBootConfig, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&state, |this: &mut Shell, state, cx| {
            this.on_state_changed(&state, cx);
            cx.notify();
        });
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        let composer = cx.new(|cx| Composer::new(state.clone(), cx));
        let status_strip = cx.new(|_| SessionStatusStrip {
            state: state.clone(),
            composer: composer.clone(),
        });
        let jump_to_bottom = cx.new(|_| JumpToBottom {
            transcript: transcript.clone(),
        });
        let transcript_events = cx.subscribe(
            &transcript,
            |this: &mut Shell, _, event: &TranscriptEvent, cx| match event {
                TranscriptEvent::OpenTurnDiff { diff, file_path } => {
                    this.open_turn_diff(diff.clone(), file_path.as_deref(), cx);
                }
            },
        );
        // Own-send re-engages the stick-to-bottom pin with a smooth scroll.
        let composer_events = cx.subscribe(&composer, {
            let transcript = transcript.clone();
            move |_: &mut Shell, _, event: &ComposerEvent, cx| match event {
                ComposerEvent::Sent { .. } => {
                    transcript.update(cx, |t, cx| t.on_own_send(cx));
                }
            }
        });
        // Working-indicator heartbeat: notify once a second while a session is
        // live so elapsed time and the flavour word stay fresh.
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |shell: &mut Shell, cx| {
                    let live = {
                        let s = shell.state.read(cx);
                        s.selected_chat
                            .as_deref()
                            .is_some_and(|id| s.indicator_for(id, Utc::now()) != Indicator::None)
                    };
                    if live {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let account_usage_task = cx.spawn(async move |this, cx| {
            loop {
                let Ok(engine) = this.update(cx, |shell: &mut Shell, cx| {
                    shell.state.read(cx).engine().cloned()
                }) else {
                    break;
                };
                let retry_soon = engine.is_none();
                if let Some(engine) = engine {
                    let result = engine
                        .client()
                        .call(
                            methods::LIST_AGENT_ACCOUNTS,
                            serde_json::json!({
                                "forceUsage": true,
                                "usageOnly": true,
                            }),
                        )
                        .await;
                    if let Ok(value) = result
                        && let Ok(snapshot) =
                            serde_json::from_value::<jolt_proto::AgentAccountsSnapshot>(value)
                        && this
                            .update(cx, |shell, cx| {
                                shell.notify_account_usage(&snapshot, cx);
                            })
                            .is_err()
                    {
                        break;
                    }
                }
                cx.background_executor()
                    .timer(Duration::from_secs(if retry_soon { 5 } else { 5 * 60 }))
                    .await;
            }
        });
        let data_dir = boot.data_dir.clone();
        let settings = UiSettings::load(&data_dir);
        // Bind the customizable hotkeys from the persisted keymap.
        apply_keymap(cx, &settings.keymap);
        // Dev/testing knob: `JOLT_OPEN_ROUTE=archived|settings[/<section>]`
        // boots straight into a secondary page — these pages have no deep link
        // and synthetic input can't reach them on headless compositors.
        let route = match std::env::var("JOLT_OPEN_ROUTE").ok().as_deref() {
            Some("settings") | Some("settings/devices") => {
                Route::Settings(SettingsSection::Devices)
            }
            Some("settings/agents") => Route::Settings(SettingsSection::Agents),
            Some("settings/secrets") => Route::Settings(SettingsSection::Secrets),
            Some("settings/vcs") => Route::Settings(SettingsSection::VersionControl),
            Some("settings/terminal") => Route::Settings(SettingsSection::Terminal),
            Some("settings/appearance") => Route::Settings(SettingsSection::Appearance),
            Some("settings/notifications") => Route::Settings(SettingsSection::Notifications),
            Some("settings/hotkeys" | "settings/shortcuts") => {
                Route::Settings(SettingsSection::Hotkeys)
            }
            Some("archived" | "settings/archived") => Route::Archived,
            // `new` pins the new-chat canvas (suppresses boot auto-select).
            Some("new") => {
                state.update(cx, |s, _| s.auto_selected = true);
                Route::Chat
            }
            _ => Route::Chat,
        };
        // More capture knobs of the same kind: `JOLT_OPEN_DIALOG=rename|delete`
        // opens that dialog for the first chat once chats land; `=model` pops
        // the combined harness/model menu once the shell is Ready;
        // `JOLT_FORCE_GATE=signin|org|failed` renders that gate regardless of
        // real auth state (display-only — for styling passes).
        let debug_dialog = std::env::var("JOLT_OPEN_DIALOG").ok();
        let debug_gate = match std::env::var("JOLT_FORCE_GATE").ok().as_deref() {
            Some("signin") => Some(GatePhase::SignIn),
            Some("org") => Some(GatePhase::OrgGate),
            Some("failed") => Some(GatePhase::Failed(
                "Could not reach the Jolt engine on port 27901".into(),
            )),
            _ => None,
        };
        let nav = NavHistory::new(match route {
            Route::Chat => NavEntry::Chat(String::new()),
            Route::Archived => NavEntry::Archived,
            Route::Settings(section) => NavEntry::Settings(section),
        });
        #[cfg(any(debug_assertions, feature = "debug-ui"))]
        let performance_hud =
            crate::debug::performance_hud_requested().then(|| cx.new(PerformanceHud::new));
        Self {
            state,
            transcript,
            composer,
            status_strip,
            jump_to_bottom,
            file_drag_active: false,
            terminal: None,
            terminal_expanded: false,
            changes: None,
            changes_expanded: false,
            changes_sub: None,
            #[cfg(any(debug_assertions, feature = "debug-ui"))]
            performance_hud,
            route,
            nav,
            devices_page: None,
            archived_page: None,
            appearance_page: None,
            notifications_page: None,
            hotkeys_page: None,
            accounts_page: None,
            secrets_page: None,
            vcs_page: None,
            terminal_page: None,
            notifications_sub: None,
            hotkeys_sub: None,
            terminal_settings_sub: None,
            terminal_panel_sub: None,
            chat_menu: None,
            rename_dialog: None,
            breakdown_dialog: None,
            delete_confirm: None,
            space_menu: None,
            rename_space_dialog: None,
            delete_space_confirm: None,
            add_space: None,
            session_search: None,
            spaces_menu: None,
            spaces_menu_dismissed_at: None,
            tab_hover: None,
            tab_menu: None,
            tab_drag: None,
            tabs_scroll: gpui::ScrollHandle::new(),
            tabs_scrolled_to: None,
            sidebar_scroll: UniformListScrollHandle::new(),
            space_boot_applied: false,
            observed_scope: None,
            usage_warning_levels: std::collections::HashMap::new(),
            user_menu_open: false,
            user_menu_dismissed_at: None,
            update_flow: UpdateFlow::Idle,
            update_task: None,
            update_checking: false,
            update_check_task: None,
            notified_update_version: None,
            install: jolt_update::detect_install(),
            org: None,
            mutate_task: None,
            auth_task: None,
            boot,
            data_dir,
            settings,
            panels: SessionPanels::default(),
            active_chat: String::new(),
            sidebar_prev_order: Vec::new(),
            sidebar_resort: std::collections::HashMap::new(),
            sidebar_new_keys: std::collections::HashSet::new(),
            resort_epoch: 0,
            was_window_active: false,
            debug_dialog,
            debug_gate,
            sidebar_tween: None,
            right_tween: None,
            terminal_tween: None,
            fullscreen: None,
            titlebar_tween: None,
            titlebar_should_move: false,
            terminal_tween_task: None,
            terminal_drag_anchor: None,
            reduced_motion: false,
            motion_active: std::cell::Cell::new(false),
            splash: SplashPhase::Visible,
            splash_task: None,
            save_task: None,
            settings_focus: cx.focus_handle(),
            focus_sub: None,
            _ticker: ticker,
            _account_usage_task: account_usage_task,
            _state_observation: observation,
            _composer_events: composer_events,
            _transcript_events: transcript_events,
        }
    }

    // ---- splash ----

    fn sync_scope_navigation(&mut self, state: &Entity<AppState>, cx: &mut Context<Self>) {
        let Some(scope) = state.read(cx).scope.as_ref().map(|status| status.active) else {
            return;
        };
        if self.observed_scope == Some(scope) {
            return;
        }

        let snapshot = ScopeNavigation {
            last_space_id: self.settings.last_space_id.clone(),
            open_tabs: self.settings.open_tabs.clone(),
            active_tab_id: self.settings.active_tab_id.clone(),
            space_filter: self.settings.space_filter.clone(),
        };
        if let Some(previous) = self.observed_scope {
            self.settings
                .scope_navigation
                .insert(scope_key(previous).into(), snapshot);
        } else if self.settings.scope_navigation.is_empty()
            && (snapshot.last_space_id.is_some()
                || snapshot
                    .open_tabs
                    .as_ref()
                    .is_some_and(|tabs| !tabs.is_empty()))
        {
            // Existing account-only installs have no scope map. If Account won
            // splash resolution, assign navigation directly; a Local fallback
            // preserves it as legacy until that account signs in again.
            let key = if scope == jolt_engine::ScopeKind::Account {
                "account"
            } else {
                "legacy-account"
            };
            self.settings.scope_navigation.insert(key.into(), snapshot);
        }

        let target = self
            .settings
            .scope_navigation
            .get(scope_key(scope))
            .cloned()
            .or_else(|| {
                (scope == jolt_engine::ScopeKind::Account)
                    .then(|| {
                        self.settings
                            .scope_navigation
                            .get("legacy-account")
                            .cloned()
                    })
                    .flatten()
            })
            .unwrap_or_default();
        self.settings.last_space_id = target.last_space_id;
        self.settings.open_tabs = target.open_tabs;
        self.settings.active_tab_id = target.active_tab_id;
        self.settings.space_filter = target.space_filter;
        self.observed_scope = Some(scope);
        // Scope-bound views own standing RPC streams; recreate them against the
        // newly routed runtime while leaving both engine runtimes alive.
        self.terminal = None;
        self.terminal_expanded = false;
        self.changes = None;
        self.changes_expanded = false;
        self.changes_sub = None;
        self.devices_page = None;
        self.archived_page = None;
        self.add_space = None;
        self.session_search = None;
        self.space_boot_applied = false;
        self.tabs_scrolled_to = None;
        self.schedule_save(cx);
    }

    fn on_state_changed(&mut self, state: &Entity<AppState>, cx: &mut Context<Self>) {
        self.sync_scope_navigation(state, cx);
        if !matches!(
            state.read(cx).auth.as_ref(),
            Some(jolt_proto::AuthState::NeedsOrganization { .. })
        ) {
            self.org = None;
        }
        // Capture knob: the add-space palette needs only the device registry.
        if self.debug_dialog.as_deref() == Some("add-space") && !state.read(cx).devices.is_empty() {
            self.debug_dialog = None;
            self.open_add_space(cx);
        }
        // Capture knob: pop the requested dialog once chats have landed.
        if let Some(which) = self.debug_dialog.clone()
            && let Some(first) = state.read(cx).chats.first().map(|c| c.id.clone())
        {
            self.debug_dialog = None;
            match which.as_str() {
                "rename" => self.open_rename_chat(first, cx),
                "delete" => {
                    self.delete_confirm = Some(first);
                }
                _ => {}
            }
        }
        self.notify_jolt_update(state, cx);
        // Boot: restore the last selected space once the first spaces frame
        // lands (a still-existing row wins over the auto-selected first one;
        // the boot-auto-selected chat's own space wins over both — selecting a
        // chat implies its space, which `select_chat` already applied).
        if !self.space_boot_applied && !state.read(cx).spaces.is_empty() {
            self.space_boot_applied = true;
            if state.read(cx).selected_chat.is_none()
                && let Some(last) = self.settings.last_space_id.clone()
                && state.read(cx).space_row(&last).is_some()
            {
                state.update(cx, |s, cx| s.select_space(Some(last), cx));
            }
        }
        // Persist the active space for new-session fallback, then reconcile
        // device-local tabs only after the first registry frames land.
        {
            let selected_space = state.read(cx).selected_space.clone();
            if selected_space != self.settings.last_space_id && selected_space.is_some() {
                self.settings.last_space_id = selected_space;
                self.schedule_save(cx);
            }
        }
        self.sync_open_tabs(cx);
        if state.read(cx).spaces_synced
            && let Some(filter) = self.settings.space_filter.clone()
            && state.read(cx).space_row(&filter).is_none()
        {
            self.settings.space_filter = None;
            self.schedule_save(cx);
        }
        // Chat switch: restore THAT chat's panel state (per-session open flags;
        // snap, no tween — the panels belong to the destination chat).
        let selected = state.read(cx).selected_chat.clone().unwrap_or_default();
        if selected != self.active_chat {
            self.active_chat = selected;
            // Route history: a chat switch is a navigation. The very first
            // selection off the untouched boot canvas REPLACES that entry —
            // The boot route redirects into the last-used chat, leaving no
            // dead Back target. Walking history lands here too, but the
            // destination already equals `current()`, so the push dedups.
            if matches!(self.route, Route::Chat) {
                let entry = NavEntry::Chat(self.active_chat.clone());
                if self.nav.len() == 1 && *self.nav.current() == NavEntry::Chat(String::new()) {
                    self.nav.replace(entry);
                } else {
                    self.nav.push(entry);
                }
            }
            self.right_tween = None;
            self.terminal_tween = None;
            self.set_changes_expanded(false, cx);
            self.set_terminal_expanded(false, cx);
            let panels = self.panels.get(&self.panel_key(cx));
            if let Some(panel) = self.terminal.clone() {
                panel.update(cx, |panel, cx| panel.set_open(panels.terminal_open, cx));
            }
            if panels.changes_open {
                self.changes_pane(cx).update(cx, Changes::collapse_all);
            }
            self.sync_changes_watch(cx);
        }
        match state.read(cx).connection {
            ConnectionStatus::Ready => {
                if self.splash == SplashPhase::Visible {
                    self.splash = SplashPhase::FadingOut;
                    self.splash_task = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(SPLASH_OUT.total() + Duration::from_millis(30))
                            .await;
                        this.update(cx, |shell, cx| {
                            shell.splash = SplashPhase::Gone;
                            cx.notify();
                        })
                        .ok();
                    }));
                }
            }
            // Reveal the gate card immediately; the splash never returns mid-session.
            ConnectionStatus::Failed(_) => self.splash = SplashPhase::Gone,
            ConnectionStatus::Connecting => {}
        }
    }

    fn notify_account_usage(
        &mut self,
        snapshot: &jolt_proto::AgentAccountsSnapshot,
        cx: &mut Context<Self>,
    ) {
        for account in snapshot.accounts.iter().filter(|account| {
            account.active && matches!(account.harness, HarnessId::ClaudeCode | HarnessId::Codex)
        }) {
            let (provider, product) = match account.harness {
                HarnessId::ClaudeCode => ("Claude Code", "Claude"),
                HarnessId::Codex => ("Codex", "ChatGPT"),
                _ => continue,
            };
            let account_label = account
                .email
                .as_deref()
                .or(account.display_name.as_deref())
                .unwrap_or("account");
            for (index, window) in account.usage_windows.iter().enumerate() {
                let key = format!("{provider}:{}:{index}:{}", account.id, window.label);
                let level = usage_warning_level(window.used_fraction);
                let previous = self
                    .usage_warning_levels
                    .insert(key.clone(), level)
                    .unwrap_or(UsageWarningLevel::Normal);
                if level <= previous || level == UsageWarningLevel::Normal {
                    continue;
                }
                let (threshold, title, kind) = match level {
                    UsageWarningLevel::Warning => {
                        (80, format!("{provider} usage is high"), ToastKind::Warning)
                    }
                    UsageWarningLevel::Danger => (
                        95,
                        format!("{provider} usage is nearly exhausted"),
                        ToastKind::Error,
                    ),
                    UsageWarningLevel::Normal => continue,
                };
                let percent = (window.used_fraction * 100.0).round() as u32;
                let reset = crate::settings::accounts::format_reset(window.resets_at, Utc::now())
                    .map(|reset| format!("; {reset}"))
                    .unwrap_or_default();
                crate::toast::show(
                    Toast::new(format!("account-usage-{key}-{threshold}"), title, kind).body(
                        format!(
                            "{product} {} limit for {account_label} is {percent}% used{reset}.",
                            window.label.to_lowercase()
                        ),
                    ),
                    cx,
                );
            }
        }
    }

    fn notify_jolt_update(&mut self, state: &Entity<AppState>, cx: &mut Context<Self>) {
        let Some(latest) = state
            .read(cx)
            .update
            .as_ref()
            .filter(|status| status.update_available)
            .and_then(|status| status.latest_version.clone())
        else {
            return;
        };
        if self.notified_update_version.as_deref() == Some(latest.as_str()) {
            return;
        }
        self.notified_update_version = Some(latest.clone());
        self.show_jolt_update_available(latest, cx);
    }

    fn show_jolt_update_available(&mut self, latest: String, cx: &mut Context<Self>) {
        let mut toast = Toast::new(
            format!("jolt-update-{latest}"),
            "Jolt update available",
            ToastKind::Info,
        )
        .persistent();
        if matches!(self.install, jolt_update::InstallKind::MacApp { .. }) {
            let shell = cx.entity().downgrade();
            toast = toast
                .body(format!("Version {latest} is ready to download."))
                .action(ToastAction::new("Download", move |cx| {
                    shell
                        .update(cx, |shell, cx| shell.begin_update_download(cx))
                        .ok();
                }));
        } else {
            toast = toast.body(format!(
                "Version {latest} is available. Run `jolt update` to install it."
            ));
        }
        crate::toast::show(toast, cx);
    }

    // ---- layout state ----

    fn sidebar_target(&self) -> f32 {
        if self.settings.sidebar_collapsed {
            0.0
        } else {
            self.settings.sidebar_width
        }
    }

    /// Does the selected space's folder have git? Owner-stamped and synced —
    /// gates the Changes pane, its toggle, and Cmd-B with zero RPCs.
    fn space_git_detected(&self, cx: &App) -> bool {
        self.state.read(cx).selected_space_git()
    }

    /// The current chat's changes-pane flag (per-session, in-memory), gated on
    /// the space having git at all: a stale per-chat open flag must not reopen
    /// the pane after switching into a non-git space.
    /// The per-session panel key. The new-chat canvas (no selection) keys per
    /// SPACE — one shared "" key made a canvas toggle read as global state
    /// (user report).
    fn panel_key(&self, cx: &App) -> String {
        if self.active_chat.is_empty() {
            let space = self
                .state
                .read(cx)
                .selected_space
                .clone()
                .unwrap_or_default();
            format!("space-canvas:{space}")
        } else {
            self.active_chat.clone()
        }
    }

    fn right_pane_open(&self, cx: &App) -> bool {
        self.panels.get(&self.panel_key(cx)).changes_open && self.space_git_detected(cx)
    }

    /// The current chat's terminal flag (per-session, in-memory).
    fn terminal_open(&self, cx: &App) -> bool {
        self.panels.get(&self.panel_key(cx)).terminal_open
    }

    fn right_target(&self, cx: &App) -> f32 {
        if self.right_pane_open(cx) {
            self.settings.right_pane_width
        } else {
            0.0
        }
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        let from = self.sidebar_target();
        self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
        self.sidebar_tween = Some(WidthTween::new(from, self.sidebar_target()));
        self.schedule_save(cx);
        cx.notify();
    }

    fn toggle_right_pane(&mut self, cx: &mut Context<Self>) {
        // No git in this space → no diff pane, Cmd-B goes dead.
        if !self.space_git_detected(cx) {
            return;
        }
        let from = self.right_target(cx);
        let key = self.panel_key(cx);
        let open = self.panels.toggle_changes(&key);
        self.right_tween = Some(WidthTween::new(from, self.right_target(cx)));
        if open {
            // Lazy: the Changes entity (and its WatchCheckoutDiffV2) exists only
            // once the pane has been opened.
            let changes = self.changes_pane(cx);
            changes.update(cx, |changes, cx| {
                changes.collapse_all(cx);
                changes.ensure_watch(cx);
            });
        } else if let Some(changes) = self.changes.clone() {
            self.set_changes_expanded(false, cx);
            changes.update(cx, Changes::stop_watch);
        }
        cx.notify();
    }

    fn set_changes_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        if expanded {
            self.set_terminal_expanded(false, cx);
        }
        if self.changes_expanded == expanded {
            return;
        }
        self.changes_expanded = expanded;
        if let Some(changes) = self.changes.clone() {
            changes.update(cx, |changes, cx| changes.set_expanded_view(expanded, cx));
        }
        cx.notify();
    }

    fn open_turn_diff(
        &mut self,
        diff: jolt_proto::TurnDiffManifest,
        file_path: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        if self.state.read(cx).selected_chat.as_deref() != Some(diff.chat_id.as_str()) {
            return;
        }
        let from = self.right_target(cx);
        let key = self.panel_key(cx);
        self.panels.open_changes(&key);
        self.right_tween = Some(WidthTween::new(from, self.right_target(cx)));
        let target = (self.state.read(cx).local_device_id.as_deref()
            != Some(diff.device_id.as_str()))
        .then(|| diff.device_id.clone());
        self.changes_pane(cx).update(cx, |changes, cx| {
            changes.show_turn_diff(diff, target, file_path, cx);
        });
        cx.notify();
    }

    fn changes_pane(&mut self, cx: &mut Context<Self>) -> Entity<Changes> {
        if let Some(changes) = &self.changes {
            return changes.clone();
        }
        let changes = cx.new(|cx| Changes::new(self.state.clone(), cx));
        changes.update(cx, |changes, cx| {
            changes.set_expanded_view(self.changes_expanded, cx)
        });
        self.changes_sub = Some(cx.subscribe(
            &changes,
            |this: &mut Shell, _, event: &ChangesEvent, cx| match event {
                ChangesEvent::ToggleExpanded => {
                    this.set_changes_expanded(!this.changes_expanded, cx)
                }
            },
        ));
        self.changes = Some(changes.clone());
        changes
    }
    fn sync_changes_watch(&mut self, cx: &mut Context<Self>) {
        let on_chat = matches!(self.route, Route::Chat);
        if !on_chat {
            self.set_terminal_expanded(false, cx);
        }
        let visible = on_chat && self.panels.get(&self.panel_key(cx)).changes_open;
        if visible {
            self.changes_pane(cx).update(cx, Changes::ensure_watch);
        } else if let Some(changes) = self.changes.clone() {
            self.set_changes_expanded(false, cx);
            changes.update(cx, Changes::stop_watch);
        }
    }

    fn terminal_panel(&mut self, cx: &mut Context<Self>) -> Entity<TerminalPanel> {
        if let Some(terminal) = &self.terminal {
            return terminal.clone();
        }
        let command = self.settings.terminal_command.clone();
        let terminal = cx.new(|cx| TerminalPanel::new(self.state.clone(), command, cx));
        terminal.update(cx, |terminal, cx| {
            terminal.set_expanded_view(self.terminal_expanded, cx)
        });
        self.terminal_panel_sub = Some(cx.subscribe(
            &terminal,
            |this: &mut Shell, _, event: &TerminalPanelEvent, cx| match event {
                TerminalPanelEvent::ChatEmptied(chat_id) => {
                    this.close_terminal_for_exited_chat(chat_id, cx);
                }
                TerminalPanelEvent::ToggleExpanded => {
                    this.set_terminal_expanded(!this.terminal_expanded, cx);
                }
            },
        ));
        self.terminal = Some(terminal.clone());
        terminal
    }

    fn set_terminal_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        if expanded {
            self.set_changes_expanded(false, cx);
        }
        if self.terminal_expanded == expanded {
            return;
        }
        self.terminal_expanded = expanded;
        if let Some(terminal) = self.terminal.clone() {
            terminal.update(cx, |terminal, cx| terminal.set_expanded_view(expanded, cx));
        }
        cx.notify();
    }

    fn terminal_target(&self, cx: &App) -> f32 {
        if self.terminal_open(cx) {
            self.settings.terminal_height
        } else {
            0.0
        }
    }

    fn close_terminal_for_exited_chat(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let current = self.active_chat == chat_id;
        let from = current.then(|| self.terminal_target(cx));
        if !self.panels.close_terminal(chat_id) {
            return;
        }
        if let Some(from) = from {
            self.set_terminal_expanded(false, cx);
            self.terminal_tween = Some(WidthTween::new(from, 0.0));
            self.schedule_terminal_tween_cleanup(cx);
            cx.notify();
        }
    }

    fn schedule_terminal_tween_cleanup(&mut self, cx: &mut Context<Self>) {
        self.terminal_tween_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(RESIZE.total().mul_f32(motion::speed_scale()) + Duration::from_millis(30))
                .await;
            this.update(cx, |shell, cx| {
                shell.terminal_tween = None;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Cmd/Ctrl+` and the header button. Height animates 200 ms; closing detaches
    /// (PTYs stay alive), opening restores. The flag is per chat.
    fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.terminal_target(cx);
        let key = self.panel_key(cx);
        let open = self.panels.toggle_terminal(&key);
        self.terminal_tween = Some(WidthTween::new(from, self.terminal_target(cx)));
        if !open {
            self.set_terminal_expanded(false, cx);
        }
        let panel = self.terminal_panel(cx);
        panel.update(cx, |panel, cx| panel.set_open(open, cx));
        if open {
            // Opening lands keyboard focus in the shell so typing goes straight
            // to the prompt with no click needed.
            // The handle is focusable before the panel's first paint; once the
            // terminal body mounts with `track_focus` it receives the keys.
            window.focus(&panel.read(cx).focus_handle(), cx);
        } else {
            // Hiding the panel removes the (likely focused) terminal view;
            // with nothing focused, window key bindings stop dispatching, so
            // hand focus to the composer. Cmd+` is a pure toggle, so a second
            // press closes even while the terminal is focused.
            window.focus(&self.composer.focus_handle(cx), cx);
        }
        self.schedule_terminal_tween_cleanup(cx);
        cx.notify();
    }

    fn on_terminal_drag(
        &mut self,
        event: &gpui::DragMoveEvent<TerminalResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((anchor_y, anchor_h)) = self.terminal_drag_anchor else {
            return;
        };
        let dy = anchor_y - f32::from(event.event.position.y);
        let viewport_h = f32::from(window.viewport_size().height);
        self.settings.terminal_height = clamp_terminal_height(anchor_h + dy, viewport_h);
        self.terminal_tween = None; // live drag tracks the pointer
        self.schedule_save(cx);
        cx.notify();
    }

    fn on_sidebar_drag(
        &mut self,
        event: &gpui::DragMoveEvent<SidebarResize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let x = f32::from(event.event.position.x);
        self.settings.sidebar_width = x.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        self.settings.sidebar_collapsed = false;
        self.sidebar_tween = None; // live drag tracks the pointer directly
        self.schedule_save(cx);
        cx.notify();
    }

    fn on_right_pane_drag(
        &mut self,
        event: &gpui::DragMoveEvent<RightPaneResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = f32::from(window.viewport_size().width);
        let width = viewport - f32::from(event.event.position.x);
        // jolt caps the pane at 52% of the window on top of the absolute range.
        let max = RIGHT_PANE_MAX.min(viewport * 0.52);
        self.settings.right_pane_width = width.clamp(RIGHT_PANE_MIN, max.max(RIGHT_PANE_MIN));
        self.right_tween = None;
        self.schedule_save(cx);
        cx.notify();
    }

    /// Debounced settings write: waits [`SAVE_DEBOUNCE_MS`], then persists the
    /// latest snapshot on the background executor. Re-scheduling drops (cancels)
    /// the previous timer.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        let dir = self.data_dir.clone();
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(SAVE_DEBOUNCE_MS))
                .await;
            // Re-stamp the appearance from the global before writing. The View
            // menu changes it through `appearance::set_mode`, which never touches
            // this shell's in-memory copy — without this, the next pane resize
            // would quietly write the boot-time appearance back over the user's
            // choice.
            let Ok(snapshot) = this.update(cx, |shell, cx| {
                shell.settings.appearance = crate::appearance::mode(cx);
                let (light_theme, dark_theme) = crate::appearance::theme_ids(cx);
                shell.settings.light_theme = light_theme;
                shell.settings.dark_theme = dark_theme;
                let (ui_font, prompt_font, code_font, terminal_font) =
                    crate::appearance::font_families(cx);
                shell.settings.ui_font = ui_font.to_string();
                shell.settings.prompt_font = prompt_font.to_string();
                shell.settings.code_font = code_font.to_string();
                shell.settings.terminal_font = terminal_font.to_string();
                let sizes = crate::appearance::font_sizes(cx);
                shell.settings.font_size_interface = sizes.interface;
                shell.settings.font_size_prompt = sizes.prompt;
                shell.settings.font_size_code = sizes.code;
                shell.settings.font_size_terminal = sizes.terminal;
                if let Some(scope) = shell.observed_scope {
                    shell.settings.scope_navigation.insert(
                        scope_key(scope).into(),
                        ScopeNavigation {
                            last_space_id: shell.settings.last_space_id.clone(),
                            open_tabs: shell.settings.open_tabs.clone(),
                            active_tab_id: shell.settings.active_tab_id.clone(),
                            space_filter: shell.settings.space_filter.clone(),
                        },
                    );
                }
                shell.settings.clone()
            }) else {
                return;
            };
            cx.background_executor()
                .spawn(async move {
                    if let Err(err) = snapshot.save(&dir) {
                        tracing::warn!(error = %err, "failed to persist ui settings");
                    }
                })
                .await;
        }));
    }

    fn retry_engine(&mut self, cx: &mut Context<Self>) {
        AppState::bootstrap(self.state.clone(), self.boot.clone(), cx);
    }

    // ---- routes / settings ----

    fn open_settings(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        self.route = Route::Settings(section);
        self.nav.push(NavEntry::Settings(section));
        self.sync_changes_watch(cx);
        self.user_menu_open = false;
        self.chat_menu = None;
        cx.notify();
    }

    fn open_archived(&mut self, cx: &mut Context<Self>) {
        self.route = Route::Archived;
        self.nav.push(NavEntry::Archived);
        self.sync_changes_watch(cx);
        self.user_menu_open = false;
        self.chat_menu = None;
        cx.notify();
    }

    /// Open the unmaterialized new-session tab. Every entry point shares the
    /// same target resolver: sidebar filter, last active space, first space.
    fn open_new_session(&mut self, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.nav.push(NavEntry::Chat(String::new()));
        self.user_menu_open = false;
        self.chat_menu = None;
        let target = {
            let state = self.state.read(cx);
            let valid = |id: &String| state.space_row(id).is_some();
            self.settings
                .space_filter
                .clone()
                .filter(valid)
                .or_else(|| self.settings.last_space_id.clone().filter(valid))
                .or_else(|| state.spaces.first().map(|space| space.id.clone()))
        };
        self.state.update(cx, |state, cx| {
            if target.is_some() {
                state.select_space(target, cx);
            }
            state.select_chat(None, cx);
        });
        self.sync_changes_watch(cx);
        cx.notify();
    }

    fn close_secondary_page(&mut self, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.nav.push(NavEntry::Chat(self.active_chat.clone()));
        self.sync_changes_watch(cx);
        cx.notify();
    }

    // ---- back/forward (route history) ----

    fn navigate_back(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.nav.back() {
            self.apply_nav(entry, cx);
        }
    }

    fn navigate_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.nav.forward() {
            self.apply_nav(entry, cx);
        }
    }

    /// Land on a history entry WITHOUT recording a new one: the stack already
    /// points at `entry` (back/forward moved the index); the selection change
    /// this triggers dedups against `current()` in [`Self::on_state_changed`].
    fn apply_nav(&mut self, entry: NavEntry, cx: &mut Context<Self>) {
        match entry {
            NavEntry::Chat(chat_id) => {
                self.route = Route::Chat;
                let target = (!chat_id.is_empty()).then_some(chat_id);
                if self.state.read(cx).selected_chat != target {
                    self.state.update(cx, |s, cx| s.select_chat(target, cx));
                }
            }
            NavEntry::Archived => {
                self.route = Route::Archived;
            }
            NavEntry::Settings(section) => {
                self.route = Route::Settings(section);
            }
        }
        self.sync_changes_watch(cx);
        self.user_menu_open = false;
        self.chat_menu = None;
        cx.notify();
    }

    /// Lazily create the entity for a settings section and return it renderable.
    fn settings_outlet(&mut self, section: SettingsSection, cx: &mut Context<Self>) -> AnyElement {
        match section {
            SettingsSection::Devices => {
                if self.devices_page.is_none() {
                    let state = self.state.clone();
                    self.devices_page = Some(cx.new(|cx| DevicesPage::new(state, cx)));
                }
                match &self.devices_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Agents => {
                if self.accounts_page.is_none() {
                    let state = self.state.clone();
                    self.accounts_page = Some(cx.new(|cx| AccountsPage::new(state, cx)));
                }
                match &self.accounts_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Secrets => {
                if self.secrets_page.is_none() {
                    let state = self.state.clone();
                    self.secrets_page = Some(cx.new(|cx| SecretsPage::new(state, cx)));
                }
                match &self.secrets_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::VersionControl => {
                if self.vcs_page.is_none() {
                    let state = self.state.clone();
                    self.vcs_page = Some(cx.new(|cx| VcsPage::new(state, cx)));
                }
                match &self.vcs_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Terminal => {
                if self.terminal_page.is_none() {
                    let command = self.settings.terminal_command.clone();
                    let page = cx.new(|cx| TerminalPage::new(command, cx));
                    self.terminal_settings_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &TerminalSettingsEvent, cx| {
                            let TerminalSettingsEvent::Changed(command) = event;
                            this.settings.terminal_command = command.clone();
                            if let Some(panel) = this.terminal.clone() {
                                panel.update(cx, |panel, cx| {
                                    panel.set_launch_command(command.clone(), cx);
                                });
                            }
                            this.schedule_save(cx);
                        },
                    ));
                    self.terminal_page = Some(page);
                }
                match &self.terminal_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Appearance => {
                if self.appearance_page.is_none() {
                    self.appearance_page = Some(cx.new(AppearancePage::new));
                }
                match &self.appearance_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Notifications => {
                if self.notifications_page.is_none() {
                    let system_notifications_enabled = self.settings.system_notifications_enabled;
                    let page = cx.new(|_| NotificationsPage::new(system_notifications_enabled));
                    self.notifications_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &NotificationsEvent, cx| {
                            let NotificationsEvent::SystemNotificationsEnabledChanged(enabled) =
                                event;
                            this.settings.system_notifications_enabled = *enabled;
                            crate::toast::configure(this.settings.system_notifications_enabled, cx);
                            this.schedule_save(cx);
                        },
                    ));
                    self.notifications_page = Some(page);
                }
                match &self.notifications_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Hotkeys => {
                if self.hotkeys_page.is_none() {
                    let state = self.state.clone();
                    let keymap = self.settings.keymap.clone();
                    let page = cx.new(|cx| HotkeysPage::new(state, keymap, cx));
                    // Persist + re-apply the keymap whenever the page changes it.
                    self.hotkeys_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &HotkeysEvent, cx| {
                            let HotkeysEvent::Changed(keymap) = event;
                            this.settings.keymap = keymap.clone();
                            apply_keymap(cx, keymap);
                            // gpui snapshots menu key equivalents in `set_menus`.
                            cx.set_menus(crate::app_menus::app_menus());
                            this.schedule_save(cx);
                            cx.notify();
                        },
                    ));
                    self.hotkeys_page = Some(page);
                }
                match &self.hotkeys_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
        }
    }

    /// Lazily create the standalone archived-sessions page.
    fn archived_outlet(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.archived_page.is_none() {
            let state = self.state.clone();
            self.archived_page = Some(cx.new(|cx| ArchivedPage::new(state, cx)));
        }
        match &self.archived_page {
            Some(page) => page.clone().into_any_element(),
            None => Empty.into_any_element(),
        }
    }

    // ---- sidebar mutations ----

    /// Fire a Mutate op; failures surface through the app-wide toast center.
    fn mutate(&mut self, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            crate::toast::show(
                Toast::new("mutation-error", "Action failed", ToastKind::Error)
                    .body("The Jolt engine is not connected."),
                cx,
            );
            return;
        };
        self.mutate_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = engine.client().call(methods::MUTATE, params).await {
                this.update(cx, |_, cx| {
                    crate::toast::show(
                        Toast::new("mutation-error", "Action failed", ToastKind::Error)
                            .body(err.to_string()),
                        cx,
                    );
                })
                .ok();
            }
        }));
    }

    fn open_rename_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.chat_menu = None;
        let current = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .and_then(|c| c.title.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Session title", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename_chat(cx);
            }
        });
        self.rename_dialog = Some(RenameChatDialog {
            chat_id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    fn submit_rename_chat(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_dialog.take() else {
            return;
        };
        let title = dialog.input.read(cx).text().trim().to_string();
        if !title.is_empty() {
            self.mutate(
                serde_json::json!({ "op": "renameChat", "chatId": dialog.chat_id, "title": title }),
                cx,
            );
        }
        cx.notify();
    }

    fn archive_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.chat_menu = None;
        self.remove_archived_tab(&chat_id, cx);
        self.mutate(
            serde_json::json!({ "op": "setChatArchived", "chatId": chat_id, "archived": true }),
            cx,
        );
        cx.notify();
    }

    fn confirm_delete_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.chat_menu = None;
        self.delete_confirm = Some(chat_id);
        cx.notify();
    }

    fn delete_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.delete_confirm = None;
        if self.state.read(cx).selected_chat.as_deref() == Some(chat_id.as_str()) {
            self.state.update(cx, |s, cx| s.select_chat(None, cx));
        }
        self.composer
            .update(cx, |composer, _| composer.purge_chat(&chat_id));
        self.mutate(
            serde_json::json!({ "op": "deleteChat", "chatId": chat_id }),
            cx,
        );
        cx.notify();
    }

    fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.user_menu_open = false;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = engine
                .client()
                .call(methods::SIGN_OUT, serde_json::json!({}))
                .await
            {
                this.update(cx, |_, cx| {
                    crate::toast::show(
                        Toast::new("sign-out-error", "Sign out failed", ToastKind::Error)
                            .body(err.to_string()),
                        cx,
                    );
                })
                .ok();
            }
        }));
        cx.notify();
    }

    fn switch_scope(&mut self, scope: jolt_engine::ScopeKind, cx: &mut Context<Self>) {
        self.user_menu_open = false;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(err) = engine
                .client()
                .call(methods::SWITCH_SCOPE, serde_json::json!({ "scope": scope }))
                .await
            {
                tracing::warn!(error = %err, "scope switch failed");
            }
        })
        .detach();
        cx.notify();
    }

    fn resolve_account_link(&mut self, merge: bool, cx: &mut Context<Self>) {
        if merge {
            let navigation = ScopeNavigation {
                last_space_id: self.settings.last_space_id.clone(),
                open_tabs: self.settings.open_tabs.clone(),
                active_tab_id: self.settings.active_tab_id.clone(),
                space_filter: self.settings.space_filter.clone(),
            };
            self.settings
                .scope_navigation
                .insert("account".into(), navigation);
            self.settings
                .scope_navigation
                .insert("local".into(), ScopeNavigation::default());
            self.schedule_save(cx);
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = engine
                .client()
                .call(
                    methods::RESOLVE_ACCOUNT_LINK,
                    serde_json::json!({ "merge": merge }),
                )
                .await
            {
                this.update(cx, |_, cx| {
                    crate::toast::show(
                        Toast::new(
                            "local-account-link-error",
                            "Couldn’t open the account",
                            ToastKind::Error,
                        )
                        .body(err.to_string()),
                        cx,
                    );
                })
                .ok();
            }
        }));
    }

    fn start_sign_in(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SIGN_IN, serde_json::json!({}))
                .await;
            this.update(cx, |_, cx| match result {
                Ok(value) => {
                    if let Some(url) = value.get("url").and_then(|u| u.as_str()) {
                        cx.open_url(url);
                    }
                }
                Err(err) => {
                    crate::toast::show(
                        Toast::new("sign-in-error", "Sign in failed", ToastKind::Error)
                            .body(err.to_string()),
                        cx,
                    );
                }
            })
            .ok();
        }));
    }

    // ---- automatic organization setup ----

    fn ensure_org_ui(&mut self, cx: &mut Context<Self>) {
        if self.org.is_some() {
            return;
        }
        self.org = Some(OrgGateUi {
            submitting: false,
            error: None,
            task: None,
        });
        self.provision_personal_org(cx);
    }

    fn provision_personal_org(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(org) = self.org.as_mut() else { return };
        if org.submitting {
            return;
        }
        org.submitting = true;
        org.error = None;
        org.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::ENSURE_PERSONAL_ORG, serde_json::json!({}))
                .await;
            this.update(cx, |shell, cx| {
                if let Some(org) = shell.org.as_mut() {
                    org.submitting = false;
                    if let Err(err) = result {
                        org.error = Some(err.to_string().into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // ---- render pieces ----

    /// Evaluate a width tween at "now" (manual drive — see [`WidthTween`]).
    /// Mid-flight: eased 200ms lerp, and `motion_active` is flagged so render
    /// schedules the next animation frame. Finished, stale, absent, or under
    /// reduced motion: exactly `target`. Honors `JOLT_MOTION_SCALE`.
    fn eval_tween(&self, tween: Option<WidthTween>, target: f32) -> f32 {
        let Some(WidthTween { from, to, started }) = tween else {
            return target;
        };
        if self.reduced_motion {
            return target;
        }
        let total = RESIZE.total().mul_f32(motion::speed_scale());
        let raw = started.elapsed().as_secs_f32() / total.as_secs_f32();
        if raw >= 1.0 {
            return target;
        }
        self.motion_active.set(true);
        motion::lerp(from, to, RESIZE.progress(raw))
    }

    /// Animated width container: tweens 200ms ease-out on collapse/expand, and
    /// clips a fixed-width inner so content never reflows mid-transition.
    fn pane_container(
        &self,
        tween: Option<WidthTween>,
        target: f32,
        inner: AnyElement,
    ) -> AnyElement {
        div()
            .h_full()
            .flex_none()
            .overflow_hidden()
            .w(px(self.eval_tween(tween, target)))
            .child(inner)
            .into_any_element()
    }

    /// The animated spacer clearing the macOS traffic lights ahead of a
    /// titlebar control cluster. Fullscreen toggles tween the cluster start
    /// over 200ms ease-out ([`RESIZE`]; reduced motion snaps).
    /// `None` off macOS — no phantom flex child.
    fn titlebar_spacer(&self, container_pad: f32) -> Option<AnyElement> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let fullscreen = self.fullscreen.unwrap_or(false);
        // The tween runs in cluster-start coordinates; the spacer is that
        // minus the container's own padding.
        let start = self.eval_tween(self.titlebar_tween, titlebar_cluster_start(fullscreen));
        let width = (start - container_pad).max(0.0);
        Some(div().flex_none().h_full().w(px(width)).into_any_element())
    }

    /// The header's content row with an animated left inset. On sidebar toggles
    /// and macOS fullscreen flips, the same element's padding tweens so the title
    /// glides to its new x-position. Route changes snap by killing the tween.
    /// Where unified-titlebar content (tabs / the settings label) starts: past
    /// the traffic lights + control cluster, riding the fullscreen inset tween.
    pub(super) fn title_bar_content_start(&self) -> f32 {
        let fullscreen = self.fullscreen.unwrap_or(false);
        let is_macos = cfg!(target_os = "macos");
        let cluster = self.eval_tween(
            self.titlebar_tween,
            cluster_buttons_start(is_macos, fullscreen),
        );
        cluster + CLUSTER_BUTTONS_WIDTH + 10.0
    }

    /// The unified window titlebar: chat shows the session tab strip;
    /// secondary pages keep the strip clear. Full-width on the glass shell;
    /// the traffic lights and control cluster overlay its left end.
    fn render_title_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        match self.route {
            Route::Chat => self.render_session_tab_strip(cx),
            Route::Archived | Route::Settings(_) => {
                let inner = div()
                    .size_full()
                    .flex()
                    .items_center()
                    .pt(px(Theme::TITLEBAR_TOP_PAD))
                    .pl(px(self.title_bar_content_start()))
                    .pr(px(titlebar_right_padding(
                        cfg!(target_os = "windows"),
                        Theme::SPACE_LG,
                    )));
                let bar = div().h(px(Theme::TITLEBAR_HEIGHT)).flex_none().child(inner);
                self.titlebar_drag_region("settings-header-titlebar", bar, cx)
                    .into_any_element()
            }
        }
    }

    /// Make a titlebar strip drag the window using the platform-titlebar pattern:
    /// mark it as a [`WindowControlArea::Drag`]
    /// (macOS app-owned titlebar), hand the drag to the compositor once the
    /// pointer moves with the button down, and double-click zooms.
    fn titlebar_drag_region(
        &self,
        id: &'static str,
        el: gpui::Div,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        el.id(id)
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down_out(cx.listener(|this, _, _, _| this.titlebar_should_move = false))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_should_move = false),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_should_move = true),
            )
            // Hand the drag to the compositor only while the button is
            // actually held (`pressed_button` guard): on macOS
            // `start_window_move` runs AppKit's NATIVE drag session
            // (`performWindowDragWithEvent:`), and AppKit resolves a quick
            // second click inside that session as a titlebar double-click —
            // system zoom — natively, beyond gpui's reach. Without the guard a
            // stale `titlebar_should_move` (armed by a down whose bubble was
            // later stopped) would start that session from a mere hover move
            // between the two clicks of a double-click.
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, _| {
                    if this.titlebar_should_move && event.pressed_button == Some(MouseButton::Left)
                    {
                        this.titlebar_should_move = false;
                        window.start_window_move();
                    }
                }),
            )
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    if cfg!(target_os = "macos") {
                        // Native titlebar double-click action (zoom/minimize
                        // per system preference).
                        window.titlebar_double_click();
                    } else {
                        window.zoom_window();
                    }
                }
            })
    }

    /// The one top-left window-control cluster (sidebar toggle + back/forward),
    /// rendered once in a paint-only overlay layer
    /// pinned at the window's top-left, ABOVE the sidebar and headers. The
    /// sidebar width animates *beneath* it, so the buttons keep their element
    /// identity and never move or remount on collapse/expand; only the
    /// fullscreen traffic-light inset tweens (the animated spacer). The
    /// container has no id/listeners — everything between the buttons falls
    /// through to the titlebar drag strips below.
    fn render_titlebar_cluster(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let can_back = self.nav.can_back();
        let can_forward = self.nav.can_forward();
        div()
            .absolute()
            .top_0()
            .left_0()
            .h(px(Theme::TITLEBAR_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .pt(px(Theme::TITLEBAR_TOP_PAD))
            .gap(px(2.0))
            .px(px(10.0))
            .children(self.titlebar_spacer(12.0))
            .child(window_control_button(
                "toggle-sidebar",
                icons::SIDEBAR_MINIMALISTIC_LEFT,
                &theme,
                cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)),
            ))
            .child(nav_history_button(
                "nav-back",
                icons::ARROW_LEFT,
                can_back,
                &theme,
                cx.listener(|this, _, _, cx| this.navigate_back(cx)),
            ))
            .child(nav_history_button(
                "nav-forward",
                icons::ARROW_RIGHT,
                can_forward,
                &theme,
                cx.listener(|this, _, _, cx| this.navigate_forward(cx)),
            ))
            .into_any_element()
    }

    /// Native Windows caption controls integrated into Jolt's unified
    /// titlebar. `WindowControlArea` maps these hit targets to HTMINBUTTON,
    /// HTMAXBUTTON, and HTCLOSE, so Windows owns their behavior (including
    /// Snap Layouts) while GPUI renders the system Segoe caption glyphs.
    fn render_windows_caption_controls(&self, window: &Window, cx: &App) -> Option<AnyElement> {
        if !cfg!(target_os = "windows") {
            return None;
        }

        let theme = Theme::of(cx);
        let (maximize_id, maximize_glyph) = if window.is_maximized() {
            ("window-restore", "\u{e923}")
        } else {
            ("window-maximize", "\u{e922}")
        };
        Some(
            div()
                .id("windows-window-controls")
                .absolute()
                .top_0()
                .right_0()
                .h(px(Theme::TITLEBAR_HEIGHT))
                .flex()
                .flex_row()
                .font_family("Segoe Fluent Icons")
                .child(windows_caption_button(
                    "window-minimize",
                    "\u{e921}",
                    WindowControlArea::Min,
                    theme,
                    false,
                ))
                .child(windows_caption_button(
                    maximize_id,
                    maximize_glyph,
                    WindowControlArea::Max,
                    theme,
                    false,
                ))
                .child(windows_caption_button(
                    "window-close",
                    "\u{e8bb}",
                    WindowControlArea::Close,
                    theme,
                    true,
                ))
                .into_any_element(),
        )
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let inner: AnyElement = match self.route {
            Route::Settings(section) => self.render_settings_nav(section, &theme, cx),
            Route::Chat | Route::Archived => self.render_chat_sidebar(&theme, cx),
        };
        let target = self.sidebar_target();
        // Transparent — the sidebar sits directly on the frost shell; the main
        // card's own border provides the separation.
        self.pane_container(
            self.sidebar_tween,
            target,
            div().h_full().child(inner).into_any_element(),
        )
    }

    /// Settings-mode sidebar: window-control strip, "Settings" heading, icon
    /// section rows styled like session rows,
    /// and a Back row pinned to the bottom.
    fn render_settings_nav(
        &mut self,
        section: SettingsSection,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let section_icon = |item: SettingsSection| match item {
            SettingsSection::Devices => icons::MONITOR,
            SettingsSection::Agents => icons::USER,
            SettingsSection::Secrets => icons::KEY_MINIMALISTIC,
            SettingsSection::VersionControl => icons::GIT_BRANCH,
            SettingsSection::Terminal => icons::TERMINAL,
            SettingsSection::Appearance => icons::TUNING,
            SettingsSection::Notifications => icons::BELL,
            SettingsSection::Hotkeys => icons::KEYBOARD,
        };
        // Match the user's dragged sidebar width — the pane container clips to
        // it, so a hardcoded default here left hover washes stopping short of
        // the sidebar's right edge (user-reported). Device identity lives on
        // the Accounts page now — the one surface where the device matters.
        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .px(px(Theme::SPACE_SM))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px(px(Theme::SPACE_SM))
                            .pt(px(12.0))
                            .pb(px(4.0))
                            .text_size(px(11.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from("Settings")),
                    )
                    .child(div().flex().flex_col().gap(px(2.0)).children(
                        SettingsSection::ALL.into_iter().map(|item| {
                            let selected = item == section;
                            div()
                                .id(SharedString::from(format!("settings-nav-{}", item.label())))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .rounded(px(8.0))
                                .px(px(Theme::SPACE_SM))
                                .py(px(6.0))
                                .text_size(px(13.0))
                                .when(selected, |el| {
                                    el.bg(crate::theme::wash(0.17))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                })
                                .text_color(if selected {
                                    theme.text
                                } else {
                                    theme.text_muted
                                })
                                .cursor_pointer()
                                .hover(|s| s.bg(crate::theme::wash(0.11)).text_color(theme.text))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.open_settings(item, cx)),
                                )
                                .child(
                                    icon(section_icon(item))
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from(item.label()))
                        }),
                    )),
            )
            // Back is pinned to the bottom.
            .child(
                div().px(px(Theme::SPACE_SM)).pb(px(12.0)).child(
                    div()
                        .id("settings-back")
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .rounded(px(8.0))
                        .px(px(Theme::SPACE_SM))
                        .py(px(6.0))
                        .text_size(px(13.0))
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .hover(|s| s.bg(crate::theme::wash(0.11)).text_color(theme.text))
                        .on_click(cx.listener(|this, _, _, cx| this.close_secondary_page(cx)))
                        .child(
                            // Use the AltArrowLeft chevron, not the straight
                            // history arrow.
                            icon(icons::ALT_ARROW_LEFT)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Back")),
                ),
            )
            .into_any_element()
    }

    /// One session row: status rail on the left
    /// (a live dotted orb while working, a dot otherwise), title +
    /// relative time on the first line, "folder · device" underneath aligned
    /// to the title. Click selects; right-click opens the context menu.
    #[allow(clippy::too_many_arguments)]
    fn render_chat_row(
        &self,
        id: String,
        title: SharedString,
        time_ago: SharedString,
        space_name: SharedString,
        branch: Option<SharedString>,
        harness: Option<jolt_proto::HarnessId>,
        status: jolt_proto::ChatIndicator,
        selected: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Status is a rail, not a word, and is always present
        // so rows align and state changes read in place. Working animates as a
        // compact dotted orb; every other status is a dot.
        let dot_color = spaces::status_dot_color(status, theme);
        let status_rail: AnyElement = if status == jolt_proto::ChatIndicator::Working {
            div()
                .w(px(6.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(loaders::activity_orb(
                    format!("chat-working-{id}"),
                    theme,
                    16.0,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element()
        } else {
            div()
                .size(px(6.0))
                .rounded_full()
                .flex_none()
                .bg(dot_color)
                .into_any_element()
        };
        let (hover, text) = (theme.glass_hover(), theme.text);
        let selected_wash = crate::theme::glass_selected_bg();
        let subline = theme.text_muted.opacity(0.5);
        let select_id = id.clone();
        let menu_id = id.clone();
        let archive_id = id.clone();
        let delete_id = id.clone();
        let group: SharedString = format!("chat-row-group-{id}").into();
        // Both the hover wash and title brighten over the same 150ms blend.
        let fade_key = format!("chat-row-{id}");
        let rest_bg = if selected {
            selected_wash
        } else {
            crate::theme::wash(0.0)
        };
        // A selected row must NOT drift toward the hover wash: in dark the two
        // fills are identical so the blend is a no-op, but light's hover sits
        // below its near-opaque selected fill, and blending toward it visibly
        // dimmed the active row under the pointer (user report).
        let hover_bg = if selected { selected_wash } else { hover };
        let rest_text = if selected { text } else { text.opacity(0.8) };
        div()
            .id(SharedString::from(format!("chat-{id}")))
            .group(group.clone())
            .relative()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .text_color(motion::hover_blend(&fade_key, rest_text, text))
            .bg(motion::hover_blend(&fade_key, rest_bg, hover_bg))
            .when(selected, |el| {
                el.shadow(crate::theme::glass_selected_shadows())
            })
            .on_hover(motion::hover_listener(fade_key))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_chat_tab(select_id.clone(), cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.chat_menu = Some((menu_id.clone(), event.position));
                    cx.notify();
                }),
            )
            // Line 1: status rail, space name, time-ago.
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::SPACE_SM))
                    .child(status_rail)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.0))
                            .line_height(px(14.0))
                            .text_color(subline)
                            .child(space_name),
                    )
                    // The fixed trailing slot keeps metadata from reflowing
                    // when hover swaps the timestamp for row actions.
                    .child(
                        div()
                            .w(px(40.0))
                            .h(px(14.0))
                            .flex_none()
                            .relative()
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .right_0()
                                    .text_size(px(11.0))
                                    .text_color(subline)
                                    .opacity(1.0)
                                    .group_hover(group.clone(), |style| style.opacity(0.0))
                                    .child(time_ago),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .top(px(-2.0))
                                    .right_0()
                                    .flex()
                                    .flex_row()
                                    .gap(px(2.0))
                                    .opacity(0.0)
                                    .group_hover(group.clone(), |style| style.opacity(1.0))
                                    .child(
                                        div()
                                            .id(SharedString::from(format!("chat-archive-{id}")))
                                            .size(px(18.0))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded(px(5.0))
                                            .text_color(theme.text_muted)
                                            .cursor_pointer()
                                            .hover(|style| {
                                                style
                                                    .bg(crate::theme::wash(0.14))
                                                    .text_color(theme.text)
                                            })
                                            .tooltip(|_, cx| {
                                                cx.new(|_| {
                                                    SessionActionTooltip("Archive session".into())
                                                })
                                                .into()
                                            })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.archive_chat(archive_id.clone(), cx);
                                            }))
                                            .child(
                                                icon(icons::ARCHIVE_MINIMALISTIC)
                                                    .size(px(12.0))
                                                    .text_color(theme.text_muted),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .id(SharedString::from(format!("chat-delete-{id}")))
                                            .size(px(18.0))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded(px(5.0))
                                            .text_color(theme.text_muted)
                                            .cursor_pointer()
                                            .hover(|style| {
                                                style
                                                    .bg(theme.danger.opacity(0.12))
                                                    .text_color(theme.danger)
                                            })
                                            .tooltip(|_, cx| {
                                                cx.new(|_| {
                                                    SessionActionTooltip("Delete session".into())
                                                })
                                                .into()
                                            })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.confirm_delete_chat(delete_id.clone(), cx);
                                            }))
                                            .child(
                                                icon(icons::TRASH_BIN_MINIMALISTIC)
                                                    .size(px(12.0))
                                                    .text_color(theme.danger),
                                            ),
                                    ),
                            ),
                    ),
            )
            // Line 2: the session title, aligned under the folder icon
            // (rail 6 + gap 8).
            .child(
                div()
                    .w_full()
                    .pl(px(14.0))
                    .truncate()
                    .text_size(px(13.0))
                    .line_height(px(17.0))
                    .child(title),
            )
            // Line 3 (always): harness brand mark; worktree sessions append
            // the branch icon + name.
            .child(
                div()
                    .w_full()
                    .pl(px(14.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .when_some(
                        harness.map(crate::pickers::harness_brand_icon),
                        |el, (path, tint)| {
                            el.child(
                                icon(path)
                                    .size(px(11.0))
                                    .flex_none()
                                    .text_color(tint.unwrap_or(subline).opacity(0.8)),
                            )
                        },
                    )
                    .when_some(branch, |el, branch| {
                        el.child(
                            icon(icons::GIT_BRANCH)
                                .size(px(11.0))
                                .flex_none()
                                .text_color(subline),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(11.0))
                                .line_height(px(14.0))
                                .text_color(subline)
                                .child(branch),
                        )
                    }),
            )
            .into_any_element()
    }

    /// Which sidebar-list edges have hidden overflow (offset from the LAST
    /// frame — the invisible one-frame lag every fade here rides).
    pub(super) fn sidebar_fade_zones(&self) -> (bool, bool) {
        let scroll = self.sidebar_scroll.0.borrow();
        let scrolled = -f32::from(scroll.base_handle.offset().y);
        let max_scroll = f32::from(scroll.base_handle.max_offset().y);
        (scrolled > 1.0, scrolled < max_scroll - 1.0)
    }

    /// Chat-mode sidebar: fixed space filter, filtered Sessions list, notices,
    /// and the user menu.
    fn render_chat_sidebar(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let user = self.state.read(cx).auth_user().cloned();

        // Keep only compact row data here. The uniform list below asks for the
        // visible range, so a large session history no longer rebuilds and lays
        // out every offscreen row on each composer or transcript frame. In the
        // 100-session release typing fixture this holds p50 draw at 1.9ms.
        let rows = self.active_rows(cx);

        // When the order of a live list changes due to activity or grouping,
        // surviving rows glide from their old y to the new one. Layout is at the new
        // position; the offset is a paint-only relative inset animated to 0
        // over 260ms cubic-bezier(0.22,1,0.36,1). New rows fade in; removals
        // disappear immediately. First fill and chat switches that do not
        // reorder never animate.
        let order: Vec<(String, f32)> = rows
            .iter()
            .map(|row| (row.key.clone(), CHAT_ROW_HEIGHT))
            .collect();
        if self.sidebar_prev_order != order {
            if !self.sidebar_prev_order.is_empty() {
                let offsets = resort_offsets(&self.sidebar_prev_order, &order, SIDEBAR_LIST_GAP);
                let prev_keys: std::collections::HashSet<&str> = self
                    .sidebar_prev_order
                    .iter()
                    .map(|(key, _)| key.as_str())
                    .collect();
                let new_keys: std::collections::HashSet<String> = order
                    .iter()
                    .filter(|(key, _)| !prev_keys.contains(key.as_str()))
                    .map(|(key, _)| key.clone())
                    .collect();
                if !offsets.is_empty() || !new_keys.is_empty() {
                    self.resort_epoch += 1;
                    self.sidebar_resort = offsets;
                    self.sidebar_new_keys = new_keys;
                }
            }
            self.sidebar_prev_order = order;
        }

        let row_count = rows.len();
        let epoch = self.resort_epoch;
        let theme_for_rows = theme.clone();
        let shell = cx.weak_entity();
        let sessions = uniform_list("sidebar-sessions", row_count, move |range, _, cx| {
            shell
                .update(cx, |this, cx| {
                    range
                        .filter_map(|index| rows.get(index))
                        .map(|row| {
                            let element = this.render_chat_row(
                                row.id.clone(),
                                row.title.clone(),
                                row.time_ago.clone(),
                                row.space_name.clone(),
                                row.branch.clone(),
                                row.harness,
                                row.status,
                                row.selected,
                                &theme_for_rows,
                                cx,
                            );
                            let element = if let Some(dy) =
                                this.sidebar_resort.get(&row.key).copied()
                            {
                                let id = SharedString::from(format!("resort-{epoch}-{}", row.key));
                                div()
                                    .child(element)
                                    .with_animation(id, RESORT.animation(), move |el, t| {
                                        el.relative().top(px(dy * (1.0 - t)))
                                    })
                                    .into_any_element()
                            } else if this.sidebar_new_keys.contains(&row.key) {
                                let id = SharedString::from(format!("row-in-{epoch}-{}", row.key));
                                motion::fade_quick(id, div().child(element)).into_any_element()
                            } else {
                                element
                            };
                            div()
                                .h(px(CHAT_ROW_HEIGHT + SIDEBAR_LIST_GAP))
                                .pb(px(SIDEBAR_LIST_GAP))
                                .child(element)
                                .into_any_element()
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
        .track_scroll(&self.sidebar_scroll)
        .w_full()
        .flex_1()
        .min_h_0();

        // Overflow edge fades for the lists scroll region — the tab strip's
        // idiom, vertical (offset from the LAST frame; the lag is invisible).
        let (lists_fade_top, lists_fade_bottom) = self.sidebar_fade_zones();
        // Opaque platforms melt overflow into the surface tone with painted
        // gradient overlays. Over GLASS no overlay can work — the backdrop is
        // see-through blur, so tone stacks into a smudge and black reads as a
        // shadow (user reports). Instead the ROWS fade themselves: prepaint-
        // measured bounds drive per-row opacity toward the viewport edges
        // ([`Shell::sidebar_row_alpha`]), dissolving the edge to pure glass.
        let glass = theme.is_glass();
        let sidebar_fade = theme.surface;

        let local_active = self.state.read(cx).active_scope() == jolt_engine::ScopeKind::Local;
        let user_line: SharedString = if local_active {
            SharedString::from("Local")
        } else {
            user.as_ref()
                .map(|u| u.name.clone().unwrap_or_else(|| u.email.clone()).into())
                .unwrap_or_else(|| SharedString::from("Account"))
        };
        let user_email: Option<SharedString> = if local_active {
            Some(SharedString::from("Stored only on this device"))
        } else {
            user.as_ref().map(|u| u.email.clone().into())
        };
        let user_menu = self.render_user_menu(user_line.clone(), user_email.clone(), theme, cx);

        let filter_row = self.render_spaces_filter(theme, cx);

        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            // The filter stays outside overflow so its dropdown cannot clip.
            .child(filter_row)
            // The filtered Sessions list is the only scrolling sidebar body.
            .child(crate::edge_fade::edge_faded(
                SIDEBAR_GLASS_FADE_BAND,
                glass && lists_fade_top,
                glass && lists_fade_bottom,
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("sidebar-lists")
                            .size_full()
                            .px(px(Theme::SPACE_SM))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .h(px(28.0))
                                    .pl(px(Theme::SPACE_SM))
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .text_size(px(11.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text_muted.opacity(0.6))
                                    .child(SharedString::from("Sessions"))
                                    .child(
                                        div()
                                            .id("search-sessions")
                                            .size(px(24.0))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded(px(6.0))
                                            .cursor_pointer()
                                            .bg(motion::hover_blend(
                                                "search-sessions",
                                                crate::theme::wash(0.0),
                                                crate::theme::wash(0.14),
                                            ))
                                            .on_hover(motion::hover_listener("search-sessions"))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.open_session_search(cx)
                                            }))
                                            .child(
                                                icon(icons::MAGNIFER)
                                                    .size(px(14.0))
                                                    .text_color(theme.text_muted.opacity(0.7)),
                                            ),
                                    ),
                            )
                            .child(if row_count > 0 {
                                sessions.into_any_element()
                            } else {
                                div()
                                    .px(px(Theme::SPACE_SM))
                                    .pb(px(Theme::SPACE_SM))
                                    .text_size(px(12.0))
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from("No sessions yet"))
                                    .into_any_element()
                            }),
                    )
                    .when(lists_fade_top && !glass, |el| {
                        el.child(div().absolute().top_0().left_0().right_0().h(px(24.0)).bg(
                            gpui::linear_gradient(
                                180.0,
                                gpui::linear_color_stop(sidebar_fade, 0.0),
                                gpui::linear_color_stop(sidebar_fade.opacity(0.0), 1.0),
                            ),
                        ))
                    })
                    .when(lists_fade_bottom && !glass, |el| {
                        el.child(
                            div()
                                .absolute()
                                .bottom_0()
                                .left_0()
                                .right_0()
                                .h(px(24.0))
                                .bg(gpui::linear_gradient(
                                    0.0,
                                    gpui::linear_color_stop(sidebar_fade, 0.0),
                                    gpui::linear_color_stop(sidebar_fade.opacity(0.0), 1.0),
                                )),
                        )
                    }),
            ))
            .child(div().p(px(Theme::SPACE_SM)).flex_none().child(user_menu))
            .into_any_element()
    }

    fn check_for_update(&mut self, cx: &mut Context<Self>) {
        self.user_menu_open = false;
        if self.update_checking {
            cx.notify();
            return;
        }
        self.update_checking = true;
        let edge_url = self.boot.edge_url.clone();
        let check = Tokio::spawn(
            cx,
            async move { jolt_update::fetch_latest(&edge_url).await },
        );
        self.update_check_task = Some(cx.spawn(async move |this, cx| {
            let outcome = match check.await {
                Ok(Ok(manifest)) => Ok(manifest),
                Ok(Err(error)) => Err(format!("{error:#}")),
                Err(error) => Err(error.to_string()),
            };
            this.update(cx, |shell, cx| {
                shell.update_checking = false;
                match outcome {
                    Ok(manifest)
                        if jolt_update::version_newer(
                            &manifest.version,
                            jolt_update::current_version(),
                        ) =>
                    {
                        shell.notified_update_version = Some(manifest.version.clone());
                        shell.show_jolt_update_available(manifest.version, cx);
                    }
                    Ok(_) => crate::toast::show(
                        Toast::new(
                            "jolt-update-check",
                            "Jolt is up to date",
                            ToastKind::Success,
                        )
                        .body(format!(
                            "Version {} is the latest available release.",
                            jolt_update::current_version()
                        )),
                        cx,
                    ),
                    Err(message) => {
                        let shell_handle = cx.entity().downgrade();
                        crate::toast::show(
                            Toast::new(
                                "jolt-update-check",
                                "Update check failed",
                                ToastKind::Error,
                            )
                            .body(message)
                            .action(ToastAction::new(
                                "Retry",
                                move |cx| {
                                    shell_handle
                                        .update(cx, |shell, cx| shell.check_for_update(cx))
                                        .ok();
                                },
                            )),
                            cx,
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Fetch and stage a new macOS bundle. Progress and outcomes are app-wide
    /// notifications so delivery follows the user's notification preference.
    fn begin_update_download(&mut self, cx: &mut Context<Self>) {
        if matches!(self.update_flow, UpdateFlow::Downloading) {
            return;
        }
        let edge_url = self.boot.edge_url.clone();
        let data_dir = self.data_dir.clone();
        self.update_flow = UpdateFlow::Downloading;
        crate::toast::show(
            Toast::new(
                "jolt-update-download",
                "Downloading Jolt update",
                ToastKind::Info,
            )
            .body("The update will be ready to restart shortly."),
            cx,
        );
        let download = Tokio::spawn(cx, async move {
            let manifest = jolt_update::fetch_latest(&edge_url).await?;
            jolt_update::stage_mac_app(&edge_url, &manifest, &data_dir).await
        });
        self.update_task = Some(cx.spawn(async move |this, cx| {
            let outcome = match download.await {
                Ok(Ok(staged)) => Ok(staged),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join_err) => Err(join_err.to_string()),
            };
            this.update(cx, |shell, cx| match outcome {
                Ok(staged) => {
                    shell.update_flow = UpdateFlow::Ready(staged);
                    let shell_handle = cx.entity().downgrade();
                    crate::toast::show(
                        Toast::new("jolt-update-ready", "Jolt update ready", ToastKind::Success)
                            .persistent()
                            .body("Restart Jolt to apply the update.")
                            .action(ToastAction::new("Restart", move |cx| {
                                shell_handle
                                    .update(cx, |shell, cx| shell.apply_ready_update(cx))
                                    .ok();
                            })),
                        cx,
                    );
                }
                Err(message) => {
                    tracing::warn!(%message, "update download failed");
                    shell.update_flow = UpdateFlow::Failed;
                    shell.show_update_error(message, cx);
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn apply_ready_update(&mut self, cx: &mut Context<Self>) {
        let flow = std::mem::replace(&mut self.update_flow, UpdateFlow::Idle);
        match flow {
            UpdateFlow::Ready(staged) => self.apply_staged_update(staged, cx),
            other => self.update_flow = other,
        }
    }

    fn show_update_error(&mut self, message: String, cx: &mut Context<Self>) {
        let shell = cx.entity().downgrade();
        crate::toast::show(
            Toast::new("jolt-update-error", "Jolt update failed", ToastKind::Error)
                .persistent()
                .body(message)
                .action(ToastAction::new("Retry", move |cx| {
                    shell
                        .update(cx, |shell, cx| shell.begin_update_download(cx))
                        .ok();
                })),
            cx,
        );
    }

    /// Swap the staged bundle over the installed one, arm the detached
    /// relauncher, and quit — the relauncher `open`s the new bundle once this
    /// process (and its engine lock / IPC port) is gone.
    fn apply_staged_update(&mut self, staged: PathBuf, cx: &mut Context<Self>) {
        let jolt_update::InstallKind::MacApp { bundle } = self.install.clone() else {
            return;
        };
        match jolt_update::apply_mac_app(&staged, &bundle) {
            Ok(()) => {
                jolt_update::relaunch_app_after_exit(&bundle);
                cx.quit();
            }
            Err(err) => {
                let message = format!("{err:#}");
                tracing::error!(error = %err, "update apply failed");
                self.update_flow = UpdateFlow::Failed;
                self.show_update_error(message, cx);
            }
        }
    }

    /// User menu: account identity, app pages, updates, and scope actions.
    fn render_user_menu(
        &mut self,
        user_line: SharedString,
        user_email: Option<SharedString>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.user_menu_open;
        let mut trigger = div()
            .id("user-menu")
            .flex_none()
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(Theme::SPACE_SM))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .cursor_pointer()
            // The open state uses a slightly stronger wash than hover, and
            // the hover wash fades over the standard color transition.
            .bg(if open {
                theme.glass_hover()
            } else {
                motion::hover_blend(
                    "user-menu-trigger",
                    theme.glass_hover().opacity(0.0),
                    theme.glass_hover().opacity(0.8),
                )
            })
            .on_hover(motion::hover_listener("user-menu-trigger"))
            .on_click(cx.listener(|this, _, _, cx| {
                // A click that just dismissed the menu (outside-click on the
                // trigger) must not instantly reopen it.
                let just_dismissed = this
                    .user_menu_dismissed_at
                    .is_some_and(|at| at.elapsed() < Duration::from_millis(400));
                this.user_menu_open = !this.user_menu_open && !just_dismissed;
                this.user_menu_dismissed_at = None;
                cx.notify();
            }))
            .child(
                div()
                    .size(px(24.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(theme.element_hover)
                    .child(
                        icon(icons::USER)
                            .size(px(14.0))
                            .text_color(theme.text_muted),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(13.0))
                    .line_height(px(17.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .truncate()
                    .child(user_line.clone()),
            );
        if open {
            // The menu is exactly as wide as the trigger row: sidebar minus
            // its gutters. Local and Account share settings/update actions;
            // only their final scope/auth actions differ.
            let (active_scope, account_available, scopes_supported) = {
                let state = self.state.read(cx);
                (
                    state.active_scope(),
                    state.account_available(),
                    state.scope.is_some(),
                )
            };
            let mut scope_rows: Vec<AnyElement> = match active_scope {
                jolt_engine::ScopeKind::Account => vec![
                    popover::menu_row(theme, false, "user-menu-signout")
                        .id("user-menu-signout")
                        .on_click(cx.listener(|this, _, _, cx| this.sign_out(cx)))
                        .child(
                            icon(icons::LOGOUT_2)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Sign out"))
                        .into_any_element(),
                ],
                jolt_engine::ScopeKind::Local if account_available => vec![
                    popover::menu_row(theme, false, "user-menu-switch-account")
                        .id("user-menu-switch-account")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.switch_scope(jolt_engine::ScopeKind::Account, cx)
                        }))
                        .child(
                            icon(icons::GLOBAL)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Switch back to account"))
                        .into_any_element(),
                ],
                jolt_engine::ScopeKind::Local => vec![
                    popover::menu_row(theme, false, "user-menu-signin")
                        .id("user-menu-signin")
                        .on_click(cx.listener(|this, _, _, cx| this.start_sign_in(cx)))
                        .child(
                            icon(icons::GLOBAL)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Sign in"))
                        .into_any_element(),
                ],
            };
            if active_scope == jolt_engine::ScopeKind::Account && scopes_supported {
                scope_rows.insert(
                    0,
                    popover::menu_row(theme, false, "user-menu-switch-local")
                        .id("user-menu-switch-local")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.switch_scope(jolt_engine::ScopeKind::Local, cx)
                        }))
                        .child(
                            icon(icons::LAPTOP)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Switch to Local"))
                        .into_any_element(),
                );
            }
            let menu = popover::popover_card(theme)
                .w(px(self.settings.sidebar_width - 2.0 * Theme::SPACE_SM))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.user_menu_open = false;
                    this.user_menu_dismissed_at = Some(std::time::Instant::now());
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .px(px(8.0))
                        .pt(px(6.0))
                        .pb(px(4.0))
                        .text_size(px(11.0))
                        .text_color(theme.text_muted.opacity(0.7))
                        .truncate()
                        .child(user_email.unwrap_or(user_line)),
                )
                .child(
                    popover::menu_row(theme, false, "user-menu-settings")
                        .id("user-menu-settings")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_settings(SettingsSection::Devices, cx)
                        }))
                        .child(
                            icon(icons::SETTINGS_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Settings")),
                )
                .child(
                    popover::menu_row(theme, false, "user-menu-archived")
                        .id("user-menu-archived")
                        .on_click(cx.listener(|this, _, _, cx| this.open_archived(cx)))
                        .child(
                            icon(icons::ARCHIVE_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Archived sessions")),
                )
                .child(
                    popover::menu_row(theme, false, "user-menu-usage-breakdown")
                        .id("user-menu-usage-breakdown")
                        .on_click(cx.listener(|this, _, _, cx| this.open_breakdown(cx)))
                        .child(
                            icon(icons::LIST)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Usage breakdown")),
                )
                .child(
                    popover::menu_row(theme, self.update_checking, "user-menu-check-update")
                        .id("user-menu-check-update")
                        .on_click(cx.listener(|this, _, _, cx| this.check_for_update(cx)))
                        .child(
                            icon(icons::REFRESH)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from(if self.update_checking {
                            "Checking for updates…"
                        } else {
                            "Check for update"
                        })),
                )
                .child(popover::menu_separator())
                .children(scope_rows)
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu_above("user-menu-popover", menu));
        }
        trigger.into_any_element()
    }

    fn open_breakdown(&mut self, cx: &mut Context<Self>) {
        self.user_menu_open = false;
        self.breakdown_dialog = Some(BreakdownDialog {
            days: 30,
            data: Loadable::Loading,
            unavailable_devices: 0,
            task: None,
        });
        self.load_breakdown(30, cx);
    }

    fn load_breakdown(&mut self, days: u16, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            if let Some(dialog) = &mut self.breakdown_dialog {
                dialog.data = Loadable::Error("Engine not connected".into());
            }
            cx.notify();
            return;
        };
        let (remote_devices, offline_devices) = {
            let state = self.state.read(cx);
            state
                .local_device_id
                .as_ref()
                .map(|local_id| {
                    let remotes: Vec<_> = state
                        .devices
                        .iter()
                        .filter(|device| device.id != *local_id && device.is_engine_host())
                        .collect();
                    let online = remotes
                        .iter()
                        .filter(|device| state.device_online(&device.id, Utc::now()))
                        .map(|device| device.id.clone())
                        .collect::<Vec<_>>();
                    let offline = remotes.len().saturating_sub(online.len());
                    (online, offline)
                })
                .unwrap_or_default()
        };
        if let Some(dialog) = &mut self.breakdown_dialog {
            dialog.days = days;
            // Keep the previous report visible while switching ranges. Replacing
            // it with the shorter loading state made the modal collapse and
            // expand around every request.
            if !matches!(&dialog.data, Loadable::Ready(_)) {
                dialog.data = Loadable::Loading;
                dialog.unavailable_devices = 0;
            }
            dialog.task = Some(cx.spawn(async move |this, cx| {
                let mut targets = vec![None];
                targets.extend(remote_devices.into_iter().map(Some));
                let mut replies = Vec::new();
                let mut unavailable = offline_devices;
                for target in targets {
                    let mut params = serde_json::json!({ "days": days });
                    if let (Some(target), Some(object)) = (target, params.as_object_mut()) {
                        object.insert("targetDeviceId".into(), serde_json::Value::String(target));
                    }
                    match engine.client().call(methods::USAGE_BREAKDOWN, params).await {
                        Ok(value) => match serde_json::from_value::<UsageBreakdown>(value) {
                            Ok(reply) => replies.push(reply),
                            Err(error) => {
                                tracing::warn!(%error, "malformed usage breakdown");
                                unavailable += 1;
                            }
                        },
                        Err(error) => {
                            tracing::warn!(%error, "usage breakdown device unavailable");
                            unavailable += 1;
                        }
                    }
                }
                this.update(cx, |shell, cx| {
                    let Some(dialog) = &mut shell.breakdown_dialog else {
                        return;
                    };
                    if dialog.days != days {
                        return;
                    }
                    dialog.task = None;
                    dialog.unavailable_devices = unavailable;
                    dialog.data = if replies.is_empty() {
                        Loadable::Error("Usage data is unavailable".into())
                    } else {
                        Loadable::Ready(merge_breakdowns(days, replies))
                    };
                    cx.notify();
                })
                .ok();
            }));
        }
        cx.notify();
    }

    fn render_breakdown_dialog(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.breakdown_dialog.as_ref()?;
        let days = dialog.days;
        let data = dialog.data.clone();
        let unavailable = dialog.unavailable_devices;
        let refreshing = dialog.task.is_some() && matches!(&data, Loadable::Ready(_));
        let theme = Theme::of(cx).clone();

        let ranges =
            div()
                .flex()
                .items_center()
                .gap(px(3.0))
                .children([7_u16, 30, 90].into_iter().map(|range| {
                    div()
                        .id(("breakdown-range", range as usize))
                        .px(px(9.0))
                        .h(px(24.0))
                        .flex()
                        .items_center()
                        .rounded(px(6.0))
                        .cursor_pointer()
                        .text_size(px(11.0))
                        .text_color(if days == range {
                            theme.text
                        } else {
                            theme.text_muted
                        })
                        .bg(if days == range {
                            theme.element_hover
                        } else {
                            gpui::transparent_black()
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.load_breakdown(range, cx);
                        }))
                        .child(format!("{range}d"))
                }));

        let mut body = div()
            .id("breakdown-body")
            .h(px(440.0))
            .max_h(viewport.height - px(120.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .opacity(if refreshing { 0.72 } else { 1.0 });
        match data {
            Loadable::Idle | Loadable::Loading => {
                body = body.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child("Loading usage…"),
                );
            }
            Loadable::Error(error) => {
                body = body.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(12.0))
                        .text_color(theme.danger_muted)
                        .child(error),
                );
            }
            Loadable::Ready(breakdown) => {
                let cost = breakdown
                    .cost_usd
                    .map(|value| format!("${value:.2}"))
                    .unwrap_or_else(|| "—".into());
                let summaries = [
                    ("Sessions", compact_number(breakdown.sessions)),
                    ("Output", compact_number(breakdown.output_tokens)),
                    ("Tokens", compact_number(breakdown.total_tokens())),
                    ("Reported cost", cost),
                ];
                body = body.child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .children(summaries.into_iter().map(|(label, value)| {
                            div()
                                .flex_1()
                                .p(px(10.0))
                                .rounded(px(8.0))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.element_hover.opacity(0.45))
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(theme.text_muted)
                                        .child(label),
                                )
                                .child(
                                    div()
                                        .mt(px(4.0))
                                        .text_size(px(17.0))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(theme.text)
                                        .child(value),
                                )
                        })),
                );

                let by_day: std::collections::HashMap<_, _> = breakdown
                    .activity
                    .iter()
                    .map(|day| (day.day.as_str(), day.tokens))
                    .collect();
                let max_tokens = breakdown
                    .activity
                    .iter()
                    .map(|day| day.tokens)
                    .max()
                    .unwrap_or(1)
                    .max(1);
                let cells = (0..breakdown.days).rev().map(|offset| {
                    let day = (chrono::Local::now().date_naive()
                        - chrono::Duration::days(i64::from(offset)))
                    .format("%Y-%m-%d")
                    .to_string();
                    let tokens = by_day.get(day.as_str()).copied().unwrap_or_default();
                    let intensity = tokens as f32 / max_tokens as f32;
                    div().size(px(11.0)).rounded(px(2.0)).bg(if tokens == 0 {
                        theme.element_hover.opacity(0.45)
                    } else {
                        theme.accent.opacity(0.22 + intensity * 0.78)
                    })
                });
                body = body.child(
                    div()
                        .child(
                            div()
                                .mb(px(7.0))
                                .text_size(px(11.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text_muted)
                                .child("Activity"),
                        )
                        .child(div().flex().flex_wrap().gap(px(4.0)).children(cells)),
                );

                let mut rows = div().flex().flex_col();
                for row in breakdown.rows.iter().take(8) {
                    let location = std::path::Path::new(&row.cwd)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(&row.cwd);
                    rows = rows.child(
                        div()
                            .h(px(34.0))
                            .border_t_1()
                            .border_color(theme.border)
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .text_size(px(11.0))
                            .child(
                                div()
                                    .w(px(90.0))
                                    .text_color(theme.text_muted)
                                    .child(harness_label(row.harness)),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .truncate()
                                    .text_color(theme.text)
                                    .child(row.model.clone()),
                            )
                            .child(
                                div()
                                    .w(px(90.0))
                                    .truncate()
                                    .text_color(theme.text_muted)
                                    .child(location.to_string()),
                            )
                            .child(
                                div()
                                    .w(px(64.0))
                                    .text_right()
                                    .text_color(theme.text)
                                    .child(compact_number(row.total_tokens())),
                            ),
                    );
                }
                body = body.child(
                    div()
                        .child(
                            div()
                                .mb(px(4.0))
                                .text_size(px(11.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text_muted)
                                .child("Harness · model · space"),
                        )
                        .child(rows),
                );
                if unavailable != 0 {
                    body = body.child(div().text_size(px(10.0)).text_color(theme.warning).child(
                        format!(
                            "{unavailable} device(s) unavailable or did not return usage data."
                        ),
                    ));
                }
            }
        }

        let card = div()
            .w(px(680.0))
            .max_w(viewport.width - px(32.0))
            .p(px(18.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_raised)
            .shadow_lg()
            .child(
                div()
                    .mb(px(16.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .child(
                                div()
                                    .text_size(px(16.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child("Usage breakdown"),
                            )
                            .child(
                                div()
                                    .mt(px(2.0))
                                    .text_size(px(10.0))
                                    .text_color(theme.text_muted)
                                    .child("Tracked Jolt sessions across reachable devices"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(ranges)
                            .child(
                                div()
                                    .id("close-breakdown")
                                    .size(px(24.0))
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_color(theme.text_muted)
                                    .bg(motion::hover_blend(
                                        "close-breakdown",
                                        gpui::transparent_black(),
                                        theme.element_hover,
                                    ))
                                    .on_hover(motion::hover_listener("close-breakdown"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.breakdown_dialog = None;
                                        cx.notify();
                                    }))
                                    .child("×"),
                            ),
                    ),
            )
            .child(body);
        Some(popover::modal(
            "usage-breakdown",
            viewport,
            card.into_any_element(),
        ))
    }

    /// Floating layers owned by the shell: the session context menu and the
    /// rename / delete-confirm dialogs.
    fn render_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays: Vec<AnyElement> = Vec::new();

        if let Some(overlay) = self.render_breakdown_dialog(viewport, cx) {
            overlays.push(overlay);
        }

        if self
            .state
            .read(cx)
            .scope
            .as_ref()
            .is_some_and(|status| status.merge_pending)
        {
            let card = popover::dialog_card(&theme)
                .w(px(480.0))
                .child(popover::dialog_title(&theme, "Sync your local work?"))
                .child(div().mt(px(8.0)).child(popover::dialog_body(
                    &theme,
                    "Syncing moves your local spaces, sessions, transcripts, and attachments into this account so they’re available on your other devices and iPhone.",
                )))
                .child(div().mt(px(8.0)).child(popover::dialog_body(
                    &theme,
                    "Repository files, harness and provider credentials, Jolt secrets, full tool inputs, journals, and detailed usage remain on this device.",
                )))
                .child(div().mt(px(8.0)).child(popover::dialog_body(
                    &theme,
                    "If you keep them Local, these sessions stay only on this device and cannot be viewed or controlled remotely.",
                )))
                .child(
                    div()
                        .mt(px(18.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Keep Local", "keep-local-data")
                                .id("keep-local-data")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.resolve_account_link(false, cx)
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Sync with account")
                                .id("sync-local-data")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.resolve_account_link(true, cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("local-account-link", viewport, card));
        }

        if let Some((chat_id, position)) = self.chat_menu.clone() {
            let rename_id = chat_id.clone();
            let archive_id = chat_id.clone();
            let delete_id = chat_id.clone();
            let menu = popover::popover_card(&theme)
                .w(px(170.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.chat_menu = None;
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .child(
                    popover::menu_row(&theme, false, format!("chat-menu-rename-{chat_id}"))
                        .id("chat-menu-rename")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_rename_chat(rename_id.clone(), cx)
                        }))
                        .child(icon(icons::PEN).size(px(16.0)).text_color(theme.text_muted))
                        .child(SharedString::from("Rename…")),
                )
                .child(
                    popover::menu_row(&theme, false, format!("chat-menu-archive-{chat_id}"))
                        .id("chat-menu-archive")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.archive_chat(archive_id.clone(), cx)
                        }))
                        .child(
                            icon(icons::ARCHIVE_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Archive")),
                )
                .child(popover::menu_separator())
                .child(
                    popover::menu_row(&theme, false, format!("chat-menu-delete-{chat_id}"))
                        .id("chat-menu-delete")
                        .text_color(theme.danger)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.confirm_delete_chat(delete_id.clone(), cx)
                        }))
                        .child(
                            icon(icons::TRASH_BIN_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.danger),
                        )
                        .child(SharedString::from("Delete…")),
                )
                .into_any_element();
            overlays.push(popover::menu_at("chat-context-menu", position, menu));
        }

        if let Some(dialog) = &mut self.rename_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                window.focus(&dialog.input.focus_handle(cx), cx);
            }
            let input = dialog.input.clone();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.rename_dialog = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Rename session"))
                .child(
                    div()
                        .mt(px(12.0))
                        .child(popover::dialog_field(input.into_any_element())),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "rename-chat-cancel")
                                .id("rename-chat-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rename_dialog = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Rename")
                                .id("rename-chat-save")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.submit_rename_chat(cx)),
                                ),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("rename-chat-dialog", viewport, card));
        }

        if let Some(menu) = self.render_tab_menu(cx) {
            overlays.push(menu);
        }
        overlays.extend(self.render_space_overlays(viewport, window, cx));
        if let Some(overlay) = self.render_add_space_overlay(viewport, window, cx) {
            overlays.push(overlay);
        }
        if let Some(overlay) = self.render_session_search_overlay(viewport, window, cx) {
            overlays.push(overlay);
        }

        if let Some(chat_id) = self.delete_confirm.clone() {
            let title = transcript::single_line(
                &self
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|c| c.id == chat_id)
                    .and_then(|c| c.title.clone())
                    .unwrap_or_else(|| "New session".into()),
            );
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Delete session?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    format!("\u{201C}{title}\u{201D} will be permanently deleted. This can\u{2019}t be undone."),
                )))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "delete-chat-cancel")
                                .id("delete-chat-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.delete_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Delete")
                                .id("delete-chat-confirm")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_chat(chat_id.clone(), cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("delete-chat-dialog", viewport, card));
        }

        overlays
    }

    fn resize_handle<T>(
        &self,
        id: &'static str,
        marker: fn() -> T,
        reset: fn(&mut Shell, &mut Context<Shell>),
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div>
    where
        T: 'static,
    {
        let hover = Theme::of(cx).border_strong;
        div()
            .id(id)
            .w(px(5.0))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            .hover(move |s| s.bg(hover))
            .on_drag(marker(), |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        reset(this, cx);
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            )
    }

    fn render_main(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme_owned = Theme::of(cx).clone();
        let theme = &theme_owned;
        let view = cx.entity_id();
        let theme_bg = theme.bg;
        let (border, text, faint) = (theme.border, theme.text, theme.text_faint);

        // Secondary routes show only their page outlet; chat-scoped composer,
        // transcript, terminal, and Changes chrome remain mounted on Chat.
        let secondary_outlet = match self.route {
            Route::Archived => Some(self.archived_outlet(cx)),
            Route::Settings(section) => Some(self.settings_outlet(section, cx)),
            Route::Chat => None,
        };
        if let Some(outlet) = secondary_outlet {
            return div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0().child(outlet))
                .into_any_element();
        }

        let _ = (text, border);
        let has_selection = self.state.read(cx).selected_chat.is_some();
        let has_spaces = !self.state.read(cx).spaces.is_empty();
        let space_name: SharedString = self
            .state
            .read(cx)
            .selected_space_row()
            .map(|s| s.display_name().to_string())
            .unwrap_or_default()
            .into();

        // Content outlet: selected chat → transcript; nothing selected → the
        // "Send a message to start" canvas with a watermark; no spaces at all
        // → the onboarding card. The composer sits below the first two
        // (new-chat mode mints the chat id on first send).
        let outlet: AnyElement = if has_selection {
            self.transcript.clone().into_any_element()
        } else if !has_spaces {
            // Onboarding (first boot / after the destructive wipe): no folders
            // to work in yet — one clear affordance.
            let _ = faint;
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in(
                    "no-spaces-canvas",
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(crate::ascii_mark::ascii_jolt_mark(
                            theme,
                            132.0,
                            crate::ascii_mark::AsciiMarkMotion::Idle,
                            view,
                            cx,
                        ))
                        .child(
                            div()
                                .mt(px(18.0))
                                .text_size(px(16.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(SharedString::from("Add a space to get started")),
                        )
                        .child(
                            div()
                                .mt(px(6.0))
                                .text_size(px(13.0))
                                .text_color(theme.text_muted.opacity(0.7))
                                .child(SharedString::from(
                                    "A space is a folder on one of your devices.",
                                )),
                        )
                        .child(
                            popover::btn_primary(&theme_owned, "Add a space")
                                .id("onboarding-add-space")
                                .mt(px(20.0))
                                .on_click(cx.listener(|this, _, _, cx| this.open_add_space(cx))),
                        ),
                ))
                .into_any_element()
        } else {
            // New-chat canvas: the dim violet Jolt mark over the centered
            // helper line, naming the space the session will start in.
            let helper: SharedString = if space_name.is_empty() {
                "Send a message to start a new session.".into()
            } else {
                format!("Send a message to start a session in {space_name}.").into()
            };
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in(
                    "new-chat-canvas",
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(crate::ascii_mark::ascii_jolt_mark(
                            theme,
                            132.0,
                            crate::ascii_mark::AsciiMarkMotion::Idle,
                            view,
                            cx,
                        ))
                        .child(
                            div()
                                .mt(px(18.0))
                                .text_size(px(14.0))
                                .text_color(theme.text_muted.opacity(0.6))
                                .child(helper),
                        ),
                ))
                .into_any_element()
        };

        let status = self.status_strip.clone();
        // File dropzone over the ENTIRE conversation column (transcript +
        // composer, not just the pill): dragging OS files anywhere across the
        // chat area shows the "Drop images to attach" veil; a drop stages the
        // files in the composer. `has_active_drag` gates the veil so a drag
        // that left the window (FileDrop Exited) can't strand it.
        let file_drag_active = self.file_drag_active && cx.has_active_drag();
        div()
            .id("chat-dropzone")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .on_drag_move::<gpui::ExternalPaths>(cx.listener(
                |this, e: &gpui::DragMoveEvent<gpui::ExternalPaths>, _, cx| {
                    let inside = e.bounds.contains(&e.event.position);
                    if this.file_drag_active != inside {
                        this.file_drag_active = inside;
                        cx.notify();
                    }
                },
            ))
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                this.file_drag_active = false;
                let paths = paths.paths().to_vec();
                this.composer
                    .update(cx, |composer, cx| composer.add_paths(paths, cx));
                cx.notify();
            }))
            .child(
                // The conversation fades out at its bottom edge instead of
                // hard-cutting against the composer — a gradient overlay from
                // transparent into the panel background.
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(outlet)
                    .child(
                        div()
                            .absolute()
                            .bottom_0()
                            .left_0()
                            .right(px(10.0))
                            .h(px(Theme::TRANSCRIPT_FADE_BAND))
                            .bg(gpui::linear_gradient(
                                0.0,
                                gpui::linear_color_stop(theme_bg, 0.0),
                                gpui::linear_color_stop(theme_bg.opacity(0.0), 1.0),
                            )),
                    )
                    .child(self.jump_to_bottom.clone()),
            )
            // Reserved status strip (h-6) — the WorkingIndicator lives here so
            // the composer below never shifts. Both live INSIDE the
            // conversation region, above the terminal dock.
            .child(status)
            .when(has_spaces, |el| el.child(self.composer.clone()))
            .child(self.render_terminal_container(cx))
            .when(file_drag_active, |el| {
                el.child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(theme.scrim().opacity(0.4 / 0.6))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .child("Drop images to attach"),
                )
            })
            .into_any_element()
    }

    /// Terminal panel dock at the main-column bottom: a 5px height-drag handle
    /// over the panel, the whole container height-animated 200 ms on toggle.
    fn render_terminal_container(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let target = self.terminal_target(cx);
        let tween = self.terminal_tween;
        if target <= 0.0 && tween.is_none() {
            return gpui::Empty.into_any_element();
        }
        // Defensive: an open flag needs its entity (and set_open) even if
        // toggle_terminal never created one.
        if self.terminal_open(cx) && self.terminal.is_none() {
            let panel = self.terminal_panel(cx);
            panel.update(cx, |panel, cx| panel.set_open(true, cx));
        }
        let Some(panel) = self.terminal.clone() else {
            return gpui::Empty.into_any_element();
        };
        let border = Theme::of(cx).border;
        let handle_hover = Theme::of(cx).border_strong;
        let height = self.settings.terminal_height;

        let handle = div()
            .id("terminal-resize")
            .h(px(5.0))
            .w_full()
            .flex_none()
            .cursor_row_resize()
            .hover(move |s| s.bg(handle_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, _| {
                    this.terminal_drag_anchor =
                        Some((f32::from(event.position.y), this.settings.terminal_height));
                }),
            )
            .on_drag(TerminalResize, |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        this.settings.terminal_height = TERMINAL_DEFAULT_HEIGHT;
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            );

        // Fixed-height inner clipped by the animated container: content never
        // reflows mid-transition (same trick as the side panes).
        let inner = div()
            .h(px(height))
            .w_full()
            .flex()
            .flex_col()
            .child(handle)
            .child(div().flex_1().min_h_0().child(panel));

        div()
            .w_full()
            .flex_none()
            .overflow_hidden()
            .border_t_1()
            .border_color(border)
            .h(px(self.eval_tween(tween, target)))
            .child(inner)
            .into_any_element()
    }

    fn render_expanded_changes(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let changes = self.changes_pane(cx);
        changes.update(cx, |changes, cx| changes.ensure_watch(cx));
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .mx(px(8.0))
            .mb(px(8.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.bg)
            .overflow_hidden()
            .child(changes)
            .into_any_element()
    }

    fn render_expanded_terminal(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let terminal = self.terminal_panel(cx);
        terminal.update(cx, |terminal, cx| terminal.set_open(true, cx));
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .mx(px(8.0))
            .mb(px(8.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.bg)
            .overflow_hidden()
            .child(terminal)
            .into_any_element()
    }

    /// Right "Changes" pane — hidden by default, drag-resizable; content is the
    /// lazy [`Changes`] diff viewer (created on first open).
    fn render_right_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let bg = theme.bg;
        let content: AnyElement = if self.right_pane_open(cx) {
            let changes = self.changes_pane(cx);
            // Idempotent — also covers a persisted-open pane on boot.
            changes.update(cx, |changes, cx| changes.ensure_watch(cx));
            changes.into_any_element()
        } else {
            gpui::Empty.into_any_element()
        };
        // Its OWN inset card (user request): the conversation card's right
        // gutter is the gap; padding (not margins) keeps the tweened width
        // container clean, and the resize grabber floats over the gap.
        let handle = self
            .resize_handle(
                "right-pane-resize",
                || RightPaneResize,
                |shell, _| shell.settings.right_pane_width = RIGHT_PANE_DEFAULT,
                cx,
            )
            .absolute()
            .top_0()
            .bottom_0()
            // INSIDE the width-clipped container (a negative inset was
            // clipped into unreachability — user-reported dead resize),
            // overlapping the card's left border.
            .left(px(0.0));
        let card = div()
            .size_full()
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .bg(bg)
            .overflow_hidden()
            .child(content);
        let target = self.right_target(cx);
        self.pane_container(
            self.right_tween,
            target,
            // Mirrors the conversation card's box exactly: flush under the
            // titlebar (no top pad), 8px bottom/right gutters — the
            // conversation card's own right margin is the 8px gap between the
            // two insets (user-reported height/gap mismatch).
            div()
                .h_full()
                .relative()
                .pb(px(8.0))
                .pr(px(8.0))
                .child(card)
                .child(handle)
                .into_any_element(),
        )
    }

    fn render_gate_card(&mut self, phase: &GatePhase, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let content: AnyElement = match phase {
            // Backend unreachable: quiet centered copy (jolt Gate `Failed`),
            // plus a Retry affordance (the native engine doesn't self-redial).
            GatePhase::Failed(error) => div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(Theme::SPACE_MD))
                .child(
                    div()
                        .text_size(px(14.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(error.clone())),
                )
                .child(
                    div()
                        .id("retry-engine")
                        .px(px(12.0))
                        .py(px(6.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(theme.border)
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.glass_hover()))
                        .on_click(cx.listener(|this, _, _, cx| this.retry_engine(cx)))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element(),
            // Login card centered on the grid: logo, copy, and a full-width
            // white Log in button.
            _ => div()
                .w(px(360.0))
                .px(px(32.0))
                .py(px(40.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.surface_card)
                .shadow_lg()
                .flex()
                .flex_col()
                .items_center()
                .text_center()
                .child(
                    icon(icons::JOLT_LOGO)
                        .size(px(36.0))
                        .text_color(theme.code_text),
                )
                .child(
                    div()
                        .mt(px(24.0))
                        .text_size(px(18.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child(SharedString::from("Log in to Jolt")),
                )
                .child(
                    div()
                        .mt(px(6.0))
                        .mb(px(24.0))
                        .text_size(px(13.0))
                        .line_height(px(19.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(
                            "This opens your browser to finish logging in — you'll come right back.",
                        )),
                )
                .child(
                    div()
                        .id("sign-in")
                        .w_full()
                        .h(px(36.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .bg(theme.text)
                        .text_size(px(14.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.on_solid)
                        .cursor_pointer()
                        .hover(|s| s.opacity(0.9))
                        .on_click(cx.listener(|this, _, _, cx| this.start_sign_in(cx)))
                        .child(SharedString::from("Log in")),
                )
                .into_any_element(),
        };
        div()
            .size_full()
            .relative()
            .bg(theme.bg)
            .child(grid_backdrop(&theme))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    // Keyed per phase so every gate swap replays the 0.5s
                    // entrance instead of mutating one animated element.
                    .child(motion::fade_in(
                        match phase {
                            GatePhase::SignIn => "gate-card-signin",
                            _ => "gate-card-failed",
                        },
                        div().child(content),
                    )),
            )
            .into_any_element()
    }

    /// Automatic first-sign-in setup. The organization is an internal tenancy
    /// detail, so the UI only reports progress or a retryable failure.
    fn render_org_gate(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.ensure_org_ui(cx);
        let theme = Theme::of(cx).clone();
        let Some(org) = self.org.as_ref() else {
            return Empty.into_any_element();
        };
        let error = org.error.clone();
        let card = div()
            .w(px(400.0))
            .px(px(32.0))
            .py(px(36.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_card)
            .shadow_lg()
            .flex()
            .flex_col()
            .child(
                icon(icons::JOLT_LOGO)
                    .size(px(28.0))
                    .text_color(theme.code_text),
            )
            .child(
                div()
                    .mt(px(20.0))
                    .text_size(px(18.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from("Setting up Jolt")),
            )
            .child(div().mt(px(10.0)).flex().items_center().gap(px(8.0)).when(
                error.is_none(),
                |el| {
                    el.child(loaders::activity_orb(
                        "account-setup-indicator",
                        &theme,
                        14.0,
                        cx.entity_id(),
                        cx,
                    ))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from("Finishing account setup…")),
                    )
                },
            ))
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .mt(px(10.0))
                        .text_size(px(12.0))
                        .line_height(px(17.0))
                        .text_color(theme.danger_muted)
                        .child(message),
                )
                .child(
                    div()
                        .id("account-setup-retry")
                        .mt(px(16.0))
                        .h(px(36.0))
                        .px(px(16.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .bg(theme.text)
                        .text_size(px(14.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.on_solid)
                        .cursor_pointer()
                        .hover(|s| s.opacity(0.9))
                        .on_click(cx.listener(|this, _, _, cx| this.provision_personal_org(cx)))
                        .child(SharedString::from("Retry")),
                )
            })
            .child(
                div().mt(px(24.0)).child(
                    div()
                        .id("org-signout")
                        .text_size(px(12.0))
                        .text_color(theme.text_muted.opacity(0.6))
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.text))
                        .on_click(cx.listener(|this, _, _, cx| this.sign_out(cx)))
                        .child(SharedString::from("Use a different account")),
                ),
            );

        div()
            .size_full()
            .relative()
            .bg(theme.bg)
            .child(grid_backdrop(&theme))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(motion::fade_in("org-gate-card", card)),
            )
            .into_any_element()
    }
}

/// Isolated from the shell so composer keystrokes invalidate this tiny strip,
/// not the sidebar and the rest of the window chrome.
impl Render for SessionStatusStrip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let state = self.state.read(cx);

        let strip = div()
            .h(px(Theme::STATUS_STRIP_HEIGHT))
            .flex_none()
            .w_full()
            .max_w(px(768.0))
            .mx_auto()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .px(px(Theme::SPACE_LG + 8.0))
            .text_size(px(11.0));

        let Some(chat_id) = state.selected_chat.clone() else {
            return strip.into_any_element();
        };
        let indicator = state.indicator_for(&chat_id, now);
        let queued_host = state
            .queued_send_offline_host_name(&chat_id, now)
            .map(str::to_owned);
        let compacting = state
            .session_for(&chat_id)
            .is_some_and(|session| session.compacting);
        let elapsed_secs = state
            .session_for(&chat_id)
            .and_then(|session| session.started_at)
            .map(|started| now.signed_duration_since(started).num_seconds())
            .unwrap_or(0);
        let composer = self.composer.read(cx);
        let sending = composer.is_sending();
        if composer.has_inline_progress() {
            return strip.into_any_element();
        }
        if let Some(host) = queued_host {
            return strip
                .text_color(theme.warning)
                .child(div().size(px(6.0)).rounded_full().bg(theme.warning))
                .child(SharedString::from(format!("Queued · {host} is offline")))
                .into_any_element();
        }

        match indicator {
            Indicator::Working if compacting => strip
                .child(loaders::activity_orb(
                    "compacting-indicator",
                    &theme,
                    14.0,
                    cx.entity_id(),
                    cx,
                ))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Compacting context…")),
                )
                .into_any_element(),
            Indicator::Working => {
                let word =
                    transcript::flavour_word(transcript::flavour_seed(&chat_id), elapsed_secs);
                strip
                    .child(loaders::activity_orb(
                        "working-indicator",
                        &theme,
                        14.0,
                        cx.entity_id(),
                        cx,
                    ))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!("{word}…"))),
                    )
                    .child(
                        div()
                            .text_color(theme.text_faint)
                            .child(SharedString::from(transcript::format_elapsed(elapsed_secs))),
                    )
                    .into_any_element()
            }
            Indicator::AwaitingInput => strip.into_any_element(),
            Indicator::Errored => strip
                .text_color(theme.danger)
                .child(SharedString::from("Run failed"))
                .into_any_element(),
            Indicator::None if sending => strip
                .child(loaders::activity_orb(
                    "sending-indicator",
                    &theme,
                    14.0,
                    cx.entity_id(),
                    cx,
                ))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Sending…")),
                )
                .into_any_element(),
            Indicator::None => strip.into_any_element(),
        }
    }
}

/// Isolated from the shell so wheel/touch frames only redraw the transcript
/// and this tiny overlay rather than rebuilding every session row.
impl Render for JumpToBottom {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.transcript.read(cx).jump_button_shown() {
            return Empty.into_any_element();
        }
        let theme = Theme::of(cx);
        div()
            .absolute()
            .bottom(px(-14.0))
            .left_0()
            .right(px(10.0))
            .flex()
            .justify_center()
            .child(motion::dialog_in(
                "jump-to-bottom",
                div()
                    .id("jump-to-bottom-btn")
                    .h(px(30.0))
                    .rounded_full()
                    .border_1()
                    .border_color(theme.border)
                    .shadow_md()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .pl(px(11.0))
                    .pr(px(13.0))
                    .cursor_pointer()
                    .bg(motion::hover_blend(
                        "jump-pill",
                        theme.surface_raised,
                        theme.surface_raised_hover,
                    ))
                    .on_hover(motion::hover_listener("jump-pill"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.transcript
                            .update(cx, |transcript, cx| transcript.jump_to_bottom(cx));
                    }))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from("↓")),
                    )
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme.text)
                            .child(SharedString::from("Scroll to bottom")),
                    ),
            ))
            .into_any_element()
    }
}

/// The sign-in gate's faint grid backdrop (jolt styles.css `.bg-grid`):
/// 44px hairlines at white 3.5%, with the radial mask approximated by edge
/// gradients back into the page background (gpui has no mask-image).
fn grid_backdrop(theme: &Theme) -> AnyElement {
    let line = crate::theme::hairline(0.035);
    let bg = theme.bg;
    const STEP: f32 = 44.0;
    const SPAN: f32 = 2640.0;
    let verticals = (1..(SPAN / STEP) as usize).map(|i| {
        div()
            .absolute()
            .left(px(i as f32 * STEP))
            .top_0()
            .bottom_0()
            .w(px(1.0))
            .bg(line)
    });
    let horizontals = (1..((SPAN * 0.75) / STEP) as usize).map(|i| {
        div()
            .absolute()
            .top(px(i as f32 * STEP))
            .left_0()
            .right_0()
            .h(px(1.0))
            .bg(line)
    });
    div()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .children(verticals)
        .children(horizontals)
        // Fade the grid back into the background toward the window edges.
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(120.0))
                .bg(gpui::linear_gradient(
                    180.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
                .h(px(260.0))
                .bg(gpui::linear_gradient(
                    0.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(px(200.0))
                .bg(gpui::linear_gradient(
                    90.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(200.0))
                .bg(gpui::linear_gradient(
                    270.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .into_any_element()
}

struct SessionActionTooltip(SharedString);

impl Render for SessionActionTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(5.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_raised)
            .text_size(px(11.0))
            .text_color(theme.text_muted)
            .child(self.0.clone())
    }
}

/// A 24px icon button for the titlebar strip.
fn window_control_button(
    id: &'static str,
    icon_path: &'static str,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let muted = theme.text_muted;
    let fade_key = format!("window-control-{id}");
    div()
        .id(id)
        .size(px(24.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        // Fade the hover wash.
        .bg(motion::hover_blend(
            &fade_key,
            theme.glass_hover().opacity(0.0),
            theme.glass_hover(),
        ))
        .on_hover(motion::hover_listener(fade_key))
        // Buttons in/over a titlebar drag strip must be EXCLUDED from the
        // strip's event surface entirely. `.occlude()` (gpui
        // `HitboxBehavior::BlockMouse`) makes the window hit-test STOP at the
        // button, so every `is_hovered`-guarded strip listener — the
        // mouse-down that arms the drag, the mouse-move that hands AppKit a
        // native drag session (`performWindowDragWithEvent:`, whose second
        // quick click zooms NATIVELY on macOS), and the `click_count == 2`
        // zoom handler — never fires with the pointer over a button. It also
        // removes the button's rect from the native Drag control-area
        // hit-test on Windows/Linux. The click-level stop_propagation is
        // zed's ButtonLike belt on top. Double-click on EMPTY strip space
        // still zooms — nothing occludes it there.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(icon(icon_path).size(px(16.0)).text_color(muted))
}

const WINDOWS_CAPTION_BUTTON_WIDTH: f32 = 36.0;
const WINDOWS_CAPTION_WIDTH: f32 = WINDOWS_CAPTION_BUTTON_WIDTH * 3.0;

fn titlebar_right_padding(is_windows: bool, base: f32) -> f32 {
    base + if is_windows {
        WINDOWS_CAPTION_WIDTH
    } else {
        0.0
    }
}

/// A Windows-owned caption target using the same system glyphs and native
/// non-client hit-test areas as GPUI/Zed's platform titlebar.
fn windows_caption_button(
    id: &'static str,
    glyph: &'static str,
    area: WindowControlArea,
    theme: &Theme,
    close: bool,
) -> impl IntoElement {
    let (hover_bg, hover_fg, active_bg, active_fg) = if close {
        let red: gpui::Hsla = gpui::rgb(0xe81123).into();
        (
            red,
            gpui::white(),
            red.opacity(0.8),
            gpui::white().opacity(0.8),
        )
    } else {
        (
            theme.glass_hover(),
            theme.text,
            theme.glass_hover().opacity(0.7),
            theme.text,
        )
    };
    div()
        .id(id)
        .w(px(WINDOWS_CAPTION_BUTTON_WIDTH))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(10.0))
        .text_color(theme.text)
        .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
        .active(move |style| style.bg(active_bg).text_color(active_fg))
        .occlude()
        .window_control_area(area)
        .child(glyph)
}

/// A titlebar history button. Enabled, it is a normal window-control button;
/// disabled, it dims to 35% opacity and ignores
/// the pointer (`disabled:pointer-events-none disabled:opacity-35`).
fn nav_history_button(
    id: &'static str,
    icon_path: &'static str,
    enabled: bool,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    if !enabled {
        return div()
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            // Even disabled it reads as a control — occlude so double-clicks
            // on it don't fall through to the titlebar strip's zoom handler.
            .occlude()
            .child(
                icon(icon_path)
                    .size(px(16.0))
                    .text_color(theme.text_muted.opacity(0.35)),
            )
            .into_any_element();
    }
    window_control_button(id, icon_path, theme, on_click).into_any_element()
}

/// A 28px icon button for the main-panel header.
fn header_icon_button(
    id: &'static str,
    icon_path: &'static str,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let muted = theme.text_muted;
    let fade_key = format!("header-icon-{id}");
    div()
        .id(id)
        .size(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        // Fade the hover wash.
        .bg(motion::hover_blend(
            &fade_key,
            crate::theme::wash(0.0),
            crate::theme::wash(0.11),
        ))
        .on_hover(motion::hover_listener(fade_key))
        // Same occlusion + click-swallowing as [`window_control_button`]: this
        // button sits inside the chat header's titlebar drag region, so its
        // rect must be carved out of the strip's drag/double-click surface.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(icon(icon_path).size(px(16.0)).text_color(muted))
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        // The shell tone (jolt `.frost`): the surface the sidebar sits on and
        // the main panel floats over as an inset rounded card. On macOS the
        // window background is the blurred desktop (lib.rs `Blurred`), so the
        // frost paints translucent — the sidebar and card margins read as
        // glass while the opaque card keeps text off it.
        let (frost, text, font, interface_font_size) = (
            theme.glass(),
            theme.text,
            theme.font_sans.clone(),
            theme.font_sizes.interface,
        );
        let gate = self
            .debug_gate
            .clone()
            .unwrap_or_else(|| self.state.read(cx).gate());

        // Fullscreen hides the macOS traffic lights — reflow the control
        // cluster with a 200ms ease-out tween (§1.1). A fullscreen transition
        // resizes the window, which re-renders us, so polling here is exact.
        let fullscreen = window.is_fullscreen();
        if self.fullscreen != Some(fullscreen) {
            if self.fullscreen.is_some() && cfg!(target_os = "macos") {
                self.titlebar_tween = Some(WidthTween::new(
                    titlebar_cluster_start(!fullscreen),
                    titlebar_cluster_start(fullscreen),
                ));
            }
            self.fullscreen = Some(fullscreen);
        }
        // Manual tween drive bookkeeping for this pass (see [`WidthTween`]).
        self.reduced_motion = motion::reduced_motion(cx);
        self.motion_active.set(false);

        // App hotkeys (mod-e/b/`) dispatch through the window focus
        // chain — with nothing focused they go dead. Land initial focus on the
        // composer, and whenever focus is lost with no successor (e.g. the
        // focused element unmounted), route it back there.
        if self.focus_sub.is_none() {
            self.focus_sub =
                Some(
                    cx.on_focus_lost(window, |this: &mut Shell, window, cx| match this.route {
                        Route::Chat => window.focus(&this.composer.focus_handle(cx), cx),
                        Route::Archived | Route::Settings(_) => {
                            window.focus(&this.settings_focus, cx)
                        }
                    }),
                );
        }
        if matches!(gate, GatePhase::Ready) && window.focused(cx).is_none() {
            match self.route {
                Route::Chat => window.focus(&self.composer.focus_handle(cx), cx),
                Route::Archived | Route::Settings(_) => window.focus(&self.settings_focus, cx),
            }
        }

        let root = div()
            .id("shell-root")
            .when(!matches!(self.route, Route::Chat), |root| {
                root.track_focus(&self.settings_focus)
            })
            .relative()
            .flex()
            .flex_row()
            .size_full()
            .bg(frost)
            .text_color(text)
            .font_family(font)
            .text_size(px(f32::from(interface_font_size)))
            .on_drag_move(cx.listener(Self::on_sidebar_drag))
            .on_drag_move(cx.listener(Self::on_right_pane_drag))
            .on_drag_move(cx.listener(Self::on_terminal_drag))
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                let shell_overlay_open = this.user_menu_open
                    || this.chat_menu.is_some()
                    || this.rename_dialog.is_some()
                    || this.breakdown_dialog.is_some()
                    || this.delete_confirm.is_some()
                    || this.space_menu.is_some()
                    || this.spaces_menu.is_some()
                    || this.tab_menu.is_some()
                    || this.rename_space_dialog.is_some()
                    || this.delete_space_confirm.is_some()
                    || this.add_space.is_some()
                    || this.session_search.is_some();
                if event.keystroke.key == "escape" && this.changes_expanded {
                    this.set_changes_expanded(false, cx);
                    cx.stop_propagation();
                    return;
                }
                if event.keystroke.key == "escape" && this.terminal_expanded {
                    this.set_terminal_expanded(false, cx);
                    cx.stop_propagation();
                    return;
                }
                if event.keystroke.key == "escape"
                    && matches!(this.route, Route::Chat)
                    && this.state.read(cx).selected_chat.is_none()
                    && !shell_overlay_open
                {
                    let had_tabs = !this.open_tab_ids(cx).is_empty();
                    this.close_current_tab(cx);
                    if had_tabs {
                        cx.stop_propagation();
                    }
                }
            }))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && !matches!(this.route, Route::Chat) {
                    this.close_secondary_page(cx);
                }
            }))
            // Panel hotkeys are chat-scoped chrome and no-op on secondary
            // pages; the terminal panel only mounts on session routes. The sidebar
            // toggle stays live everywhere.
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                if matches!(this.route, Route::Chat) {
                    this.toggle_terminal(window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &OpenSpacesDropdown, window, cx| {
                this.open_spaces_dropdown(window, cx)
            }))
            .on_action(cx.listener(|this, _: &NewSession, _, cx| this.open_new_session(cx)))
            .on_action(cx.listener(|this, _: &ClearInput, _, cx| {
                this.composer
                    .update(cx, |composer, cx| composer.clear_input(cx));
            }))
            .on_action(cx.listener(|this, _: &CloseCurrentTab, _, cx| {
                if matches!(this.route, Route::Chat) {
                    // On an empty new-session canvas Cmd-W is deliberately a
                    // no-op; on secondary pages it propagates to Close Window.
                    this.close_current_tab(cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &PreviousTranscriptTurn, _, cx| {
                if matches!(this.route, Route::Chat) && this.state.read(cx).selected_chat.is_some()
                {
                    this.transcript.update(cx, |transcript, cx| {
                        transcript.navigate_rail(rail::RailDirection::Previous, cx)
                    });
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &NextTranscriptTurn, _, cx| {
                if matches!(this.route, Route::Chat) && this.state.read(cx).selected_chat.is_some()
                {
                    this.transcript.update(cx, |transcript, cx| {
                        transcript.navigate_rail(rail::RailDirection::Next, cx)
                    });
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, action: &SelectTab, _, cx| {
                this.select_tab_at_position(action.0, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| {
                this.open_settings(SettingsSection::Devices, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleChanges, _, cx| {
                if matches!(this.route, Route::Chat) {
                    this.toggle_right_pane(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &AddSpacePalette, _, cx| {
                if this.add_space.is_some() {
                    this.add_space = None;
                    cx.notify();
                } else {
                    this.open_add_space(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SearchSessionsPalette, _, cx| {
                if this.session_search.is_some() {
                    this.session_search = None;
                    cx.notify();
                } else {
                    this.open_session_search(cx);
                }
            }));

        #[cfg(any(debug_assertions, feature = "debug-ui"))]
        let root = root.on_action(cx.listener(|this, _: &TogglePerformanceHud, _, cx| {
            if this.performance_hud.is_some() {
                this.performance_hud = None;
            } else {
                this.performance_hud = Some(cx.new(PerformanceHud::new));
            }
            cx.notify();
        }));

        let root = match &gate {
            GatePhase::Ready => {
                // Focus is a sync signal: on the rising edge of window
                // activation, nudge every open room to verify liveness — a
                // broadcast-deaf socket (accepted writes, runtime pongs,
                // nothing delivered; 2026-08-04 incident) then heals within
                // seconds of the user looking at the app rather than waiting
                // out the background probe cadence.
                let window_active = window.is_window_active();
                if window_active && !self.was_window_active {
                    self.state.update(cx, |s, cx| s.probe_sync(cx));
                }
                self.was_window_active = window_active;
                // A run finishing while you're LOOKING at the session must not
                // badge "completed" until you leave and return — mark it seen
                // live while the window is active (idempotent guard inside;
                // one extra frame settles it).
                if window_active {
                    let unseen_selected = {
                        let s = self.state.read(cx);
                        s.selected_chat_row()
                            .filter(|c| c.unseen())
                            .map(|c| c.id.clone())
                    };
                    if let Some(chat_id) = unseen_selected {
                        self.state
                            .update(cx, |s, cx| s.mark_chat_seen(&chat_id, cx));
                    }
                }
                // Capture knob: `JOLT_OPEN_DIALOG=model` pops the combined
                // harness/model menu (needs `window`, so it fires here rather
                // than in `on_state_changed`).
                if self.debug_dialog.as_deref() == Some("model") {
                    self.debug_dialog = None;
                    self.composer
                        .update(cx, |c, cx| c.debug_open_model_menu(window, cx));
                }
                // MessageRail width gate: hide below 48rem of main-panel width.
                let viewport = f32::from(window.viewport_size().width);
                let main_width = viewport - self.sidebar_target() - self.right_target(cx) - 10.0;
                self.transcript.update(cx, |t, cx| {
                    t.set_rail_enabled(rail::rail_visible(main_width), cx)
                });

                // The expanded Changes view replaces the app body while the
                // titlebar remains available for navigation.
                let on_chat = matches!(self.route, Route::Chat);
                let expanded_changes = on_chat && self.changes_expanded && self.right_pane_open(cx);
                let expanded_terminal = on_chat && self.terminal_expanded && self.terminal_open(cx);
                let expanded_panel = expanded_changes || expanded_terminal;
                let sidebar = if expanded_panel {
                    Empty.into_any_element()
                } else {
                    self.render_sidebar(cx)
                };
                let sidebar_handle = self.resize_handle(
                    "sidebar-resize",
                    || SidebarResize,
                    |shell, _| shell.settings.sidebar_width = SIDEBAR_DEFAULT,
                    cx,
                );
                let main = if expanded_panel {
                    Empty.into_any_element()
                } else {
                    self.render_main(cx)
                };
                // The Changes pane is chat-scoped chrome, so secondary routes
                // never render it. The per-session open flags stay
                // intact for the return trip.
                let right: AnyElement = if on_chat && !expanded_panel {
                    self.render_right_pane(cx)
                } else {
                    Empty.into_any_element()
                };
                let overlays = self.render_overlays(window.viewport_size(), window, cx);
                // The signature frame: the conversation card and — when the
                // changes pane is open — a SECOND inset card beside it, both
                // rounded hairline-bordered floats on the frost shell (the
                // changes card is built inside `render_right_pane`).
                let theme = Theme::of(cx);
                // Margins, radius, and border color melt over the same 200ms
                // ease-out as the sidebar width. Collapsed removes margins and
                // radius and makes the border transparent; its width remains so
                // layout never jumps by a hairline.
                let border_color = theme.border;
                let card = div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .overflow_hidden()
                    .bg(theme.bg)
                    .border_1()
                    .child(main);
                // Manual drive on the SAME clock as the sidebar width tween.
                // Crucially there is no `with_animation` wrapper here: the
                // wrapper's epoch-keyed id used to change every card
                // descendant's global element-id path on each toggle, which
                // reset gpui's per-element animation state and REPLAYED any
                // stale pane/terminal tween from t=0 (the changes pane slid
                // ~100px under the clip mid-toggle — round-6 §2/§3).
                //
                // The inset card persists in EVERY state (user request): top
                // gutter under the unified titlebar, constant left/right/
                // bottom gutters, constant radius + hairline — the 8px left
                // gap holds whether it borders the sidebar or the window edge.
                // No top margin: the titlebar's own internal air (44px bar,
                // 28px tabs) is the gap — an extra gutter read as a hole
                // between the header and the app (user report).
                // The right margin is the window gutter when the changes
                // pane is closed, but the SEAM between the two inset cards
                // when it's open — a full gutter there read double-wide next
                // to the two borders it separates (user report).
                let right_gap = if on_chat && self.right_pane_open(cx) {
                    4.0
                } else {
                    8.0
                };
                let card: AnyElement = card
                    .mb(px(8.0))
                    .mr(px(right_gap))
                    .ml(px(8.0))
                    .rounded(px(12.0))
                    .border_color(border_color)
                    .into_any_element();
                // The whole app page is one keyed entrance: arriving from the
                // splash or any gate fades the page in; the
                // splash-out crossfades over it on boot.
                // The sidebar resize handle FLOATS over the sidebar/card seam
                // (zero layout width, same idiom as the changes-pane grabber)
                // so the sidebar's right gutter stays exactly as wide as its
                // left one — a 5px flex child here read as lopsided spacing.
                let sidebar_seam = div()
                    .w(px(0.0))
                    .h_full()
                    .flex_none()
                    .relative()
                    .child(sidebar_handle.absolute().top_0().bottom_0().left(px(-2.0)));
                let title_bar = self.render_title_bar(cx);
                // Sidebar tone: a slightly lighter column behind the sidebar,
                // spanning the FULL window height (under the traffic lights,
                // through the titlebar, down to the bottom edge). Its width
                // rides the same tween as the sidebar, so the tone melts away
                // with the collapse instead of vanishing in a frame.
                let sidebar_now = if expanded_panel {
                    0.0
                } else {
                    self.eval_tween(self.sidebar_tween, self.sidebar_target())
                };
                // Hairline on its right edge — full height like the tone,
                // so the sidebar column reads as its own surface.
                let sidebar_tone = div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px(sidebar_now))
                    .bg(crate::theme::wash(0.05))
                    .border_r_1()
                    .border_color(border_color);
                let body: AnyElement = if expanded_changes {
                    self.render_expanded_changes(cx)
                } else if expanded_terminal {
                    self.render_expanded_terminal(cx)
                } else {
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_row()
                        .child(sidebar)
                        .child(sidebar_seam)
                        .child(card)
                        .child(right)
                        .into_any_element()
                };
                let page = div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(title_bar)
                    .child(body)
                    .child(self.render_titlebar_cluster(cx))
                    .children(overlays);
                root.child(sidebar_tone)
                    .child(motion::fade_in("phase-app", page))
            }
            GatePhase::Loading => root, // splash overlay covers boot
            GatePhase::OrgGate => {
                let card = self.render_org_gate(cx);
                root.child(card)
            }
            phase @ (GatePhase::Failed(_) | GatePhase::SignIn) => {
                let card = self.render_gate_card(phase, cx);
                root.child(card)
            }
        };

        // A manually-driven tween is mid-flight: keep frames coming (the same
        // scheduling `with_animation` would have requested). Hover color fades
        // ride the same clock; their once-per-frame tick lives here (this is
        // the window's root render — it runs exactly once per frame).
        if self.motion_active.get() | motion::hover_fades_active() {
            window.request_animation_frame();
        }

        // Boot splash overlay: visible → crossfades out on Ready → removed.
        let root = match self.splash {
            SplashPhase::Visible => {
                let theme = Theme::of(cx).clone();
                let view = cx.entity_id();
                root.child(loaders::splash_overlay(&theme, false, view, cx))
            }
            SplashPhase::FadingOut => {
                let theme = Theme::of(cx).clone();
                let view = cx.entity_id();
                root.child(loaders::splash_overlay(&theme, true, view, cx))
            }
            SplashPhase::Gone => root,
        };

        // Caption controls are shell-level chrome, not Ready-page content:
        // keep them above the splash and every auth/org/error gate as well as
        // the full application. Gate pages also need a native drag surface
        // because they do not render the unified tabs/settings titlebar.
        let root = if matches!(gate, GatePhase::Ready) || !cfg!(target_os = "windows") {
            root
        } else {
            root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(Theme::TITLEBAR_HEIGHT))
                    .window_control_area(WindowControlArea::Drag),
            )
        };
        let root = root.children(self.render_windows_caption_controls(window, cx));
        let root = root.child(crate::toast::layer(cx));
        #[cfg(any(debug_assertions, feature = "debug-ui"))]
        let root = if let Some(hud) = self.performance_hud.clone() {
            root.child(hud)
        } else {
            root
        };
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_usage_warnings_match_account_meter_thresholds() {
        assert_eq!(usage_warning_level(0.79), UsageWarningLevel::Normal);
        assert_eq!(usage_warning_level(0.80), UsageWarningLevel::Warning);
        assert_eq!(usage_warning_level(0.94), UsageWarningLevel::Warning);
        assert_eq!(usage_warning_level(0.95), UsageWarningLevel::Danger);
    }

    #[test]
    fn usage_breakdowns_merge_devices_by_day_and_model() {
        let row = |tokens| UsageBreakdownRow {
            harness: HarnessId::Pi,
            model: "anthropic/sonnet".into(),
            cwd: "/repo".into(),
            sessions: 1,
            calls: 1,
            input_tokens: tokens,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            cost_usd: Some(0.25),
        };
        let report = |device: &str, tokens| UsageBreakdown {
            device_id: device.into(),
            days: 30,
            sessions: 1,
            calls: 1,
            input_tokens: tokens,
            cost_usd: Some(0.25),
            activity: vec![UsageDay {
                day: "2026-08-06".into(),
                tokens,
                calls: 1,
                cost_usd: Some(0.25),
            }],
            rows: vec![row(tokens)],
            ..UsageBreakdown::default()
        };
        let merged = merge_breakdowns(30, vec![report("a", 10), report("b", 20)]);
        assert_eq!(merged.sessions, 2);
        assert_eq!(merged.total_tokens(), 30);
        assert_eq!(merged.cost_usd, Some(0.5));
        assert_eq!(merged.activity[0].tokens, 30);
        assert_eq!(merged.rows[0].sessions, 2);
        assert_eq!(merged.rows[0].total_tokens(), 30);
    }

    #[test]
    fn titlebar_cluster_clears_traffic_lights() {
        // The cluster clears the traffic lights and reclaims the inset
        // when fullscreen hides them.
        assert_eq!(titlebar_cluster_start(false), 88.0);
        assert_eq!(titlebar_cluster_start(true), 12.0);
    }

    #[test]
    fn titlebar_spacer_selects_per_platform_and_fullscreen() {
        // macOS, lights visible: spacer fills up to the 88px cluster start.
        assert_eq!(titlebar_spacer_width(true, false, 10.0), 78.0);
        assert_eq!(titlebar_spacer_width(true, false, 12.0), 76.0);
        assert_eq!(titlebar_spacer_width(true, false, 26.0), 62.0);
        // macOS fullscreen: the inset animates away (clamped at zero when the
        // strip's own padding already exceeds the 12px cluster start).
        assert_eq!(titlebar_spacer_width(true, true, 10.0), 2.0);
        assert_eq!(titlebar_spacer_width(true, true, 26.0), 0.0);
        // Linux / Windows: never any inset.
        assert_eq!(titlebar_spacer_width(false, false, 10.0), 0.0);
        assert_eq!(titlebar_spacer_width(false, true, 10.0), 0.0);
    }

    #[test]
    fn windows_caption_controls_reserve_titlebar_space() {
        assert_eq!(titlebar_right_padding(true, 16.0), 124.0);
        assert_eq!(titlebar_right_padding(false, 16.0), 16.0);
    }

    #[cfg(any(debug_assertions, feature = "debug-ui"))]
    #[test]
    fn performance_hud_hotkey_parses_for_the_current_platform() {
        let keymap = KeymapConfig::default();
        assert!(Keystroke::parse(&platform_combo(keymap.get(ShortcutId::PerformanceHud))).is_ok());
    }

    #[test]
    fn tab_hotkeys_cover_eight_positions_and_last() {
        let mut keymap = KeymapConfig::default();
        keymap.set(ShortcutId::SelectTab1, "mod-shift-1".into());
        let bindings = tab_key_bindings(&keymap);
        assert_eq!(bindings.len(), ShortcutId::TAB_SELECTION.len());

        for (index, binding) in bindings.iter().enumerate() {
            let id = ShortcutId::TAB_SELECTION[index];
            let expected =
                Keystroke::parse(&platform_combo(keymap.get(id))).expect("tab hotkey must parse");
            let actual = binding
                .keystrokes()
                .iter()
                .map(|keystroke| keystroke.inner().clone())
                .collect::<Vec<_>>();
            assert_eq!(actual, vec![expected]);
            assert_eq!(
                binding.action().as_any().downcast_ref::<SelectTab>(),
                Some(&SelectTab(index))
            );
        }
    }

    #[test]
    fn cluster_clearance_clears_the_overlay_buttons() {
        // Linux: buttons at 10..86; a 16px-padded header needs 78 more px to
        // put content at 86 + 8 breathing room.
        assert_eq!(cluster_clearance(false, false, 16.0), 78.0);
        assert_eq!(cluster_clearance(false, false, 10.0), 84.0);
        // macOS: buttons start at the 88px traffic-light cluster start.
        assert_eq!(
            cluster_clearance(true, false, 16.0),
            88.0 + 76.0 + 8.0 - 16.0
        );
        // macOS fullscreen: cluster reclaims the inset (starts at 12).
        assert_eq!(
            cluster_clearance(true, true, 16.0),
            12.0 + 76.0 + 8.0 - 16.0
        );
    }

    // ---- per-session panel flags ----

    #[test]
    fn session_panels_default_closed_per_chat() {
        let panels = SessionPanels::default();
        assert_eq!(panels.get("a"), ChatPanels::default());
        assert!(!panels.get("a").terminal_open);
        assert!(!panels.get("a").changes_open);
        // The new-chat canvas ("" key) is its own session, also closed.
        assert!(!panels.get("").terminal_open);
    }

    #[test]
    fn session_panels_flags_are_chat_scoped() {
        let mut panels = SessionPanels::default();
        // Opening the terminal in chat A opens it ONLY in chat A.
        assert!(panels.toggle_terminal("a"));
        assert!(panels.get("a").terminal_open);
        assert!(!panels.get("b").terminal_open);
        assert!(!panels.get("").terminal_open);
        // Changes pane in B is independent of A's terminal.
        assert!(panels.toggle_changes("b"));
        assert!(panels.get("b").changes_open);
        assert!(!panels.get("b").terminal_open);
        assert!(!panels.get("a").changes_open);
        // Switching back to A restores A's state untouched.
        assert!(panels.get("a").terminal_open);
        // Toggling off round-trips.
        assert!(!panels.toggle_terminal("a"));
        assert!(!panels.get("a").terminal_open);
    }

    #[test]
    fn closing_an_exited_terminal_only_changes_its_chat() {
        let mut panels = SessionPanels::default();
        panels.toggle_terminal("a");
        panels.toggle_terminal("b");

        assert!(panels.close_terminal("a"));
        assert!(!panels.get("a").terminal_open);
        assert!(panels.get("b").terminal_open);
        assert!(!panels.close_terminal("a"), "closing is idempotent");
    }

    #[test]
    fn session_panels_both_flags_coexist_per_chat() {
        let mut panels = SessionPanels::default();
        panels.toggle_terminal("a");
        panels.toggle_changes("a");
        assert_eq!(
            panels.get("a"),
            ChatPanels {
                terminal_open: true,
                changes_open: true
            }
        );
        assert_eq!(panels.get("b"), ChatPanels::default());
    }

    // ---- sidebar resort FLIP diff (§1.6) ----

    fn keys(list: &[(&str, f32)]) -> Vec<(String, f32)> {
        list.iter().map(|(k, h)| (k.to_string(), *h)).collect()
    }

    #[test]
    fn resort_offsets_empty_when_order_unchanged() {
        let order = keys(&[("a", 29.0), ("b", 29.0), ("c", 45.0)]);
        assert!(resort_offsets(&order, &order, 2.0).is_empty());
    }

    #[test]
    fn resort_offsets_activity_moves_row_to_top() {
        // c (bottom, y=62) jumps to top: c glides down-from-above? No — c's
        // old y is 62, new y is 0 → starts +62 below… offset = old - new = +62,
        // painted at +62 decaying to 0 (a glide UP into place). a and b shift
        // down by c's height + gap (31).
        let old = keys(&[("a", 29.0), ("b", 29.0), ("c", 29.0)]);
        let new = keys(&[("c", 29.0), ("a", 29.0), ("b", 29.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        assert_eq!(offsets.get("c"), Some(&62.0));
        assert_eq!(offsets.get("a"), Some(&-31.0));
        assert_eq!(offsets.get("b"), Some(&-31.0));
    }

    #[test]
    fn resort_offsets_respect_heights_and_gap() {
        // Tall row (45px) swaps with a short one (29px).
        let old = keys(&[("tall", 45.0), ("short", 29.0)]);
        let new = keys(&[("short", 29.0), ("tall", 45.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        // short: old y 47 → new y 0; tall: old y 0 → new y 31.
        assert_eq!(offsets.get("short"), Some(&47.0));
        assert_eq!(offsets.get("tall"), Some(&-31.0));
    }

    #[test]
    fn resort_offsets_ignore_added_and_removed_keys() {
        let old = keys(&[("a", 29.0), ("gone", 29.0), ("b", 29.0)]);
        let new = keys(&[("new", 29.0), ("a", 29.0), ("b", 29.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        // "new" has no old position (fades in instead); "gone" just goes.
        assert!(!offsets.contains_key("new"));
        assert!(!offsets.contains_key("gone"));
        // a: old 0 → new 31 (pushed down by the insert); b: 62 → 62 (gone's
        // slot replaced by "new" of equal height — no move, no entry).
        assert_eq!(offsets.get("a"), Some(&-31.0));
        assert_eq!(offsets.get("b"), None);
    }

    #[test]
    fn resort_glide_spec_matches_original() {
        // §1.6: 260ms cubic-bezier(0.22, 1, 0.36, 1).
        assert_eq!(RESORT.duration_ms, 260);
        assert_eq!(RESORT.curve, motion::EASE_RESORT);
    }

    // ---- navigation history (titlebar back/forward) ----

    fn chat(id: &str) -> NavEntry {
        NavEntry::Chat(id.to_string())
    }

    #[test]
    fn nav_history_starts_with_nothing_to_walk() {
        let nav = NavHistory::new(chat(""));
        assert!(!nav.can_back());
        assert!(!nav.can_forward());
        assert_eq!(*nav.current(), chat(""));
    }

    #[test]
    fn nav_push_then_back_and_forward() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("b"));
        nav.push(NavEntry::Settings(SettingsSection::Devices));
        assert!(nav.can_back());
        assert!(!nav.can_forward());

        // Back walks toward the oldest entry without dropping anything.
        assert_eq!(
            nav.back(),
            Some(chat("b")),
            "back lands on the previous route"
        );
        assert_eq!(nav.back(), Some(chat("a")));
        assert!(!nav.can_back());
        assert!(nav.can_forward());
        assert_eq!(nav.back(), None, "past the oldest entry is a no-op");

        // Forward retraces the same path.
        assert_eq!(nav.forward(), Some(chat("b")));
        assert_eq!(
            nav.forward(),
            Some(NavEntry::Settings(SettingsSection::Devices))
        );
        assert!(!nav.can_forward());
        assert_eq!(nav.forward(), None);
    }

    #[test]
    fn nav_push_dedups_the_current_route() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("a"));
        nav.push(chat("a"));
        assert_eq!(nav.len(), 1, "re-selecting the current route never stacks");
        nav.push(NavEntry::Settings(SettingsSection::Agents));
        nav.push(NavEntry::Settings(SettingsSection::Agents));
        assert_eq!(nav.len(), 2);
    }

    #[test]
    fn nav_push_truncates_the_forward_branch() {
        // a → b → c, back to a, then push d: browser semantics remove the b/c
        // branch.
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("b"));
        nav.push(chat("c"));
        nav.back();
        nav.back();
        assert_eq!(*nav.current(), chat("a"));
        assert!(nav.can_forward());
        nav.push(chat("d"));
        assert!(!nav.can_forward(), "the old branch is unreachable");
        assert_eq!(nav.len(), 2);
        assert_eq!(nav.back(), Some(chat("a")));
        assert_eq!(nav.forward(), Some(chat("d")));
    }

    #[test]
    fn nav_replace_swaps_in_place() {
        // The boot auto-select replaces the untouched canvas entry, so Back
        // stays disabled after landing in the last-used chat.
        let mut nav = NavHistory::new(chat(""));
        nav.replace(chat("boot"));
        assert_eq!(nav.len(), 1);
        assert_eq!(*nav.current(), chat("boot"));
        assert!(!nav.can_back());
    }

    #[test]
    fn nav_settings_sections_are_distinct_entries() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(NavEntry::Settings(SettingsSection::Devices));
        nav.push(NavEntry::Settings(SettingsSection::Hotkeys));
        assert_eq!(nav.len(), 3, "section changes are navigations");
        assert_eq!(
            nav.back(),
            Some(NavEntry::Settings(SettingsSection::Devices))
        );
        assert_eq!(nav.back(), Some(chat("a")));
    }
}
