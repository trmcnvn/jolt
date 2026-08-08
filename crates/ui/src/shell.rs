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
    Keystroke, ListAlignment, ListState, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Point,
    Render, SharedString, Subscription, Task, Window, WindowControlArea, actions, div, list,
    prelude::*, px,
};

use gpui_tokio::Tokio;
use jolt_api::{
    ApplyHarnessUpdate, ChatSection, EnsurePersonalOrg, ListAgentAccounts, Mutate, QueryChats,
    RegenerateChatTitle, ScopeKind, SignIn, SignOut, UsageBreakdownRequest, call as call_api,
};
use jolt_proto::{
    CostProvenance, HarnessId, HarnessUpdateState, HarnessUpdateStatus, UsageBreakdown,
    UsageBreakdownRow, UsageDay,
};

use crate::changes::{Changes, ChangesEvent};
use crate::composer::{Composer, ComposerEvent, ComposerInput, ComposerInputEvent};
#[cfg(any(debug_assertions, feature = "debug-ui"))]
use crate::debug::{PerformanceHud, TogglePerformanceHud};
use crate::icons::{self, icon};
use crate::loaders;
use crate::motion::{self, AnimationExt as _, MotionSpec, RESIZE, SPLASH_OUT};
use crate::pickers::Pickers;
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
    TERMINAL_DEFAULT_HEIGHT, UiSettings, display_combo, platform_combo,
};
use crate::state::{
    AppState, ConnectionStatus, EngineBootConfig, EngineConnector, GatePhase, Indicator,
    format_time_ago,
};
use crate::terminal::panel::{
    CloseTerminalTab, NewTerminalTab, TerminalPanel, TerminalPanelEvent, ToggleTerminal,
    clamp_terminal_height,
};
use crate::theme::Theme;
use crate::toast::{Toast, ToastAction, ToastKind};
use crate::transcript::{self, Transcript, TranscriptEvent};

mod account;
mod header;
mod layout;
mod navigation;
mod panes;
mod spaces;
mod transcript_search;
mod updates;
mod usage;

use spaces::{AddSpaceFlow, ArchivedChatRow, RenameSpaceDialog, SessionSearchFlow, SpacesMenu};
use transcript_search::TranscriptSearchFlow;

actions!(
    shell,
    [
        NewSession,
        ClearInput,
        PreviousTranscriptTurn,
        NextTranscriptTurn,
        OpenSettings,
        OpenSpacesDropdown,
        ToggleSidebar,
        ToggleChanges,
        AddSpacePalette,
        SearchThreadsPalette,
        SearchTranscriptPalette
    ]
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, gpui::Action)]
#[action(namespace = shell, no_json, no_register)]
struct SelectSession(usize);

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
                &keymap.search_transcript,
                ShortcutId::SearchTranscript.default_combo(),
            ),
            SearchTranscriptPalette,
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
                ShortcutId::SearchThreads.default_combo(),
            ),
            SearchThreadsPalette,
            None,
        ),
    ]);
    cx.bind_keys(session_key_bindings(keymap));
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

/// Customizable hotkeys for selecting sessions in active sidebar order.
fn session_key_bindings(keymap: &KeymapConfig) -> Vec<KeyBinding> {
    ShortcutId::SESSION_SELECTION
        .into_iter()
        .enumerate()
        .map(|(index, id)| {
            let combo = platform_combo(keymap.get(id));
            let combo = if Keystroke::parse(&combo).is_ok() {
                combo
            } else {
                platform_combo(id.default_combo())
            };
            KeyBinding::new(&combo, SelectSession(index), None)
        })
        .collect()
}

fn session_shortcut_hint(keymap: &KeymapConfig, position: usize, visible: bool) -> Option<String> {
    if !visible {
        return None;
    }
    ShortcutId::SESSION_SELECTION
        .get(position)
        .map(|id| display_combo(keymap.get(*id)))
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
    pub const GROUPS: [(&'static str, &'static [SettingsSection]); 3] = [
        (
            "Preferences",
            &[
                SettingsSection::Appearance,
                SettingsSection::Notifications,
                SettingsSection::Hotkeys,
            ],
        ),
        (
            "Agents",
            &[SettingsSection::Agents, SettingsSection::Secrets],
        ),
        (
            "System",
            &[
                SettingsSection::Devices,
                SettingsSection::VersionControl,
                SettingsSection::Terminal,
            ],
        ),
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

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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
/// One-line archived rows: 30px content plus the shared 2px list rhythm.
const ARCHIVED_ROW_HEIGHT: f32 = 32.0;

fn closed_header_is_sticky(top_item: usize, archived_heading_index: usize) -> bool {
    top_item > archived_heading_index
}

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

fn app_update_available(status: &jolt_update::UpdateStatus) -> bool {
    status
        .latest_version
        .as_deref()
        .is_some_and(|latest| jolt_update::version_newer(latest, jolt_update::current_version()))
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
    data: Loadable<MergedUsageBreakdown>,
    unavailable_devices: usize,
    task: Option<Task<()>>,
}

#[derive(Debug, Clone)]
struct DeviceUsageBreakdownRow {
    device_id: String,
    usage: UsageBreakdownRow,
}

#[derive(Debug, Clone)]
struct MergedUsageBreakdown {
    totals: UsageBreakdown,
    rows: Vec<DeviceUsageBreakdownRow>,
}

#[derive(Debug, Clone, Copy)]
struct HarnessUsageTotal {
    harness: HarnessId,
    tokens: u64,
    cost_usd: Option<f64>,
    cost_provenance: Option<CostProvenance>,
}

fn add_usage_cost(total: &mut Option<f64>, value: Option<f64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or_default() + value);
    }
}

fn merge_cost_provenance(total: &mut Option<CostProvenance>, value: Option<CostProvenance>) {
    *total = match (*total, value) {
        (Some(CostProvenance::Mixed), _) | (_, Some(CostProvenance::Mixed)) => {
            Some(CostProvenance::Mixed)
        }
        (Some(left), Some(right)) if left != right => Some(CostProvenance::Mixed),
        (left, right) => left.or(right),
    };
}

