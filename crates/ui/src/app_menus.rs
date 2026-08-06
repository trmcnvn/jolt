//! Native menu bar + app-level window actions (macOS-first).
//!
//! jolt never called `cx.set_menus`, so on macOS `NSApp.mainMenu` stayed nil:
//! no app menu, no ⌘Q quit, and nothing for the auto-hidden system menu bar to
//! reveal on hover (gpui only calls `setMainMenu_` from `set_menus` —
//! gpui_macos/src/platform.rs `fn set_menus`). Structure ported from zed's
//! `crates/zed/src/zed/app_menus.rs` and the gpui `set_menus.rs` example at the
//! pinned rev (f14fea9bf3c9).
//!
//! Wiring: [`init`] registers the global action handlers (run once at boot),
//! [`bind_keys`] installs the customizable macOS menu hotkeys (re-run by
//! `shell::apply_keymap`, which clears every binding first), and
//! [`app_menus`] builds the menu bar handed to `cx.set_menus` in `run_app`.

use gpui::{App, KeyBinding, Menu, MenuItem, OsAction, SystemMenuType, Window, actions};

use crate::appearance::{self, AppearanceMode};
use crate::composer;
#[cfg(any(debug_assertions, feature = "debug-ui"))]
use crate::debug::TogglePerformanceHud;
use crate::settings::{KeymapConfig, ShortcutId, platform_combo};

actions!(
    jolt,
    [
        About,
        Quit,
        Hide,
        HideOthers,
        ShowAll,
        Minimize,
        Zoom,
        CloseWindow,
        AppearanceSystem,
        AppearanceLight,
        AppearanceDark,
    ]
);

/// Register the global handlers backing the menu bar and its hotkeys. Call
/// once at boot, before `cx.set_menus`.
pub fn init(cx: &mut App) {
    cx.on_action(quit);
    // Application-menu verbs — gpui wraps NSApp `hide` / `hideOtherApplications`
    // / `unhideAllApplications` (zed registers the same trio in
    // crates/zed/src/zed.rs `init`).
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    // Window verbs route to the active window. jolt is single-window, so a
    // global handler suffices where zed registers these per-workspace
    // (crates/zed/src/zed.rs `register_action(Minimize/Zoom)`).
    cx.on_action(|_: &Minimize, cx| with_active_window(cx, |window| window.minimize_window()));
    cx.on_action(|_: &Zoom, cx| with_active_window(cx, |window| window.zoom_window()));
    cx.on_action(|_: &CloseWindow, cx| with_active_window(cx, |window| window.remove_window()));
    // Appearance. Each verb persists and repaints every window; see
    // `appearance::set_mode`.
    cx.on_action(|_: &AppearanceSystem, cx| appearance::set_mode(AppearanceMode::System, cx));
    cx.on_action(|_: &AppearanceLight, cx| appearance::set_mode(AppearanceMode::Light, cx));
    cx.on_action(|_: &AppearanceDark, cx| appearance::set_mode(AppearanceMode::Dark, cx));
}

fn with_active_window(cx: &mut App, f: impl FnOnce(&mut Window)) {
    if let Some(window) = cx.active_window() {
        window.update(cx, |_, window, _| f(window)).ok();
    }
}

/// ⌘Q / "Quit Jolt". `cx.quit()` runs the platform's standard quit routine,
/// which invokes gpui `App::shutdown` — that fires the `on_app_quit` observers
/// registered in `run_app` (embedded-engine drain: live runs + doc snapshot
/// flush) with gpui's shutdown timeout before the process exits. Same graceful
/// path as quitting from the Dock or closing the last window.
fn quit(_: &Quit, cx: &mut App) {
    cx.quit();
}

/// Customizable app hotkeys backing native menu key equivalents. macOS only —
/// on Linux/Windows these menu commands have no app-level accelerator.
pub fn bind_keys(cx: &mut App, keymap: &KeymapConfig) {
    if !cfg!(target_os = "macos") {
        return;
    }
    cx.bind_keys(macos_key_bindings(keymap));
}

/// The binding table behind [`bind_keys`] — `KeyBinding` construction is pure
/// (no `App`), so unit tests can inspect it directly.
fn macos_key_bindings(keymap: &KeymapConfig) -> Vec<KeyBinding> {
    fn binding<A: gpui::Action>(keymap: &KeymapConfig, id: ShortcutId, action: A) -> KeyBinding {
        let combo = platform_combo(keymap.get(id));
        let combo = if gpui::Keystroke::parse(&combo).is_ok() {
            combo
        } else {
            platform_combo(id.default_combo())
        };
        KeyBinding::new(&combo, action, None)
    }

    vec![
        binding(keymap, ShortcutId::Quit, Quit),
        binding(keymap, ShortcutId::Hide, Hide),
        binding(keymap, ShortcutId::HideOthers, HideOthers),
        binding(keymap, ShortcutId::Minimize, Minimize),
        binding(keymap, ShortcutId::CloseWindow, CloseWindow),
    ]
}

