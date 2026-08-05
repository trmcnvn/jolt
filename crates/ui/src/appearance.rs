//! Appearance and typography switching: what the user selected, what the OS
//! reports, and the plumbing that turns a change into a full repaint.
//!
//! Three pieces, following the pattern zed uses (`crates/theme/src/theme.rs`
//! `SystemAppearance` + `reload_theme` + `cx.refresh_windows`):
//!
//! 1. [`AppearanceMode`] — the persisted user choice: follow the OS, or pin one.
//! 2. [`AppearanceState`] — a gpui global holding that choice alongside the last
//!    appearance the OS reported, so [`resolve`] can combine them.
//! 3. [`observe_window`] — subscribes to the platform's appearance notification
//!    (macOS `viewDidChangeEffectiveAppearance`) and re-applies.
//!
//! # Why `refresh_windows` and not `notify`
//!
//! Colors are read *imperatively* (`Theme::of(cx).text`) at paint time, not
//! through a reactive binding, so no view knows its colors went stale — a
//! `notify()` on some entity would repaint that entity and nothing else.
//! [`App::refresh_windows`] marks every window dirty *and* disables gpui's
//! per-view prepaint cache for the frame, which is the only thing that forces
//! already-laid-out elements to re-run their paint with the new palette.

use std::path::{Path, PathBuf};

use gpui::{App, Global, SharedString, Subscription, Window};
use serde::{Deserialize, Serialize};

use crate::settings::UiSettings;
use crate::theme::{Appearance, DEFAULT_CODE_FONT, DEFAULT_UI_FONT, Theme};

/// The user's appearance preference. Persisted in `ui-settings.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AppearanceMode {
    /// Follow the OS. The default — matches every other native app on the
    /// machine, including when the user has macOS set to switch at sunset.
    #[default]
    System,
    Light,
    Dark,
}

impl AppearanceMode {
    /// Menu/label text.
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];
}

/// Global state behind the current theme: what the user chose, and what the OS
/// last said. Kept separate from [`Theme`] itself so that flipping the OS
/// appearance while the user has pinned Light still records the new system value
/// (and takes effect the moment they switch back to `System`).
pub struct AppearanceState {
    pub mode: AppearanceMode,
    pub system: Appearance,
    pub ui_font: SharedString,
    pub code_font: SharedString,
    pub terminal_font: SharedString,
    /// Where `ui-settings.json` lives, so a menu action can persist the choice
    /// without routing through the shell entity that normally owns settings.
    pub data_dir: PathBuf,
}

impl Global for AppearanceState {}

/// Which part of Jolt a font preference controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontRole {
    Ui,
    Code,
    Terminal,
}

/// Combine the user's choice with the OS state.
pub fn resolve(mode: AppearanceMode, system: Appearance) -> Appearance {
    match mode {
        AppearanceMode::System => system,
        AppearanceMode::Light => Appearance::Light,
        AppearanceMode::Dark => Appearance::Dark,
    }
}

/// Install the appearance globals and the matching theme. Call once at boot,
/// before any window opens, so the first frame is already the right palette
/// (installing later produces a visible dark-to-light flash).
pub fn init(
    mode: AppearanceMode,
    ui_font: impl AsRef<str>,
    code_font: impl AsRef<str>,
    terminal_font: impl AsRef<str>,
    data_dir: impl Into<PathBuf>,
    cx: &mut App,
) {
    let system = Appearance::from_window(cx.window_appearance());
    let available = cx.text_system().all_font_names();
    let ui_font = resolve_font_family(ui_font.as_ref(), DEFAULT_UI_FONT, &available);
    let code_font = resolve_font_family(code_font.as_ref(), DEFAULT_CODE_FONT, &available);
    let terminal_font = resolve_font_family(terminal_font.as_ref(), DEFAULT_CODE_FONT, &available);
    tracing::debug!(?mode, ?system, "appearance: initial");
    cx.set_global(AppearanceState {
        mode,
        system,
        ui_font: ui_font.clone().into(),
        code_font: code_font.clone().into(),
        terminal_font: terminal_font.clone().into(),
        data_dir: data_dir.into(),
    });
    sync_ns_appearance(mode);
    Theme::install_with_fonts(resolve(mode, system), ui_font, code_font, terminal_font, cx);
}

/// The mode currently in effect (defaults to `System` before [`init`]).
pub fn mode(cx: &App) -> AppearanceMode {
    cx.try_global::<AppearanceState>()
        .map(|s| s.mode)
        .unwrap_or_default()
}

/// The effective UI, code, and terminal font families.
pub fn font_families(cx: &App) -> (SharedString, SharedString, SharedString) {
    cx.try_global::<AppearanceState>()
        .map(|state| {
            (
                state.ui_font.clone(),
                state.code_font.clone(),
                state.terminal_font.clone(),
            )
        })
        .unwrap_or_else(|| {
            (
                DEFAULT_UI_FONT.into(),
                DEFAULT_CODE_FONT.into(),
                DEFAULT_CODE_FONT.into(),
            )
        })
}

