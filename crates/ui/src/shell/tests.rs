//! Module behavior tests.
use super::*;

#[test]
fn stale_engine_does_not_announce_the_installed_app_version() {
    let status = jolt_update::UpdateStatus {
        current_version: "0.1.4".into(),
        latest_version: Some(jolt_update::current_version().into()),
        update_available: true,
        can_apply: false,
        checked_at: None,
        error: None,
    };

    assert!(!app_update_available(&status));
}

#[test]
fn account_usage_warnings_match_account_meter_thresholds() {
    assert_eq!(usage_warning_level(0.79), UsageWarningLevel::Normal);
    assert_eq!(usage_warning_level(0.80), UsageWarningLevel::Warning);
    assert_eq!(usage_warning_level(0.94), UsageWarningLevel::Warning);
    assert_eq!(usage_warning_level(0.95), UsageWarningLevel::Danger);
}

#[test]
fn usage_breakdowns_preserve_devices_while_merging_totals() {
    assert_eq!(format_usage_cost(Some(41_615.03)), "$41,615.03");
    let row = |tokens, cost_provenance| UsageBreakdownRow {
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
        cost_provenance: Some(cost_provenance),
    };
    let report = |device: &str, tokens, cost_provenance| UsageBreakdown {
        device_id: device.into(),
        days: 30,
        sessions: 1,
        calls: 1,
        input_tokens: tokens,
        cost_usd: Some(0.25),
        cost_provenance: Some(cost_provenance),
        activity: vec![UsageDay {
            day: "2026-08-06".into(),
            tokens,
            calls: 1,
            cost_usd: Some(0.25),
            cost_provenance: Some(cost_provenance),
        }],
        rows: vec![row(tokens, cost_provenance)],
        ..UsageBreakdown::default()
    };
    let merged = merge_breakdowns(
        30,
        vec![
            report("a", 10, CostProvenance::ProviderReported),
            report("b", 20, CostProvenance::ModelEstimated),
        ],
    );
    assert_eq!(merged.totals.sessions, 2);
    assert_eq!(merged.totals.total_tokens(), 30);
    assert_eq!(merged.totals.cost_usd, Some(0.5));
    assert_eq!(merged.totals.cost_provenance, Some(CostProvenance::Mixed));
    assert_eq!(merged.totals.activity[0].tokens, 30);
    assert_eq!(merged.rows.len(), 2);
    assert_eq!(merged.rows[0].device_id, "b");
    assert_eq!(merged.rows[0].usage.sessions, 1);
    assert_eq!(merged.rows[0].usage.total_tokens(), 20);
    let harnesses = aggregate_harness_usage(&merged.rows);
    assert_eq!(harnesses.len(), 1);
    assert_eq!(harnesses[0].tokens, 30);
    assert_eq!(harnesses[0].cost_usd, Some(0.5));
    assert_eq!(harnesses[0].cost_provenance, Some(CostProvenance::Mixed));
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
fn session_hotkeys_cover_nine_sidebar_positions() {
    let mut keymap = KeymapConfig::default();
    keymap.set(ShortcutId::SelectSession1, "mod-shift-1".into());
    let bindings = session_key_bindings(&keymap);
    assert_eq!(bindings.len(), ShortcutId::SESSION_SELECTION.len());

    for (index, binding) in bindings.iter().enumerate() {
        let id = ShortcutId::SESSION_SELECTION[index];
        let expected =
            Keystroke::parse(&platform_combo(keymap.get(id))).expect("session hotkey must parse");
        let actual = binding
            .keystrokes()
            .iter()
            .map(|keystroke| keystroke.inner().clone())
            .collect::<Vec<_>>();
        assert_eq!(actual, vec![expected]);
        assert_eq!(
            binding.action().as_any().downcast_ref::<SelectSession>(),
            Some(&SelectSession(index))
        );
    }
}

#[test]
fn session_shortcut_hints_cover_only_switchable_rows() {
    let mut keymap = KeymapConfig::default();
    keymap.set(ShortcutId::SelectSession1, "mod-shift-1".into());

    assert_eq!(
        session_shortcut_hint(&keymap, 0, true),
        Some(display_combo("mod-shift-1"))
    );
    assert_eq!(
        session_shortcut_hint(&keymap, 8, true),
        Some(display_combo("mod-9"))
    );
    assert_eq!(session_shortcut_hint(&keymap, 9, true), None);
    assert_eq!(session_shortcut_hint(&keymap, 0, false), None);
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

// ---- sidebar list ----

#[test]
fn closed_header_takes_over_after_its_inline_heading_scrolls_away() {
    assert!(!closed_header_is_sticky(4, 5));
    assert!(!closed_header_is_sticky(5, 5));
    assert!(closed_header_is_sticky(6, 5));
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