fn merge_breakdowns(days: u16, breakdowns: Vec<UsageBreakdown>) -> MergedUsageBreakdown {
    let mut totals = UsageBreakdown {
        days,
        device_id: "all".into(),
        ..UsageBreakdown::default()
    };
    let mut activity: std::collections::BTreeMap<String, UsageDay> =
        std::collections::BTreeMap::new();
    let mut rows: std::collections::HashMap<
        (String, HarnessId, String, String),
        DeviceUsageBreakdownRow,
    > = std::collections::HashMap::new();
    for breakdown in breakdowns {
        totals.sessions = totals.sessions.saturating_add(breakdown.sessions);
        totals.calls = totals.calls.saturating_add(breakdown.calls);
        totals.input_tokens = totals.input_tokens.saturating_add(breakdown.input_tokens);
        totals.output_tokens = totals.output_tokens.saturating_add(breakdown.output_tokens);
        totals.cache_read_input_tokens = totals
            .cache_read_input_tokens
            .saturating_add(breakdown.cache_read_input_tokens);
        totals.cache_write_input_tokens = totals
            .cache_write_input_tokens
            .saturating_add(breakdown.cache_write_input_tokens);
        add_usage_cost(&mut totals.cost_usd, breakdown.cost_usd);
        merge_cost_provenance(&mut totals.cost_provenance, breakdown.cost_provenance);
        for day in breakdown.activity {
            let entry = activity.entry(day.day.clone()).or_insert_with(|| UsageDay {
                day: day.day,
                ..UsageDay::default()
            });
            entry.tokens = entry.tokens.saturating_add(day.tokens);
            entry.calls = entry.calls.saturating_add(day.calls);
            add_usage_cost(&mut entry.cost_usd, day.cost_usd);
            merge_cost_provenance(&mut entry.cost_provenance, day.cost_provenance);
        }
        for row in breakdown.rows {
            let key = (
                breakdown.device_id.clone(),
                row.harness,
                row.model.clone(),
                row.cwd.clone(),
            );
            let entry = rows.entry(key).or_insert_with(|| DeviceUsageBreakdownRow {
                device_id: breakdown.device_id.clone(),
                usage: UsageBreakdownRow {
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
                    cost_provenance: None,
                },
            });
            entry.usage.sessions = entry.usage.sessions.saturating_add(row.sessions);
            entry.usage.calls = entry.usage.calls.saturating_add(row.calls);
            entry.usage.input_tokens = entry.usage.input_tokens.saturating_add(row.input_tokens);
            entry.usage.output_tokens = entry.usage.output_tokens.saturating_add(row.output_tokens);
            entry.usage.cache_read_input_tokens = entry
                .usage
                .cache_read_input_tokens
                .saturating_add(row.cache_read_input_tokens);
            entry.usage.cache_write_input_tokens = entry
                .usage
                .cache_write_input_tokens
                .saturating_add(row.cache_write_input_tokens);
            add_usage_cost(&mut entry.usage.cost_usd, row.cost_usd);
            merge_cost_provenance(&mut entry.usage.cost_provenance, row.cost_provenance);
        }
    }
    totals.activity = activity.into_values().collect();
    let mut rows: Vec<_> = rows.into_values().collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.usage.total_tokens()));
    MergedUsageBreakdown { totals, rows }
}

fn aggregate_harness_usage(rows: &[DeviceUsageBreakdownRow]) -> Vec<HarnessUsageTotal> {
    let mut totals: std::collections::HashMap<
        HarnessId,
        (u64, Option<f64>, Option<CostProvenance>),
    > = std::collections::HashMap::new();
    for row in rows {
        let entry = totals.entry(row.usage.harness).or_default();
        entry.0 = entry.0.saturating_add(row.usage.total_tokens());
        add_usage_cost(&mut entry.1, row.usage.cost_usd);
        merge_cost_provenance(&mut entry.2, row.usage.cost_provenance);
    }
    let mut totals: Vec<_> = totals
        .into_iter()
        .map(
            |(harness, (tokens, cost_usd, cost_provenance))| HarnessUsageTotal {
                harness,
                tokens,
                cost_usd,
                cost_provenance,
            },
        )
        .collect();
    totals.sort_by_key(|total| std::cmp::Reverse(total.tokens));
    totals
}