/// Change the user's preference, repaint if that changed the palette, and write
/// the choice to disk.
pub fn set_mode(mode: AppearanceMode, cx: &mut App) {
    if !cx.has_global::<AppearanceState>() {
        return;
    }
    let state = cx.global_mut::<AppearanceState>();
    if state.mode == mode {
        return;
    }
    state.mode = mode;
    let data_dir = state.data_dir.clone();
    let ui_font = state.ui_font.clone();
    let code_font = state.code_font.clone();
    let terminal_font = state.terminal_font.clone();
    apply(cx);
    persist(mode, &ui_font, &code_font, &terminal_font, &data_dir);
}

/// Change one font family, immediately re-lay out every window, and persist it.
pub fn set_font(role: FontRole, family: impl AsRef<str>, cx: &mut App) {
    if !cx.has_global::<AppearanceState>() {
        return;
    }
    let fallback = match role {
        FontRole::Ui => DEFAULT_UI_FONT,
        FontRole::Code | FontRole::Terminal => DEFAULT_CODE_FONT,
    };
    let available = cx.text_system().all_font_names();
    let family = resolve_font_family(family.as_ref(), fallback, &available);
    let state = cx.global_mut::<AppearanceState>();
    let target = match role {
        FontRole::Ui => &mut state.ui_font,
        FontRole::Code => &mut state.code_font,
        FontRole::Terminal => &mut state.terminal_font,
    };
    if target.as_ref() == family {
        return;
    }
    *target = family.into();
    let mode = state.mode;
    let ui_font = state.ui_font.clone();
    let code_font = state.code_font.clone();
    let terminal_font = state.terminal_font.clone();
    let data_dir = state.data_dir.clone();
    apply(cx);
    persist(mode, &ui_font, &code_font, &terminal_font, &data_dir);
}

/// Resolve a persisted family against the current machine's catalogue. Family
/// names are matched case-insensitively, preserving the platform's spelling.
pub fn resolve_font_family(requested: &str, fallback: &str, available: &[String]) -> String {
    let requested = requested.trim();
    available
        .iter()
        .find(|name| name.eq_ignore_ascii_case(requested))
        .or_else(|| {
            available
                .iter()
                .find(|name| name.eq_ignore_ascii_case(fallback))
        })
        .cloned()
        .unwrap_or_else(|| fallback.to_string())
}

/// Read-modify-write `ui-settings.json` for the appearance-owned fields.
///
/// Deliberately a fresh load rather than a write of some cached struct: the
/// shell holds its own `UiSettings` and saves it debounced, so writing a stale
/// snapshot from here would silently roll back a pane resize the user made
/// seconds earlier. Reloading keeps this to the fields appearance owns.
fn persist(
    mode: AppearanceMode,
    ui_font: &str,
    code_font: &str,
    terminal_font: &str,
    data_dir: &Path,
) {
    let mut settings = UiSettings::load(data_dir);
    settings.appearance = mode;
    settings.ui_font = ui_font.to_string();
    settings.code_font = code_font.to_string();
    settings.terminal_font = terminal_font.to_string();
    if let Err(err) = settings.save(data_dir) {
        tracing::warn!(error = %err, "could not persist appearance");
    }
}

/// Subscribe a window to OS appearance changes. The returned [`Subscription`]
/// must outlive the window — callers typically `.detach()` it.
///
/// The notification is *per window*, but the appearance it reports is a system
/// setting, so any one window is enough to learn about the change; re-applying
/// is idempotent when several fire.
pub fn observe_window(window: &mut Window, cx: &mut App) -> Subscription {
    // Reconcile against the *window's* appearance before subscribing.
    //
    // [`init`] runs before any window exists and can only ask the platform
    // (`App::window_appearance`), which on macOS reads `NSApp.effectiveAppearance`
    // — and that is not reliably populated that early in launch. When it guesses
    // wrong the app paints the wrong palette until some unrelated event happens to
    // fire the appearance notification, which reads as "it booted dark and fixed
    // itself when I clicked something". The window knows for certain, so ask it.
    sync(Appearance::from_window(window.appearance()), cx);
    window.observe_window_appearance(|window, cx| {
        sync(Appearance::from_window(window.appearance()), cx);
    })
}

/// Record the OS appearance and re-apply if it moved.
fn sync(system: Appearance, cx: &mut App) {
    if !cx.has_global::<AppearanceState>() {
        return;
    }
    let state = cx.global_mut::<AppearanceState>();
    if state.system == system {
        return;
    }
    tracing::debug!(?system, "appearance: system changed");
    state.system = system;
    apply(cx);
}

