//! Settings → Hotkeys: every customizable app command binding. Click a combo
//! to record (Esc cancels), with live conflict detection, per-row Reset, and
//! Restore defaults. Hotkeys are grouped into collapsible categories; session
//! switching starts collapsed. Changes emit [`HotkeysEvent::Changed`]; the
//! shell persists them and re-applies the app keymap.

use std::collections::HashSet;

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, KeyDownEvent, SharedString, Window, div,
    prelude::*, px,
};

use crate::settings::{
    KeymapConfig, ShortcutId, combo_from_keystroke, display_combo, hotkeys_can_overlap,
};
use crate::state::AppState;
use crate::theme::Theme;

/// Outcome of one keystroke while recording. Pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
    /// Esc — abandon recording, keep the old combo.
    Cancelled,
    /// A bare modifier (or unusable key) — stay recording.
    Ignored,
    /// A full combo landed.
    Set(String),
}

pub fn record_key(key: &str, ctrl: bool, alt: bool, shift: bool, cmd: bool) -> RecordOutcome {
    if key.eq_ignore_ascii_case("escape") {
        return RecordOutcome::Cancelled;
    }
    match combo_from_keystroke(ctrl, alt, shift, cmd, key) {
        Some(combo) => RecordOutcome::Set(combo),
        None => RecordOutcome::Ignored,
    }
}

#[derive(Debug, Clone)]
pub enum HotkeysEvent {
    /// The keymap changed — persist + re-apply.
    Changed(KeymapConfig),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum HotkeyCategory {
    SessionActions,
    TabSwitching,
    NavigationLayout,
    AppWindow,
    Developer,
}

impl HotkeyCategory {
    const ALL: [Self; 5] = [
        Self::SessionActions,
        Self::TabSwitching,
        Self::NavigationLayout,
        Self::AppWindow,
        Self::Developer,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::SessionActions => "Session actions",
            Self::TabSwitching => "Tab switching",
            Self::NavigationLayout => "Navigation & layout",
            Self::AppWindow => "App & window",
            Self::Developer => "Developer",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::SessionActions => 0,
            Self::TabSwitching => 1,
            Self::NavigationLayout => 2,
            Self::AppWindow => 3,
            Self::Developer => 4,
        }
    }

    fn for_hotkey(id: ShortcutId) -> Self {
        match id {
            ShortcutId::NewSession
            | ShortcutId::ClearInput
            | ShortcutId::CloseTab
            | ShortcutId::PreviousTranscriptTurn
            | ShortcutId::NextTranscriptTurn
            | ShortcutId::SearchTranscript => Self::SessionActions,
            ShortcutId::SelectTab1
            | ShortcutId::SelectTab2
            | ShortcutId::SelectTab3
            | ShortcutId::SelectTab4
            | ShortcutId::SelectTab5
            | ShortcutId::SelectTab6
            | ShortcutId::SelectTab7
            | ShortcutId::SelectTab8
            | ShortcutId::SelectLastTab => Self::TabSwitching,
            ShortcutId::OpenSettings
            | ShortcutId::OpenSpacesDropdown
            | ShortcutId::AddSpace
            | ShortcutId::SearchSessions
            | ShortcutId::ToggleSidebar
            | ShortcutId::ToggleChanges
            | ShortcutId::ToggleTerminal
            | ShortcutId::NewTerminalTab
            | ShortcutId::CloseTerminalTab => Self::NavigationLayout,
            ShortcutId::Quit
            | ShortcutId::Hide
            | ShortcutId::HideOthers
            | ShortcutId::Minimize
            | ShortcutId::CloseWindow => Self::AppWindow,
            ShortcutId::PerformanceHud => Self::Developer,
        }
    }
}

fn default_collapsed_categories() -> HashSet<HotkeyCategory> {
    HashSet::from([HotkeyCategory::TabSwitching])
}

pub struct HotkeysPage {
    /// Working copy (kept in sync with the shell via `Changed` events).
    keymap: KeymapConfig,
    recording: Option<ShortcutId>,
    /// A rejected record attempt ("{Combo} is already assigned to {label}.") —
    /// conflicts never persist; they're refused at record time, as in jolt.
    conflict_notice: Option<SharedString>,
    collapsed_categories: HashSet<HotkeyCategory>,
    focus: FocusHandle,
    // The page never talks RPC; state is retained for future per-device keymaps.
    _state: Entity<AppState>,
}

impl EventEmitter<HotkeysEvent> for HotkeysPage {}

impl HotkeysPage {
    pub fn new(state: Entity<AppState>, keymap: KeymapConfig, cx: &mut Context<Self>) -> Self {
        Self {
            keymap,
            recording: None,
            conflict_notice: None,
            collapsed_categories: default_collapsed_categories(),
            focus: cx.focus_handle(),
            _state: state,
        }
    }