fn compact_number(value: u64) -> String {
    if value >= 1_000_000_000 {
        format!("{:.1}b", value as f64 / 1_000_000_000.0)
    } else if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn format_usage_cost(value: Option<f64>) -> String {
    let Some(value) = value.filter(|value| value.is_finite()) else {
        return "—".into();
    };
    let rendered = format!("{:.2}", value.abs());
    let Some((whole, fraction)) = rendered.split_once('.') else {
        return format!("${rendered}");
    };
    let mut grouped = String::with_capacity(rendered.len() + whole.len() / 3);
    for (index, digit) in whole.chars().enumerate() {
        if index != 0 && (whole.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!(
        "{}${grouped}.{fraction}",
        if value.is_sign_negative() { "-" } else { "" }
    )
}

fn usage_share(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 / total as f64
    }
}

fn usage_date_range(days: u16) -> String {
    let end = chrono::Local::now().date_naive();
    let start = end - chrono::Duration::days(i64::from(days.saturating_sub(1)));
    format!("{} – {}", start.format("%b %d"), end.format("%b %d"))
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

type HarnessUpdateKey = (Option<String>, HarnessId);

pub struct Shell {
    state: Entity<AppState>,
    background_service: crate::background_service::BackgroundServiceController,
    transcript: Entity<Transcript>,
    composer: Entity<Composer>,
    /// Independent copy of the composer's target-space picker for the empty
    /// new-session canvas. It shares selection state, not the sidebar filter.
    new_chat_space_picker: Entity<Pickers>,
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
    /// Current-session transcript command center, opened with Cmd/Ctrl+F.
    transcript_search: Option<TranscriptSearchFlow>,
    /// Searchable sidebar space filter, local to this viewport.
    spaces_menu: Option<SpacesMenu>,
    /// Outside-click dismissal guard for the filter trigger.
    spaces_menu_dismissed_at: Option<std::time::Instant>,
    /// Variable-height sidebar list: section headings, full active rows, then
    /// compact archived rows. Its state also drives the scroll-edge fades.
    sidebar_list: ListState,
    /// Last item identities, used to splice changed list ranges without
    /// resetting the user's scroll position.
    sidebar_list_keys: Vec<String>,
    /// Newly created thread to reveal once its registry row arrives.
    sidebar_scroll_target: Option<String>,
    /// Cursor-paged archived tail for the current scope + space filter.
    archived_page_key: String,
    archived_next_cursor: Option<String>,
    archived_total: Option<usize>,
    archived_load_task: Option<Task<()>>,
    /// `settings.last_space_id` applied once after the first spaces frame.
    space_boot_applied: bool,
    /// Last scope whose navigation snapshot is loaded into `settings`.
    observed_scope: Option<ScopeKind>,
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
    /// Harness/version notices already delivered during this process.
    notified_harness_updates: std::collections::HashSet<String>,
    /// Updates explicitly accepted by the user and awaiting a terminal state.
    requested_harness_updates: std::collections::HashSet<HarnessUpdateKey>,
    harness_update_tasks: std::collections::HashMap<HarnessUpdateKey, Task<()>>,
    /// How this binary was installed — decides the strip's click behavior.
    /// Cached: `detect_install` stats `current_exe` and this renders per frame.
    install: jolt_update::InstallKind,
    org: Option<OrgGateUi>,
    mutate_task: Option<Task<()>>,
    regenerate_title_task: Option<Task<()>>,
    auth_task: Option<Task<()>>,
    /// Kept for the failed-gate "Retry" action.
    boot: EngineBootConfig,
    connector: EngineConnector,
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
    /// Clears FLIP offsets after their one-shot animation so virtualized rows
    /// cannot replay them when they remount during scrolling.
    sidebar_resort_task: Option<Task<()>>,
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
    /// Focus target for shell-only layouts, which otherwise have no consistently
    /// mounted focusable child to receive route-level keyboard events.
    shell_focus: FocusHandle,
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
    _new_chat_space_picker_observation: Subscription,
    _composer_events: Subscription,
    _transcript_events: Subscription,
}

fn scope_key(scope: ScopeKind) -> &'static str {
    match scope {
        ScopeKind::Local => "local",
        ScopeKind::Account => "account",
    }
}

impl Shell {
    pub(crate) fn new(
        state: Entity<AppState>,
        boot: EngineBootConfig,
        connector: EngineConnector,
        background_service: crate::background_service::BackgroundServiceController,
        cx: &mut Context<Self>,
    ) -> Self {
        let observation = cx.observe(&state, |this: &mut Shell, state, cx| {
            this.on_state_changed(&state, cx);
            cx.notify();
        });
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        let composer = cx.new(|cx| Composer::new(state.clone(), cx));
        let new_chat_space_picker = cx.new(|cx| Pickers::new(state.clone(), cx));
        let new_chat_space_picker_observation =
            cx.observe(&new_chat_space_picker, |_, _, cx| cx.notify());
        let status_strip = cx.new(|_| SessionStatusStrip {
            state: state.clone(),
            composer: composer.clone(),
        });
        let jump_to_bottom = cx.new(|_| JumpToBottom {
            transcript: transcript.clone(),
        });
        let sidebar_list = ListState::new(0, ListAlignment::Top, px(CHAT_ROW_HEIGHT * 2.0));
        let shell = cx.weak_entity();
        sidebar_list.set_scroll_handler(move |_, _, cx| {
            shell.update(cx, |_shell: &mut Shell, cx| cx.notify()).ok();
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
            move |this: &mut Shell, _, event: &ComposerEvent, cx| match event {
                ComposerEvent::Sent {
                    chat_id,
                    new_thread,
                } => {
                    if *new_thread {
                        this.sidebar_scroll_target = Some(chat_id.clone());
                    }
                    transcript.update(cx, |t, cx| t.on_own_send(cx));
                }
                ComposerEvent::GeneratedReviewFinished { review_id, error } => {
                    if let Some(changes) = this.changes.clone() {
                        changes.update(cx, |changes, cx| {
                            changes.review_submission_finished(review_id, error.as_deref(), cx);
                        });
                    }
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
                    let result = call_api(
                        engine.client(),
                        &ListAgentAccounts {
                            force_usage: Some(true),
                            usage_only: true,
                            target_device_id: None,
                        },
                    )
                    .await;
                    if let Ok(snapshot) = result
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
        // Dev/testing knob: `JOLT_OPEN_ROUTE=settings[/<section>]`
        // boots straight into a secondary page — these pages have no deep link
        // and synthetic input can't reach them on headless compositors.
        let route = match std::env::var("JOLT_OPEN_ROUTE").ok().as_deref() {
            Some("settings") | Some("settings/appearance") => {
                Route::Settings(SettingsSection::Appearance)
            }
            Some("settings/devices") => Route::Settings(SettingsSection::Devices),
            Some("settings/agents") => Route::Settings(SettingsSection::Agents),
            Some("settings/secrets") => Route::Settings(SettingsSection::Secrets),
            Some("settings/vcs") => Route::Settings(SettingsSection::VersionControl),
            Some("settings/terminal") => Route::Settings(SettingsSection::Terminal),
            Some("settings/notifications") => Route::Settings(SettingsSection::Notifications),
            Some("settings/hotkeys" | "settings/shortcuts") => {
                Route::Settings(SettingsSection::Hotkeys)
            }
            Some("new") => Route::Chat,
            _ => Route::Chat,
        };
        // More capture knobs of the same kind: `JOLT_OPEN_DIALOG=rename|delete`
        // opens that dialog for the first chat once chats land; `=model` pops
        // the combined harness/model menu once the shell is Ready;
        // `JOLT_FORCE_GATE=signin|org|failed` renders that gate regardless of
        // real auth state (display-only — for styling passes).
        let debug_dialog = std::env::var("JOLT_OPEN_DIALOG").ok();
        let debug_gate = match std::env::var("JOLT_FORCE_GATE").ok().as_deref() {
            Some("org") => Some(GatePhase::OrgGate),
            Some("failed") => Some(GatePhase::Failed(
                "Could not reach the Jolt engine on port 27901".into(),
            )),
            _ => None,
        };
        let nav = NavHistory::new(match route {
            Route::Chat => NavEntry::Chat(String::new()),
            Route::Settings(section) => NavEntry::Settings(section),
        });
        #[cfg(any(debug_assertions, feature = "debug-ui"))]
        let performance_hud =
            crate::debug::performance_hud_requested().then(|| cx.new(PerformanceHud::new));
        Self {
            state,
            background_service,
            transcript,
            composer,
            new_chat_space_picker,
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
            transcript_search: None,
            spaces_menu: None,
            spaces_menu_dismissed_at: None,
            sidebar_list,
            sidebar_list_keys: Vec::new(),
            sidebar_scroll_target: None,
            archived_page_key: String::new(),
            archived_next_cursor: None,
            archived_total: None,
            archived_load_task: None,
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
            notified_harness_updates: std::collections::HashSet::new(),
            requested_harness_updates: std::collections::HashSet::new(),
            harness_update_tasks: std::collections::HashMap::new(),
            install: jolt_update::detect_install(),
            org: None,
            mutate_task: None,
            regenerate_title_task: None,
            auth_task: None,
            boot,
            connector,
            data_dir,
            settings,
            panels: SessionPanels::default(),
            active_chat: String::new(),
            sidebar_prev_order: Vec::new(),
            sidebar_resort: std::collections::HashMap::new(),
            sidebar_new_keys: std::collections::HashSet::new(),
            resort_epoch: 0,
            sidebar_resort_task: None,
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
            shell_focus: cx.focus_handle(),
            focus_sub: None,
            _ticker: ticker,
            _account_usage_task: account_usage_task,
            _state_observation: observation,
            _new_chat_space_picker_observation: new_chat_space_picker_observation,
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

        let previous_scope = self.observed_scope;
        let snapshot = ScopeNavigation {
            last_space_id: self.settings.last_space_id.clone(),
            space_filter: self.settings.space_filter.clone(),
        };
        if let Some(previous) = previous_scope {
            self.settings
                .scope_navigation
                .insert(scope_key(previous).into(), snapshot);
        } else {
            self.settings
                .scope_navigation
                .entry(scope_key(scope).into())
                .or_insert(snapshot);
        }

        let target = self
            .settings
            .scope_navigation
            .get(scope_key(scope))
            .cloned()
            .unwrap_or_default();
        self.settings.last_space_id = target.last_space_id;
        self.settings.space_filter = target.space_filter;
        self.observed_scope = Some(scope);
        // A scope switch always starts at New Session. Route history cannot
        // cross scopes because chat ids belong to different runtimes. The
        // initial scope observation preserves debug/deep-link routes.
        if previous_scope.is_some() {
            self.route = Route::Chat;
            self.nav = NavHistory::new(NavEntry::Chat(String::new()));
        }
        // Scope-bound views own standing RPC streams; recreate them against the
        // newly routed runtime while leaving both engine runtimes alive.
        self.terminal = None;
        self.terminal_expanded = false;
        self.changes = None;
        self.changes_expanded = false;
        self.changes_sub = None;
        self.devices_page = None;
        self.add_space = None;
        self.session_search = None;
        self.transcript_search = None;
        self.space_boot_applied = false;
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
        self.notify_harness_updates(state, cx);
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
        // Persist the active space for the new-session fallback.
        {
            let selected_space = state.read(cx).selected_space.clone();
            if selected_space != self.settings.last_space_id && selected_space.is_some() {
                self.settings.last_space_id = selected_space;
                self.schedule_save(cx);
            }
        }
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
            if self.sidebar_scroll_target.as_deref() != Some(selected.as_str()) {
                self.sidebar_scroll_target = None;
            }
            self.active_chat = selected;
            // Route history: a chat switch is a navigation. Walking history
            // lands here too, but the destination already equals `current()`,
            // so the push deduplicates it.
            if matches!(self.route, Route::Chat) {
                self.nav.push(NavEntry::Chat(self.active_chat.clone()));
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
            // A long-running background engine may predate this app bundle.
            // Announce releases newer than the viewport, not newer than that engine.
            .filter(|status| app_update_available(status))
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
    /// Where unified-titlebar content starts: past the traffic lights and
    /// control cluster, riding the fullscreen inset tween.
    pub(super) fn title_bar_content_start(&self) -> f32 {
        let fullscreen = self.fullscreen.unwrap_or(false);
        let is_macos = cfg!(target_os = "macos");
        let cluster = self.eval_tween(
            self.titlebar_tween,
            cluster_buttons_start(is_macos, fullscreen),
        );
        cluster + CLUSTER_BUTTONS_WIDTH + 10.0
    }

    /// The unified window titlebar: chat shows the current session identity;
    /// secondary pages keep the strip clear. Full-width on the glass shell;
    /// the traffic lights and control cluster overlay its left end.
    fn render_title_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        match self.route {
            Route::Chat => self.render_session_header(cx),
            Route::Settings(_) => {
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
                icons::LAYOUT_SIDEBAR,
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

    fn render_sidebar(
        &mut self,
        session_shortcuts_visible: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let inner: AnyElement = match self.route {
            Route::Settings(section) => self.render_settings_nav(section, &theme, cx),
            Route::Chat => self.render_chat_sidebar(session_shortcuts_visible, &theme, cx),
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
            SettingsSection::Devices => icons::DEVICE_DESKTOP,
            SettingsSection::Agents => icons::USER,
            SettingsSection::Secrets => icons::KEY,
            SettingsSection::VersionControl => icons::GIT_BRANCH,
            SettingsSection::Terminal => icons::TERMINAL_2,
            SettingsSection::Appearance => icons::ADJUSTMENTS_HORIZONTAL,
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
                    .child(div().flex().flex_col().children(
                        SettingsSection::GROUPS.into_iter().enumerate().map(
                            |(group_index, (group, items))| {
                                div()
                                    .when(group_index > 0, |el| el.mt(px(12.0)))
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.0))
                                    .child(
                                        div()
                                            .px(px(Theme::SPACE_SM))
                                            .pb(px(3.0))
                                            .text_size(px(10.0))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(theme.text_muted.opacity(0.5))
                                            .child(SharedString::from(group)),
                                    )
                                    .children(items.iter().copied().map(|item| {
                                        let selected = item == section;
                                        div()
                                            .id(SharedString::from(format!(
                                                "settings-nav-{}",
                                                item.label()
                                            )))
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
                                            .hover(|s| {
                                                s.bg(crate::theme::wash(0.11))
                                                    .text_color(theme.text)
                                            })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.open_settings(item, cx)
                                            }))
                                            .child(
                                                icon(section_icon(item))
                                                    .size(px(16.0))
                                                    .text_color(theme.text_muted),
                                            )
                                            .child(SharedString::from(item.label()))
                                    }))
                            },
                        ),
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
                            icon(icons::CHEVRON_LEFT)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Back")),
                ),
            )
            .into_any_element()
    }

    /// One session row: status rail on the left
    /// (a live text spinner while working, a dot otherwise), title +
    /// relative time on the first line, "folder · device" underneath aligned
    /// to the title. Click selects; right-click opens the context menu.
    #[allow(clippy::too_many_arguments)]
    fn render_chat_row(
        &self,
        id: String,
        title: SharedString,
        time_ago: SharedString,
        shortcut_hint: Option<SharedString>,
        space_name: SharedString,
        branch: Option<SharedString>,
        harness: Option<jolt_proto::HarnessId>,
        status: jolt_proto::ChatIndicator,
        pinned: bool,
        selected: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Status is a rail, not a word, and is always present
        // so rows align and state changes read in place. Working animates as a
        // compact text spinner; every other status is a dot.
        let dot_color = spaces::status_dot_color(status, theme);
        let status_rail: AnyElement = if status == jolt_proto::ChatIndicator::Working {
            div()
                .w(px(6.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(loaders::activity_spinner(
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
        let persistent_pin_id = id.clone();
        let pin_id = id.clone();
        let archive_id = id.clone();
        let can_close = !matches!(
            status,
            jolt_proto::ChatIndicator::Working | jolt_proto::ChatIndicator::AwaitingInput
        );
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
                this.open_chat(select_id.clone(), cx);
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
                    .h(px(14.0))
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
                                    .flex()
                                    .items_center()
                                    .gap(px(4.0))
                                    .text_size(px(11.0))
                                    .text_color(subline)
                                    .opacity(1.0)
                                    .group_hover(group.clone(), |style| style.opacity(0.0))
                                    .child(shortcut_hint.unwrap_or(time_ago)),
                            )
                            .when(pinned, |element| {
                                element.child(
                                    div()
                                        .id(SharedString::from(format!("chat-pin-persistent-{id}")))
                                        .absolute()
                                        .top(px(-2.0))
                                        .left(px(2.0))
                                        .size(px(18.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(5.0))
                                        .text_color(theme.text_muted)
                                        .cursor_pointer()
                                        .group_hover(group.clone(), |style| style.opacity(0.0))
                                        .hover(|style| {
                                            style
                                                .bg(crate::theme::wash(0.14))
                                                .text_color(theme.text)
                                        })
                                        .tooltip(|_, cx| {
                                            cx.new(|_| SessionActionTooltip("Unpin thread".into()))
                                                .into()
                                        })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.set_chat_pinned(
                                                persistent_pin_id.clone(),
                                                false,
                                                cx,
                                            );
                                        }))
                                        .child(
                                            icon(icons::PINNED)
                                                .size(px(12.0))
                                                .text_color(theme.text_muted),
                                        ),
                                )
                            })
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
                                            .id(SharedString::from(format!("chat-pin-{id}")))
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
                                            .tooltip(move |_, cx| {
                                                cx.new(|_| {
                                                    SessionActionTooltip(
                                                        if pinned {
                                                            "Unpin thread"
                                                        } else {
                                                            "Pin thread"
                                                        }
                                                        .into(),
                                                    )
                                                })
                                                .into()
                                            })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.set_chat_pinned(pin_id.clone(), !pinned, cx);
                                            }))
                                            .child(
                                                icon(if pinned {
                                                    icons::PINNED
                                                } else {
                                                    icons::PIN
                                                })
                                                .size(px(12.0))
                                                .text_color(theme.text_muted),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .id(SharedString::from(format!("chat-archive-{id}")))
                                            .size(px(18.0))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded(px(5.0))
                                            .text_color(theme.text_muted)
                                            .opacity(if can_close { 1.0 } else { 0.35 })
                                            .tooltip(move |_, cx| {
                                                cx.new(|_| {
                                                    SessionActionTooltip(
                                                        if can_close {
                                                            "Close thread"
                                                        } else {
                                                            "Stop the current run to close"
                                                        }
                                                        .into(),
                                                    )
                                                })
                                                .into()
                                            })
                                            .when(can_close, |element| {
                                                element
                                                    .cursor_pointer()
                                                    .hover(|style| {
                                                        style
                                                            .bg(crate::theme::wash(0.14))
                                                            .text_color(theme.text)
                                                    })
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        cx.stop_propagation();
                                                        this.archive_chat(archive_id.clone(), cx);
                                                    }))
                                            })
                                            .child(
                                                icon(icons::MESSAGE_CIRCLE_X)
                                                    .size(px(12.0))
                                                    .text_color(theme.text_muted),
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

    /// One-line archived session row. The row opens history without changing
    /// lifecycle state; its hover action restores explicitly.
    fn render_archived_chat_row(
        &self,
        row: ArchivedChatRow,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open_id = row.id.clone();
        let restore_id = row.id;
        let group: SharedString = format!("archived-chat-group-{restore_id}").into();
        div()
            .id(SharedString::from(format!("archived-chat-{restore_id}")))
            .group(group.clone())
            .h(px(30.0))
            .w_full()
            .px(px(Theme::SPACE_SM))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .rounded(px(8.0))
            .cursor_pointer()
            .text_color(theme.text_muted.opacity(0.7))
            .hover(|style| style.bg(crate::theme::wash(0.11)).text_color(theme.text))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_chat(open_id.clone(), cx);
            }))
            .child(
                icon(icons::MESSAGE_CIRCLE_X)
                    .size(px(13.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.55)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.0))
                    .child(row.title),
            )
            .child(
                div()
                    .w(px(34.0))
                    .h(px(20.0))
                    .flex_none()
                    .relative()
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_end()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted.opacity(0.45))
                            .opacity(1.0)
                            .group_hover(group.clone(), |style| style.opacity(0.0))
                            .child(row.time_ago),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("restore-chat-{restore_id}")))
                            .absolute()
                            .top_0()
                            .right_0()
                            .size(px(20.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .opacity(0.0)
                            .group_hover(group, |style| style.opacity(1.0))
                            .hover(|style| style.bg(crate::theme::wash(0.14)))
                            .tooltip(|_, cx| {
                                cx.new(|_| SessionActionTooltip("Reopen thread".into()))
                                    .into()
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.unarchive_chat(restore_id.clone(), cx);
                            }))
                            .child(
                                icon(icons::RESTORE)
                                    .size(px(13.0))
                                    .text_color(theme.text_muted),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Which sidebar-list edges have hidden overflow (offset from the LAST
    /// frame — the invisible one-frame lag every fade here rides).
    pub(super) fn sidebar_fade_zones(&self) -> (bool, bool) {
        let scrolled = -f32::from(self.sidebar_list.scroll_px_offset_for_scrollbar().y);
        let max_scroll = f32::from(self.sidebar_list.max_offset_for_scrollbar().y);
        (scrolled > 1.0, scrolled < max_scroll - 1.0)
    }

    fn load_more_archived(&mut self, cx: &mut Context<Self>) {
        let key = {
            let state = self.state.read(cx);
            format!(
                "{}:{}",
                scope_key(state.active_scope()),
                self.settings.space_filter.as_deref().unwrap_or("*")
            )
        };
        if self.archived_page_key != key {
            self.archived_load_task = None;
            self.archived_page_key.clone_from(&key);
            self.archived_next_cursor = None;
            self.archived_total = None;
        }
        if self.archived_load_task.is_some() {
            return;
        }
        let loaded = self.archived_rows(cx).len();
        if self.archived_total.is_some_and(|total| loaded >= total)
            || (self.archived_total.is_some() && self.archived_next_cursor.is_none())
        {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let request = QueryChats {
            section: ChatSection::Archived,
            space_id: self.settings.space_filter.clone(),
            query: String::new(),
            cursor: self.archived_next_cursor.clone(),
            limit: 50,
        };
        self.archived_load_task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(engine.client(), &request).await;
            this.update(cx, |shell, cx| {
                if shell.archived_page_key != key {
                    return;
                }
                shell.archived_load_task = None;
                match result {
                    Ok(page) => {
                        shell.archived_next_cursor = page.next_cursor;
                        shell.archived_total = Some(page.total);
                        shell.state.update(cx, |state, cx| {
                            state.merge_chat_page(page.chats);
                            cx.notify();
                        });
                    }
                    Err(error) => tracing::warn!(%error, "archived session page load failed"),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Chat-mode sidebar: fixed space filter, filtered Threads list, notices,
    /// and the user menu.
    fn render_chat_sidebar(
        &mut self,
        session_shortcuts_visible: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let user = self.state.read(cx).auth_user().cloned();

        // Keep only compact row data here. The variable-height list below asks
        // for individual visible items, so full active rows and one-line
        // archived rows share a scroll surface without laying out history that
        // is offscreen.
        self.load_more_archived(cx);
        let rows = self.active_rows(cx);
        let archived_rows = self.archived_rows(cx);

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
                    let epoch = self.resort_epoch;
                    self.sidebar_resort_task = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(RESORT.total()).await;
                        this.update(cx, |shell, cx| {
                            if shell.resort_epoch == epoch {
                                shell.clear_sidebar_resort_animation();
                                cx.notify();
                            }
                        })
                        .ok();
                    }));
                } else {
                    self.clear_sidebar_resort_animation();
                }
            }
            self.sidebar_prev_order = order;
        }

        let row_count = rows.len();
        let archived_count = archived_rows.len();
        let active_item_count = row_count.max(1);
        let archived_heading_index = active_item_count;
        let item_count = archived_heading_index
            + if archived_count > 0 {
                1 + archived_count
            } else {
                0
            };
        let mut item_keys = Vec::with_capacity(item_count);
        if rows.is_empty() {
            item_keys.push("empty:sessions".to_string());
        } else {
            item_keys.extend(rows.iter().map(|row| row.key.clone()));
        }
        if !archived_rows.is_empty() {
            item_keys.push("heading:archived".to_string());
            item_keys.extend(archived_rows.iter().map(|row| row.key.clone()));
        }
        if self.sidebar_list_keys != item_keys {
            let prefix = self
                .sidebar_list_keys
                .iter()
                .zip(&item_keys)
                .take_while(|(old, new)| old == new)
                .count();
            let suffix = self.sidebar_list_keys[prefix..]
                .iter()
                .rev()
                .zip(item_keys[prefix..].iter().rev())
                .take_while(|(old, new)| old == new)
                .count();
            let old_end = self.sidebar_list_keys.len() - suffix;
            let replacement_count = item_keys.len() - prefix - suffix;
            self.sidebar_list.splice(prefix..old_end, replacement_count);
            self.sidebar_list_keys = item_keys;
        }
        if let Some(target) = self.sidebar_scroll_target.as_deref()
            && let Some(index) = rows.iter().position(|row| row.id == target)
        {
            self.sidebar_list.scroll_to(gpui::ListOffset {
                item_ix: index,
                offset_in_item: px(0.0),
            });
            self.sidebar_scroll_target = None;
        }

        let closed_header_sticky = archived_count > 0
            && closed_header_is_sticky(
                self.sidebar_list.logical_scroll_top().item_ix,
                archived_heading_index,
            );
        let epoch = self.resort_epoch;
        let theme_for_rows = theme.clone();
        let shell = cx.weak_entity();
        let sidebar_list = list(self.sidebar_list.clone(), move |index, _, cx| {
            shell
                .update(cx, |this, cx| {
                    if rows.is_empty() && index == 0 {
                        return div()
                            .h(px(28.0))
                            .px(px(Theme::SPACE_SM))
                            .text_size(px(12.0))
                            .text_color(theme_for_rows.text_faint)
                            .child(SharedString::from(if archived_count > 0 {
                                "No active threads"
                            } else {
                                "No threads yet"
                            }))
                            .into_any_element();
                    }

                    if let Some((active_index, row)) = (index < row_count)
                        .then_some(index)
                        .and_then(|active_index| {
                            rows.get(active_index).map(|row| (active_index, row))
                        })
                    {
                        let shortcut_hint = session_shortcut_hint(
                            &this.settings.keymap,
                            active_index,
                            session_shortcuts_visible,
                        )
                        .map(SharedString::from);
                        let element = this.render_chat_row(
                            row.id.clone(),
                            row.title.clone(),
                            row.time_ago.clone(),
                            shortcut_hint,
                            row.space_name.clone(),
                            row.branch.clone(),
                            row.harness,
                            row.status,
                            row.pinned,
                            row.selected,
                            &theme_for_rows,
                            cx,
                        );
                        let element = if let Some(dy) = this.sidebar_resort.get(&row.key).copied() {
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
                        return div()
                            .h(px(CHAT_ROW_HEIGHT + SIDEBAR_LIST_GAP))
                            .pb(px(SIDEBAR_LIST_GAP))
                            .child(element)
                            .into_any_element();
                    }

                    if archived_count > 0 && index == archived_heading_index {
                        return div()
                            .h(px(32.0))
                            .mt(px(4.0))
                            .pl(px(Theme::SPACE_SM))
                            .border_t_1()
                            .border_color(theme_for_rows.border.opacity(0.55))
                            .flex()
                            .items_center()
                            .text_size(px(11.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme_for_rows.text_muted.opacity(0.6))
                            .child(SharedString::from("Closed"))
                            .into_any_element();
                    }

                    let archived_index = index.saturating_sub(archived_heading_index + 1);
                    archived_rows
                        .get(archived_index)
                        .map(|row| {
                            if archived_index.saturating_add(8) >= archived_count {
                                this.load_more_archived(cx);
                            }
                            div()
                                .h(px(ARCHIVED_ROW_HEIGHT))
                                .pb(px(SIDEBAR_LIST_GAP))
                                .child(this.render_archived_chat_row(
                                    row.clone(),
                                    &theme_for_rows,
                                    cx,
                                ))
                                .into_any_element()
                        })
                        .unwrap_or_else(|| Empty.into_any_element())
                })
                .unwrap_or_else(|_| Empty.into_any_element())
        })
        .w_full()
        .flex_1()
        .min_h_0();

        // Overflow edge fades for the lists scroll region (offset from the
        // LAST frame; the lag is invisible).
        let (lists_fade_top, lists_fade_bottom) = self.sidebar_fade_zones();
        // Opaque platforms melt overflow into the surface tone with painted
        // gradient overlays. Over GLASS no overlay can work — the backdrop is
        // see-through blur, so tone stacks into a smudge and black reads as a
        // shadow (user reports). Instead the ROWS fade themselves: prepaint-
        // measured bounds drive per-row opacity toward the viewport edges
        // ([`Shell::sidebar_row_alpha`]), dissolving the edge to pure glass.
        let glass = theme.is_glass();
        let sidebar_fade = theme.surface;

        let local_active = self.state.read(cx).active_scope() == ScopeKind::Local;
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
        let sidebar_heading = div()
            .h(px(28.0))
            .pl(px(Theme::SPACE_SM))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .text_size(px(11.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted.opacity(0.6))
            .child(SharedString::from(if closed_header_sticky {
                "Closed"
            } else {
                "Threads"
            }))
            .when(!closed_header_sticky, |el| {
                el.child(
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
                        .on_click(cx.listener(|this, _, _, cx| this.open_session_search(cx)))
                        .child(
                            icon(icons::SEARCH)
                                .size(px(14.0))
                                .text_color(theme.text_muted.opacity(0.7)),
                        ),
                )
            });

        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            // The filter stays outside overflow so its dropdown cannot clip.
            .child(filter_row)
            .child(
                div()
                    .id("sidebar-lists")
                    .flex_1()
                    .min_h_0()
                    .px(px(Theme::SPACE_SM))
                    .flex()
                    .flex_col()
                    // Keep the sticky section heading outside both fade
                    // implementations so scrolling cannot alter its color.
                    .child(sidebar_heading)
                    .child(crate::edge_fade::edge_faded(
                        SIDEBAR_GLASS_FADE_BAND,
                        glass && lists_fade_top,
                        glass && lists_fade_bottom,
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .child(sidebar_list)
                            .when(lists_fade_top && !glass, |el| {
                                el.child(
                                    div().absolute().top_0().left_0().right_0().h(px(24.0)).bg(
                                        gpui::linear_gradient(
                                            180.0,
                                            gpui::linear_color_stop(sidebar_fade, 0.0),
                                            gpui::linear_color_stop(sidebar_fade.opacity(0.0), 1.0),
                                        ),
                                    ),
                                )
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
                    )),
            )
            .child(div().p(px(Theme::SPACE_SM)).flex_none().child(user_menu))
            .into_any_element()
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
            let (active_scope, account_available) = {
                let state = self.state.read(cx);
                (state.active_scope(), state.account_available())
            };
            let mut scope_rows: Vec<AnyElement> = match active_scope {
                ScopeKind::Account => vec![
                    popover::menu_row(theme, false, "user-menu-signout")
                        .id("user-menu-signout")
                        .on_click(cx.listener(|this, _, _, cx| this.sign_out(cx)))
                        .child(
                            icon(icons::LOGOUT)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Sign out"))
                        .into_any_element(),
                ],
                ScopeKind::Local if account_available => vec![
                    popover::menu_row(theme, false, "user-menu-switch-account")
                        .id("user-menu-switch-account")
                        .on_click(
                            cx.listener(|this, _, _, cx| this.switch_scope(ScopeKind::Account, cx)),
                        )
                        .child(
                            icon(icons::WORLD)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Switch back to account"))
                        .into_any_element(),
                ],
                ScopeKind::Local => vec![
                    popover::menu_row(theme, false, "user-menu-signin")
                        .id("user-menu-signin")
                        .on_click(cx.listener(|this, _, _, cx| this.start_sign_in(cx)))
                        .child(
                            icon(icons::WORLD)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Sign in"))
                        .into_any_element(),
                ],
            };
            if active_scope == ScopeKind::Account {
                scope_rows.insert(
                    0,
                    popover::menu_row(theme, false, "user-menu-switch-local")
                        .id("user-menu-switch-local")
                        .on_click(
                            cx.listener(|this, _, _, cx| this.switch_scope(ScopeKind::Local, cx)),
                        )
                        .child(
                            icon(icons::DEVICE_LAPTOP)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Switch to Local"))
                        .into_any_element(),
                );
            }
            let update_ready = matches!(&self.update_flow, UpdateFlow::Ready(_));
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
                            this.open_settings(SettingsSection::Appearance, cx)
                        }))
                        .child(
                            icon(icons::SETTINGS)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Settings")),
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
                    popover::menu_row(
                        theme,
                        self.update_checking && !update_ready,
                        "user-menu-check-update",
                    )
                    .id("user-menu-check-update")
                    .on_click(cx.listener(|this, _, _, cx| {
                        if matches!(&this.update_flow, UpdateFlow::Ready(_)) {
                            this.apply_ready_update(cx);
                        } else {
                            this.check_for_update(cx);
                        }
                    }))
                    .child(
                        icon(if update_ready {
                            icons::RELOAD
                        } else {
                            icons::REFRESH
                        })
                        .size(px(16.0))
                        .text_color(theme.text_muted),
                    )
                    .child(SharedString::from(if update_ready {
                        "Restart to update"
                    } else if self.update_checking {
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
                    "Syncing moves your local spaces, threads, transcripts, and attachments into this account so they’re available on your other devices and iPhone.",
                )))
                .child(div().mt(px(8.0)).child(popover::dialog_body(
                    &theme,
                    "Repository files, harness and provider credentials, Jolt secrets, full tool inputs, journals, and detailed usage remain on this device.",
                )))
                .child(div().mt(px(8.0)).child(popover::dialog_body(
                    &theme,
                    "If you keep them Local, these threads stay only on this device and cannot be viewed or controlled remotely.",
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
            let (pinned, can_close) = {
                let state = self.state.read(cx);
                state
                    .chats
                    .iter()
                    .find(|chat| chat.id == chat_id)
                    .map(|chat| {
                        let can_close = !matches!(
                            state.display_status_for(chat, Utc::now()),
                            jolt_proto::ChatIndicator::Working
                                | jolt_proto::ChatIndicator::AwaitingInput
                        );
                        (chat.pinned, can_close)
                    })
                    .unwrap_or((false, false))
            };
            let pin_id = chat_id.clone();
            let rename_id = chat_id.clone();
            let regenerate_id = chat_id.clone();
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
                    popover::menu_row(&theme, false, format!("chat-menu-pin-{chat_id}"))
                        .id("chat-menu-pin")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_chat_pinned(pin_id.clone(), !pinned, cx)
                        }))
                        .child(
                            icon(if pinned { icons::PINNED } else { icons::PIN })
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from(if pinned { "Unpin" } else { "Pin" })),
                )
                .child(popover::menu_separator())
                .child(
                    popover::menu_row(&theme, false, format!("chat-menu-rename-{chat_id}"))
                        .id("chat-menu-rename")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_rename_chat(rename_id.clone(), cx)
                        }))
                        .child(
                            icon(icons::PENCIL)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Rename…")),
                )
                .child(
                    popover::menu_row(&theme, false, format!("chat-menu-regenerate-{chat_id}"))
                        .id("chat-menu-regenerate")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.regenerate_chat_title(regenerate_id.clone(), cx)
                        }))
                        .child(
                            icon(icons::REFRESH)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Regenerate name")),
                )
                .when(can_close, |menu| {
                    menu.child(
                        popover::menu_row(&theme, false, format!("chat-menu-archive-{chat_id}"))
                            .id("chat-menu-archive")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.archive_chat(archive_id.clone(), cx)
                            }))
                            .child(
                                icon(icons::MESSAGE_CIRCLE_X)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Close")),
                    )
                })
                .child(popover::menu_separator())
                .child(
                    popover::menu_row(&theme, false, format!("chat-menu-delete-{chat_id}"))
                        .id("chat-menu-delete")
                        .text_color(theme.danger)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.confirm_delete_chat(delete_id.clone(), cx)
                        }))
                        .child(icon(icons::TRASH).size(px(16.0)).text_color(theme.danger))
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
                .child(popover::dialog_title(&theme, "Rename thread"))
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

        overlays.extend(self.render_space_overlays(viewport, window, cx));
        if let Some(overlay) = self.render_add_space_overlay(viewport, window, cx) {
            overlays.push(overlay);
        }
        if let Some(overlay) = self.render_session_search_overlay(viewport, window, cx) {
            overlays.push(overlay);
        }
        if let Some(overlay) = self.render_transcript_search_overlay(viewport, window, cx) {
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
                    .unwrap_or_else(|| "New thread".into()),
            );
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Delete thread?"))
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
                .child(loaders::activity_spinner(
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
                    .child(loaders::activity_spinner(
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
                .child(loaders::activity_spinner(
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
        // `HitboxBehavior::BlockMouse`) makes the window hit-test SQUARE at the
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
        // focused element unmounted), route it to a handle that remains mounted.
        // Maximizing Changes removes the composer, so its fallback is the shell
        // itself; this keeps the panel toggle live so it can close the view.
        if self.focus_sub.is_none() {
            self.focus_sub =
                Some(
                    cx.on_focus_lost(window, |this: &mut Shell, window, cx| match this.route {
                        Route::Chat if !this.changes_expanded => {
                            window.focus(&this.composer.focus_handle(cx), cx)
                        }
                        Route::Chat | Route::Settings(_) => window.focus(&this.shell_focus, cx),
                    }),
                );
        }
        if matches!(gate, GatePhase::Ready) && window.focused(cx).is_none() {
            match self.route {
                Route::Chat if !self.changes_expanded => {
                    window.focus(&self.composer.focus_handle(cx), cx)
                }
                Route::Chat | Route::Settings(_) => window.focus(&self.shell_focus, cx),
            }
        }

        let session_shortcuts_visible = window.modifiers().secondary();
        let root = div()
            .id("shell-root")
            .when(
                !matches!(self.route, Route::Chat) || self.changes_expanded,
                |root| root.track_focus(&self.shell_focus),
            )
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
            .on_modifiers_changed(
                cx.listener(|_, _: &gpui::ModifiersChangedEvent, _, cx| cx.notify()),
            )
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && this.dismiss_settings_modal(cx) {
                    cx.stop_propagation();
                    return;
                }
                let shell_overlay_open = this.user_menu_open
                    || this.chat_menu.is_some()
                    || this.rename_dialog.is_some()
                    || this.breakdown_dialog.is_some()
                    || this.delete_confirm.is_some()
                    || this.space_menu.is_some()
                    || this.spaces_menu.is_some()
                    || this.rename_space_dialog.is_some()
                    || this.delete_space_confirm.is_some()
                    || this.add_space.is_some()
                    || this.session_search.is_some()
                    || this.transcript_search.is_some();
                if event.keystroke.key == "escape" && this.breakdown_dialog.is_some() {
                    this.breakdown_dialog = None;
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
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
                    && this.nav.can_back()
                {
                    this.navigate_back(cx);
                    cx.stop_propagation();
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
            .on_action(cx.listener(|this, action: &SelectSession, _, cx| {
                this.select_sidebar_session(action.0, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| {
                this.open_settings(SettingsSection::Appearance, cx)
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
            .on_action(cx.listener(|this, _: &SearchThreadsPalette, _, cx| {
                if this.session_search.is_some() {
                    this.session_search = None;
                    cx.notify();
                } else {
                    this.open_session_search(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SearchTranscriptPalette, _, cx| {
                if !matches!(this.route, Route::Chat) || this.state.read(cx).selected_chat.is_none()
                {
                    cx.propagate();
                } else if this.transcript_search.is_none() {
                    this.open_transcript_search(cx);
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
                    self.render_sidebar(session_shortcuts_visible, cx)
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
                // No top margin: the titlebar's own internal air is the gap —
                // an extra gutter read as a hole between the header and the app
                // (user report).
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
            phase @ GatePhase::Failed(_) => {
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
        // because they do not render the unified application titlebar.
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
mod tests;