/// Re-resolve the palette and typography and, if either moved, swap the theme
/// and force a full repaint. A no-op when the effective theme is unchanged —
/// the OS fires notifications for vibrancy and accent-color changes too.
pub fn apply(cx: &mut App) {
    let Some(state) = cx.try_global::<AppearanceState>() else {
        return;
    };
    sync_ns_appearance(state.mode);
    let wanted = resolve(state.mode, state.system);
    let ui_font = state.ui_font.clone();
    let code_font = state.code_font.clone();
    let terminal_font = state.terminal_font.clone();
    let changed = !cx.try_global::<Theme>().is_some_and(|t| {
        t.appearance == wanted
            && t.font_sans == ui_font
            && t.font_mono == code_font
            && t.font_terminal == terminal_font
    });
    if changed {
        tracing::debug!(?wanted, %ui_font, %code_font, %terminal_font, "appearance: installing theme");
        Theme::install_with_fonts(wanted, ui_font, code_font, terminal_font, cx);
        cx.refresh_windows();
    }
    // Unconditional, even when the palette did not move: this is the only thing
    // that keeps macOS vibrancy alive. gpui's macOS backend removes the
    // `NSVisualEffectView` from the window the moment the background appearance
    // is anything but `Blurred`, and nothing puts it back on its own — so a
    // single missed re-apply leaves the sidebar and tab strip permanently
    // opaque, which is exactly how the frost died. zed runs the same loop on
    // every settings change (`crates/zed/src/main.rs`).
    reapply_window_background(cx);
}

/// Tell AppKit which appearance the app's windows use, so the chrome *it*
/// draws — the traffic lights above all — matches the palette *we* paint.
/// gpui never sets `NSAppearance`, so before this a pinned in-app theme left
/// the window chrome following the OS setting: a light window rendered
/// dark-appearance inactive traffic lights when the system was dark (user
/// report). Pinned modes name the appearance explicitly; `System` clears the
/// override (`setAppearance: nil`) so AppKit follows the OS again — resolving
/// to a name there too would freeze the chrome across OS sunset switches
/// until our own notification round-trip repainted it.
#[cfg(target_os = "macos")]
fn sync_ns_appearance(mode: AppearanceMode) {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    // NSAppearanceName constants are NSStrings whose value equals the
    // constant's own name (AppKit documents them as stable identifiers), so
    // building them from literals avoids linking the extern statics.
    let name = match mode {
        AppearanceMode::System => None,
        AppearanceMode::Light => Some(c"NSAppearanceNameAqua"),
        AppearanceMode::Dark => Some(c"NSAppearanceNameDarkAqua"),
    };
    unsafe {
        let appearance: *mut Object = match name {
            None => std::ptr::null_mut(),
            Some(name) => {
                let name: *mut Object =
                    msg_send![class!(NSString), stringWithUTF8String: name.as_ptr()];
                msg_send![class!(NSAppearance), appearanceNamed: name]
            }
        };
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, setAppearance: appearance];
    }
}

#[cfg(not(target_os = "macos"))]
fn sync_ns_appearance(_mode: AppearanceMode) {}

/// Push the theme's window background appearance onto every open window.
pub fn reapply_window_background(cx: &mut App) {
    let Some(wanted) = cx
        .try_global::<Theme>()
        .map(|theme| theme.window_background_appearance())
    else {
        return;
    };
    for window in cx.windows() {
        window
            .update(cx, |_, window, _| {
                window.set_background_appearance(wanted);
            })
            .ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_mode_follows_the_os() {
        assert_eq!(
            resolve(AppearanceMode::System, Appearance::Light),
            Appearance::Light
        );
        assert_eq!(
            resolve(AppearanceMode::System, Appearance::Dark),
            Appearance::Dark
        );
    }

    #[test]
    fn pinned_modes_ignore_the_os() {
        for system in [Appearance::Light, Appearance::Dark] {
            assert_eq!(resolve(AppearanceMode::Light, system), Appearance::Light);
            assert_eq!(resolve(AppearanceMode::Dark, system), Appearance::Dark);
        }
    }

    #[test]
    fn default_mode_is_system() {
        assert_eq!(AppearanceMode::default(), AppearanceMode::System);
    }

    #[test]
    fn font_resolution_preserves_available_names_and_heals_missing_ones() {
        let available = vec!["Geist".into(), "Geist Mono".into(), "Menlo".into()];
        assert_eq!(
            resolve_font_family("menlo", DEFAULT_CODE_FONT, &available),
            "Menlo"
        );
        assert_eq!(
            resolve_font_family("Removed Font", DEFAULT_CODE_FONT, &available),
            DEFAULT_CODE_FONT
        );
        assert_eq!(
            resolve_font_family("", DEFAULT_UI_FONT, &available),
            DEFAULT_UI_FONT
        );
    }

    /// The setting round-trips through the settings file as a lowercase string.
    #[test]
    fn mode_serialises_stably() {
        for (mode, json) in [
            (AppearanceMode::System, "\"system\""),
            (AppearanceMode::Light, "\"light\""),
            (AppearanceMode::Dark, "\"dark\""),
        ] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), json);
            assert_eq!(
                serde_json::from_str::<AppearanceMode>(json).unwrap(),
                mode,
                "{json} should parse back"
            );
        }
    }
}