    fn commit(&mut self, cx: &mut Context<Self>) {
        cx.emit(HotkeysEvent::Changed(self.keymap.clone()));
        cx.notify();
    }

    fn set_hotkey(&mut self, id: ShortcutId, combo: String, cx: &mut Context<Self>) {
        if let Some(owner) = conflict_owner(&self.keymap, id, &combo) {
            self.conflict_notice = Some(
                format!(
                    "{} is already assigned to {}.",
                    display_combo(&combo),
                    owner.label()
                )
                .into(),
            );
            self.recording = None;
            cx.notify();
        } else {
            self.keymap.set(id, combo);
            self.recording = None;
            self.conflict_notice = None;
            self.commit(cx);
        }
    }

    fn toggle_category(&mut self, category: HotkeyCategory, cx: &mut Context<Self>) {
        if !self.collapsed_categories.insert(category) {
            self.collapsed_categories.remove(&category);
        }
        self.recording = None;
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(recording) = self.recording else {
            return;
        };
        let mods = &event.keystroke.modifiers;
        match record_key(
            &event.keystroke.key,
            mods.control,
            mods.alt,
            mods.shift,
            mods.platform,
        ) {
            RecordOutcome::Cancelled => {
                self.recording = None;
                cx.notify();
            }
            RecordOutcome::Ignored => {}
            RecordOutcome::Set(combo) => self.set_hotkey(recording, combo, cx),
        }
        cx.stop_propagation();
    }
}

/// The hotkey (other than `id`) already bound to `combo`, if any. Pure.
pub fn conflict_owner(keymap: &KeymapConfig, id: ShortcutId, combo: &str) -> Option<ShortcutId> {
    ShortcutId::all()
        .into_iter()
        .find(|&other| other != id && !hotkeys_can_overlap(id, other) && keymap.get(other) == combo)
}

/// One-line purpose copy per hotkey.
fn description(id: ShortcutId) -> &'static str {
    match id {
        ShortcutId::NewSession => "Open the new session page.",
        ShortcutId::ClearInput => "Clear the current composer input.",
        ShortcutId::CloseTab => "Close the current local tab without archiving its session.",
        ShortcutId::PreviousTranscriptTurn => "Scroll to the previous user prompt.",
        ShortcutId::NextTranscriptTurn => "Scroll to the next user prompt.",
        ShortcutId::SearchTranscript => "Find text in the current transcript.",
        ShortcutId::OpenSettings => "Open the settings page.",
        ShortcutId::OpenSpacesDropdown => "Open the sidebar space filter.",
        ShortcutId::AddSpace => "Open the folder browser to add a space.",
        ShortcutId::SearchSessions => "Find a session by title.",
        ShortcutId::ToggleSidebar => "Show or hide sessions and settings navigation.",
        ShortcutId::ToggleChanges => "Show or hide changes for the current session.",
        ShortcutId::ToggleTerminal => "Show or hide the terminal for the current session.",
        ShortcutId::NewTerminalTab => "Open another tab in the focused terminal pane.",
        ShortcutId::CloseTerminalTab => "Close the active tab in the focused terminal pane.",
        ShortcutId::SelectTab1
        | ShortcutId::SelectTab2
        | ShortcutId::SelectTab3
        | ShortcutId::SelectTab4
        | ShortcutId::SelectTab5
        | ShortcutId::SelectTab6
        | ShortcutId::SelectTab7
        | ShortcutId::SelectTab8
        | ShortcutId::SelectLastTab => "Select this open tab; Mod+9 always selects the last tab.",
        ShortcutId::Quit => "Quit Jolt.",
        ShortcutId::Hide => "Hide Jolt.",
        ShortcutId::HideOthers => "Hide every application except Jolt.",
        ShortcutId::Minimize => "Minimize the current window.",
        ShortcutId::CloseWindow => "Close the current window while Settings is open.",
        ShortcutId::PerformanceHud => "Show or hide developer performance metrics.",
    }
}