/// The jolt menu bar. macOS renders this natively; mac-only entries are gated
/// at runtime (`cfg!`) so the whole module compiles and tests on Linux.
pub fn app_menus() -> Vec<Menu> {
    let macos = cfg!(target_os = "macos");

    // macOS titles the first menu with the bundle/process name regardless of
    // what we pass, but gpui still wants a name.
    let mut app_items = vec![
        // Placeholder until a real about dialog exists (explicitly disabled).
        MenuItem::action("About Jolt", About).disabled(true),
        MenuItem::separator(),
    ];
    if macos {
        app_items.extend([
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide Jolt", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
        ]);
    }
    app_items.push(MenuItem::action("Quit Jolt", Quit));

    let mut menus = vec![
        Menu::new("Jolt").items(app_items),
        // Standard clipboard verbs tied to the composer's existing actions via
        // their native selectors (`OsAction` → cut:/copy:/paste:/selectAll:),
        // so the OS Edit menu routes through the responder chain to the focused
        // input — zed wires its editor actions identically
        // (crates/zed/src/zed/app_menus.rs, Edit/Selection menus).
        Menu::new("Edit").items([
            // Undo/Redo have no `OsAction` counterpart — they dispatch as plain
            // actions to the focused input, same as the composer keymap.
            MenuItem::action("Undo", composer::Undo),
            MenuItem::action("Redo", composer::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", composer::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", composer::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", composer::Paste, OsAction::Paste),
            MenuItem::separator(),
            MenuItem::os_action("Select All", composer::SelectAll, OsAction::SelectAll),
        ]),
    ];
    // Appearance lives under View on every platform — it is the only View verb
    // today, but "Appearance" as a top-level menu would read oddly next to Edit.
    let view_items = vec![
        MenuItem::action("Appearance: System", AppearanceSystem),
        MenuItem::action("Appearance: Light", AppearanceLight),
        MenuItem::action("Appearance: Dark", AppearanceDark),
    ];
    #[cfg(any(debug_assertions, feature = "debug-ui"))]
    let view_items = {
        let mut items = view_items;
        items.push(MenuItem::submenu(Menu::new("Developer").items([
            MenuItem::action("Performance HUD", TogglePerformanceHud),
        ])));
        items
    };
    menus.push(Menu::new("View").items(view_items));
    if macos {
        // Standard Window menu; macOS appends the open-window list itself.
        menus.push(Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
            MenuItem::separator(),
            MenuItem::action("Close Window", CloseWindow),
        ]));
    }
    menus
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Action as _, Keystroke};

    fn action_names(menu: &Menu) -> Vec<&'static str> {
        menu.items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action { action, .. } => Some(action.name()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn app_menu_ends_with_quit() {
        let menus = app_menus();
        assert_eq!(menus[0].name.as_ref(), "Jolt");
        let Some(MenuItem::Action { name, action, .. }) = menus[0].items.last() else {
            panic!("last app-menu item must be an action");
        };
        assert_eq!(name.as_ref(), "Quit Jolt");
        assert_eq!(action.name(), Quit.name());
    }

    #[test]
    fn about_is_disabled_placeholder() {
        let menus = app_menus();
        let first = &menus[0].items[0];
        assert!(
            first.is_disabled(),
            "About stays disabled until implemented"
        );
    }

    #[test]
    fn edit_menu_uses_composer_clipboard_os_actions() {
        let menus = app_menus();
        let edit = menus
            .iter()
            .find(|m| m.name.as_ref() == "Edit")
            .expect("Edit menu present");
        // `OsAction` has no `Debug` impl at the pinned rev, so compare
        // per-field.
        let expect = [
            (composer::Cut.name(), OsAction::Cut),
            (composer::Copy.name(), OsAction::Copy),
            (composer::Paste.name(), OsAction::Paste),
            (composer::SelectAll.name(), OsAction::SelectAll),
        ];
        let got: Vec<(&str, OsAction)> = edit
            .items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action {
                    action,
                    os_action: Some(os_action),
                    ..
                } => Some((action.name(), *os_action)),
                _ => None,
            })
            .collect();
        assert_eq!(got.len(), expect.len());
        for ((got_name, got_os), (want_name, want_os)) in got.iter().zip(expect.iter()) {
            assert_eq!(got_name, want_name);
            assert!(got_os == want_os, "OsAction mismatch for {want_name}");
        }
    }

    #[test]
    fn app_menu_has_no_conversation_menu() {
        assert!(
            app_menus()
                .iter()
                .all(|menu| menu.name.as_ref() != "Conversation")
        );
    }

    #[test]
    fn view_menu_offers_all_three_appearance_modes() {
        let menus = app_menus();
        let view = menus
            .iter()
            .find(|m| m.name.as_ref() == "View")
            .expect("View menu present");
        assert_eq!(
            action_names(view),
            vec![
                AppearanceSystem.name(),
                AppearanceLight.name(),
                AppearanceDark.name()
            ]
        );
    }

    #[cfg(any(debug_assertions, feature = "debug-ui"))]
    #[test]
    fn view_menu_offers_performance_hud_in_developer_submenu() {
        let menus = app_menus();
        let view = menus
            .iter()
            .find(|menu| menu.name.as_ref() == "View")
            .expect("View menu present");
        let developer = view
            .items
            .iter()
            .find_map(|item| match item {
                MenuItem::Submenu(menu) if menu.name.as_ref() == "Developer" => Some(menu),
                _ => None,
            })
            .expect("Developer submenu present");
        assert_eq!(action_names(developer), vec![TogglePerformanceHud.name()]);
    }

    #[test]
    fn macos_hotkeys_are_configurable() {
        // `KeyBinding::new` panics on unparseable combos, so constructing the
        // table is itself the parse check.
        let mut keymap = KeymapConfig::default();
        keymap.set(ShortcutId::Quit, "mod-shift-q".into());
        let bindings = macos_key_bindings(&keymap);
        let find = |name: &str| {
            bindings
                .iter()
                .find(|binding| binding.action().name() == name)
                .map(|binding| {
                    binding
                        .keystrokes()
                        .iter()
                        .map(|ks| ks.inner().clone())
                        .collect::<Vec<_>>()
                })
        };
        let combo = |source: &str| vec![Keystroke::parse(source).unwrap()];
        assert_eq!(find(Quit.name()), Some(combo("cmd-shift-q")));
        assert_eq!(find(CloseWindow.name()), Some(combo("cmd-w")));
        assert_eq!(find(Minimize.name()), Some(combo("cmd-m")));
    }
}