impl Render for HotkeysPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;
        let theme = Theme::of(cx).clone();
        let recording = self.recording;
        let defaults = KeymapConfig::default();
        let customized = ShortcutId::all()
            .into_iter()
            .any(|id| self.keymap.get(id) != defaults.get(id));

        let hotkeys = ShortcutId::all();
        let mut categories = Vec::new();
        for category in HotkeyCategory::ALL {
            let category_hotkeys = hotkeys
                .iter()
                .copied()
                .enumerate()
                .filter(|(_, id)| HotkeyCategory::for_hotkey(*id) == category)
                .collect::<Vec<_>>();
            if category_hotkeys.is_empty() {
                continue;
            }

            let count = category_hotkeys.len();
            let collapsed = self.collapsed_categories.contains(&category);
            let chevron = if collapsed {
                crate::icons::CHEVRON_RIGHT
            } else {
                crate::icons::CHEVRON_DOWN
            };
            let rows = category_hotkeys
                .into_iter()
                .map(|(ix, id)| {
                    let combo = self.keymap.get(id).to_string();
                    let is_recording = recording == Some(id);
                    let non_default = combo != id.default_combo();
                    let chip_text: SharedString = if is_recording {
                        "Press keys…".into()
                    } else {
                        display_combo(&combo).into()
                    };
                    // Hotkey row: minimum 72px high, label and description
                    // left, optional Reset, then the combination chip.
                    div()
                        .min_h(px(72.0))
                        .px(px(20.0))
                        .border_t_1()
                        .border_color(theme.border)
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(20.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .text_size(px(13.0))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(theme.text)
                                        .child(SharedString::from(id.label())),
                                )
                                .child(
                                    div()
                                        .mt(px(2.0))
                                        .text_size(px(12.0))
                                        .text_color(theme.text_muted)
                                        .child(SharedString::from(description(id))),
                                ),
                        )
                        .when(non_default && !is_recording, |el| {
                            el.child(
                                div()
                                    .id(("hotkey-reset", ix))
                                    .text_size(px(11.0))
                                    .text_color(theme.text_muted.opacity(0.7))
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(theme.text))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_hotkey(id, id.default_combo().to_string(), cx);
                                    }))
                                    .child(SharedString::from("Reset")),
                            )
                        })
                        .child(
                            div()
                                .id(("hotkey-combo", ix))
                                .min_w(px(96.0))
                                .px(px(12.0))
                                .py(px(6.0))
                                .rounded(px(8.0))
                                .border_1()
                                .flex()
                                .justify_center()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(12.0))
                                .cursor_pointer()
                                .map(|el| {
                                    if is_recording {
                                        el.border_color(theme.text.opacity(0.3))
                                            .bg(theme.text)
                                            .text_color(theme.on_solid)
                                    } else {
                                        el.border_color(theme.border)
                                            .bg(theme.bg)
                                            .text_color(theme.text)
                                            .hover(|s| {
                                                s.border_color(theme.text.opacity(0.2))
                                                    .bg(crate::theme::ink(0.03))
                                            })
                                    }
                                })
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.recording = Some(id);
                                    this.conflict_notice = None;
                                    window.focus(&this.focus, cx);
                                    cx.notify();
                                }))
                                .child(chip_text),
                        )
                })
                .collect::<Vec<_>>();

            let header = div()
                .id(("hotkey-category", category.index()))
                .h(px(48.0))
                .px(px(20.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .cursor_pointer()
                .hover(|s| s.bg(crate::theme::ink(0.02)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_category(category, cx);
                }))
                .child(
                    crate::icons::icon(chevron)
                        .size(px(14.0))
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .text_size(px(13.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(SharedString::from(category.label())),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted.opacity(0.65))
                        .child(SharedString::from(count.to_string())),
                );

            categories.push(
                widgets::section_card(&theme)
                    .mt(px(0.0))
                    .child(header)
                    .when(!collapsed, |card| card.children(rows)),
            );
        }

        // The helper line stays muted even for a rejected conflict because the
        // message names the specific clash.
        let helper: SharedString = if recording.is_some() {
            "Press Escape to cancel.".into()
        } else if let Some(notice) = self.conflict_notice.clone() {
            notice
        } else {
            "Hotkeys must be unique.".into()
        };

        div()
            .id("hotkeys-page")
            .size_full()
            .overflow_y_scroll()
            .track_focus(&self.focus)
            .on_key_down(
                cx.listener(|this, event: &KeyDownEvent, _, cx| this.on_key_down(event, cx)),
            )
            .child(
                widgets::page_column()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_start()
                            .justify_between()
                            .gap(px(24.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(widgets::page_header(&theme, "Hotkeys", None))
                                    .child(
                                        widgets::page_subtitle(
                                            &theme,
                                            "Click a hotkey, then press the key combination you \
                                             want to use. Changes apply immediately and stay on \
                                             this device.",
                                        )
                                        .max_w(px(512.0))
                                        .line_height(px(20.0)),
                                    ),
                            )
                            .child({
                                // `disabled:opacity-35` when nothing is
                                // customized or while recording.
                                let disabled = !customized || recording.is_some();
                                widgets::ghost_action(&theme)
                                    .id("hotkeys-restore-defaults")
                                    .flex_none()
                                    .when(disabled, |el| el.opacity(0.35))
                                    .when(!disabled, |el| {
                                        el.hover(|s| {
                                            s.bg(crate::theme::ink(0.04)).text_color(theme.text)
                                        })
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.keymap = KeymapConfig::default();
                                                this.recording = None;
                                                this.conflict_notice = None;
                                                this.commit(cx);
                                            }),
                                        )
                                    })
                                    .child(
                                        crate::icons::icon(crate::icons::RELOAD)
                                            .size(px(14.0))
                                            .text_color(theme.text_muted),
                                    )
                                    .child(SharedString::from("Restore defaults"))
                            }),
                    )
                    .child(
                        div()
                            .mt(px(32.0))
                            .flex()
                            .flex_col()
                            .gap(px(16.0))
                            .children(categories),
                    )
                    .child(
                        div()
                            .mt(px(12.0))
                            .px(px(4.0))
                            .min_h(px(20.0))
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(helper),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_cover_every_hotkey() {
        for id in ShortcutId::all() {
            let _ = HotkeyCategory::for_hotkey(id);
        }
        assert_eq!(
            HotkeyCategory::for_hotkey(ShortcutId::NewSession),
            HotkeyCategory::SessionActions
        );
        assert_eq!(
            HotkeyCategory::for_hotkey(ShortcutId::PreviousTranscriptTurn),
            HotkeyCategory::SessionActions
        );
        assert_eq!(
            HotkeyCategory::for_hotkey(ShortcutId::SelectLastTab),
            HotkeyCategory::TabSwitching
        );
        assert_eq!(
            HotkeyCategory::for_hotkey(ShortcutId::ToggleTerminal),
            HotkeyCategory::NavigationLayout
        );
        assert_eq!(
            HotkeyCategory::for_hotkey(ShortcutId::SearchSessions),
            HotkeyCategory::NavigationLayout
        );
        assert_eq!(
            HotkeyCategory::for_hotkey(ShortcutId::OpenSpacesDropdown),
            HotkeyCategory::NavigationLayout
        );
        assert_eq!(
            HotkeyCategory::for_hotkey(ShortcutId::CloseWindow),
            HotkeyCategory::AppWindow
        );
        assert_eq!(
            HotkeyCategory::for_hotkey(ShortcutId::PerformanceHud),
            HotkeyCategory::Developer
        );
        assert_eq!(
            default_collapsed_categories(),
            HashSet::from([HotkeyCategory::TabSwitching])
        );
    }

    #[test]
    fn recording_outcomes() {
        assert_eq!(
            record_key("escape", false, false, false, false),
            RecordOutcome::Cancelled
        );
        assert_eq!(
            record_key("Escape", true, false, false, false),
            RecordOutcome::Cancelled
        );
        assert_eq!(
            record_key("s", true, false, false, false),
            RecordOutcome::Set("mod-s".into())
        );
        assert_eq!(
            record_key("k", false, true, true, true),
            RecordOutcome::Set("mod-alt-shift-k".into())
        );
        // Bare modifiers stay recording.
        assert_eq!(
            record_key("shift", false, false, true, false),
            RecordOutcome::Ignored
        );
        assert_eq!(
            record_key("ctrl", true, false, false, false),
            RecordOutcome::Ignored
        );
    }

    #[test]
    fn conflicting_records_are_refused() {
        // A combination bound elsewhere is refused at record time, so conflicts
        // never persist into the keymap.
        let keymap = KeymapConfig::default();
        let RecordOutcome::Set(combo) = record_key("b", true, false, false, false) else {
            panic!("expected Set");
        };
        assert_eq!(
            conflict_owner(&keymap, ShortcutId::ToggleSidebar, &combo),
            Some(ShortcutId::ToggleChanges)
        );
        // Re-recording a shortcut's own combo is not a conflict.
        assert_eq!(
            conflict_owner(&keymap, ShortcutId::ToggleChanges, &combo),
            None
        );
        // A free combo conflicts with nothing.
        assert_eq!(
            conflict_owner(&keymap, ShortcutId::ToggleSidebar, "mod-shift-x"),
            None
        );
        assert_eq!(
            conflict_owner(&keymap, ShortcutId::ToggleSidebar, "mod-k"),
            Some(ShortcutId::AddSpace)
        );
        assert_eq!(
            conflict_owner(&keymap, ShortcutId::ToggleSidebar, "mod-,"),
            Some(ShortcutId::OpenSettings)
        );
        // Close Tab and Close Window deliberately share Cmd+W. Chat mode
        // consumes it; Settings propagates to the native window action.
        assert_eq!(
            conflict_owner(
                &keymap,
                ShortcutId::CloseTab,
                keymap.get(ShortcutId::CloseWindow)
            ),
            None
        );
    }
}
