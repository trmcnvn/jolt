//! Embedded icon assets + the gpui [`AssetSource`] that serves them.
//!
//! The asset source also exposes Jolt's embedded Geist fonts at the two
//! fallback paths GPUI's SVG renderer requests. GPUI keeps a font database for
//! SVG text separate from its native text system, so registering the fonts at
//! app startup is not enough to make them available to Mermaid diagrams.
//!
//! UI glyphs come from **Tabler Icons** by Paweł Kuna, licensed under the MIT
//! License (https://github.com/tabler/tabler-icons). The upstream 24px outline
//! assets use a 2px stroke; Jolt normalizes them to 1.5px to suit its compact UI.
//! The Jolt logo is hand-drawn. `pi-mark` is Pi's official mark from
//! https://pi.dev/logo-auto.svg.
//! - `jj-mark` is Jujutsu's official logo from docs.jj-vcs.dev, © 2025 J.
//!   Jennings, adapted to SVG by Lucas Garron, licensed CC BY 4.0. Its opaque
//!   app-icon background is removed because gpui renders SVGs as tinted alpha
//!   masks and would otherwise show only a solid square.
//! gpui tints SVGs with the text color, so the Claude mark's brand orange is
//! applied at the call site
//!   ([`CLAUDE_BRAND`]).
//!
//! Icons render via [`icon`]: `icon(icons::PAPERCLIP).size(px(16.)).text_color(…)`.

use std::borrow::Cow;

use gpui::{AssetSource, Hsla, Result, SharedString, Styled as _, Svg, svg};

macro_rules! icon_assets {
    ($(($const_name:ident, $path:literal)),+ $(,)?) => {
        $(pub const $const_name: &str = concat!("icons/", $path, ".svg");)+

        /// Serves the embedded icons to gpui's SVG renderer.
        pub struct Assets;

        impl AssetSource for Assets {
            fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
                Ok(match path {
                    // Compatibility shim for GPUI's hard-coded SVG fallback
                    // asset paths. The family names come from the font files,
                    // so these load as Geist / Geist Mono rather than aliases
                    // for IBM Plex Sans / Lilex.
                    "fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf" =>
                        Some(Cow::Borrowed(crate::FONT_GEIST)),
                    "fonts/lilex/Lilex-Regular.ttf" =>
                        Some(Cow::Borrowed(crate::FONT_GEIST_MONO)),
                    $(concat!("icons/", $path, ".svg") => Some(Cow::Borrowed(
                        include_bytes!(concat!("../assets/icons/", $path, ".svg")).as_slice(),
                    )),)+
                    _ => None,
                })
            }

            fn list(&self, path: &str) -> Result<Vec<SharedString>> {
                let all = [
                    "fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf",
                    "fonts/lilex/Lilex-Regular.ttf",
                    $(concat!("icons/", $path, ".svg")),+
                ];
                Ok(all
                    .iter()
                    .filter(|p| p.starts_with(path))
                    .map(|p| SharedString::from(*p))
                    .collect())
            }
        }
    };
}

icon_assets![
    // Tabler Icons (Outline), MIT — Paweł Kuna. Upstream names are preserved.
    (DEVICE_DESKTOP, "device-desktop"),
    (DEVICE_LAPTOP, "device-laptop"),
    (SQUARE_ROUNDED_PLUS, "square-rounded-plus"),
    (SWITCH_VERTICAL, "switch-vertical"),
    (LIST, "list"),
    (FOLDERS, "folders"),
    (FOLDER, "folder"),
    (GIT_BRANCH, "git-branch"),
    (GIT_PULL_REQUEST, "git-pull-request"),
    (LAYOUT_LIST, "layout-list"),
    (LAYOUT_COLUMNS, "layout-columns"),
    (LAYOUT_BOTTOMBAR, "layout-bottombar"),
    (LAYOUT_SIDEBAR, "layout-sidebar"),
    (KEY, "key"),
    (KEYBOARD, "keyboard"),
    (ARROW_LEFT, "arrow-left"),
    (ARROW_RIGHT, "arrow-right"),
    (ARROW_UP, "arrow-up"),
    (ARROW_DOWN, "arrow-down"),
    (CORNER_DOWN_LEFT, "corner-down-left"),
    (CHEVRON_DOWN, "chevron-down"),
    (CHEVRON_LEFT, "chevron-left"),
    (CHEVRON_RIGHT, "chevron-right"),
    (DEVICE_MOBILE, "device-mobile"),
    (RESTORE, "restore"),
    (REFRESH, "refresh"),
    (RELOAD, "reload"),
    (CIRCLE_PLUS, "circle-plus"),
    (ADJUSTMENTS_HORIZONTAL, "adjustments-horizontal"),
    (BELL, "bell"),
    (PAPERCLIP, "paperclip"),
    (PENCIL, "pencil"),
    (ARCHIVE, "archive"),
    (TRASH, "trash"),
    (SETTINGS, "settings"),
    (LOGOUT, "logout"),
    (USER, "user"),
    (SEARCH, "search"),
    (COMMAND, "command"),
    (FILE, "file"),
    (FILE_PLUS, "file-plus"),
    (WORLD, "world"),
    (LIST_CHECK, "list-check"),
    (APPS, "apps"),
    (CIRCLE_X, "circle-x"),
    (INFO_CIRCLE, "info-circle"),
    (ALERT_TRIANGLE, "alert-triangle"),
    (MESSAGE_CIRCLE, "message-circle"),
    (TERMINAL_2, "terminal-2"),
    (PLUS, "plus"),
    (X, "x"),
    (ARROWS_DIAGONAL_2, "arrows-diagonal-2"),
    (ARROWS_DIAGONAL_MINIMIZE, "arrows-diagonal-minimize"),
    (SQUARE, "square"),
    (CHECK, "check"),
    (COPY, "copy"),
    // Hand-drawn Jolt glyph.
    (JOLT_LOGO, "jolt-logo"),
    // Harness brand marks.
    (CLAUDE_MARK, "claude-mark"),
    (OPENAI_MARK, "openai-mark"),
    (PI_MARK, "pi-mark"),
    (JJ_MARK, "jj-mark"),
];

/// The Claude mark's brand orange (`#D97757`) — jolt keeps it even on the
/// monochrome surface.
pub fn claude_brand() -> Hsla {
    gpui::rgb(0xD97757).into()
}

/// An icon element for an embedded asset path. Size and colour are set by the
/// caller (`.size(..)`, `.text_color(..)`), matching the web app's
/// `[&_svg]:size-4` idiom.
pub fn icon(path: &'static str) -> Svg {
    svg().path(path).flex_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_icon_loads_and_parses() {
        let assets = Assets;
        for path in assets.list("icons/").unwrap() {
            let bytes = assets
                .load(&path)
                .unwrap()
                .unwrap_or_else(|| panic!("missing asset {path}"));
            let text = std::str::from_utf8(&bytes).expect("icon svg is utf-8");
            assert!(text.contains("<svg"), "{path} is not an svg");
            assert!(text.contains("viewBox"), "{path} lacks a viewBox");
        }
    }

    #[test]
    fn unknown_paths_are_none() {
        assert!(Assets.load("icons/nope.svg").unwrap().is_none());
    }

    #[test]
    fn svg_fallback_paths_serve_embedded_geist_fonts() {
        let sans = Assets
            .load("fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf")
            .unwrap()
            .unwrap();
        let mono = Assets
            .load("fonts/lilex/Lilex-Regular.ttf")
            .unwrap()
            .unwrap();
        assert_eq!(sans.as_ref(), crate::FONT_GEIST);
        assert_eq!(mono.as_ref(), crate::FONT_GEIST_MONO);
    }

    #[test]
    fn list_filters_by_prefix() {
        assert!(!Assets.list("icons/").unwrap().is_empty());
        assert_eq!(Assets.list("fonts/").unwrap().len(), 2);
    }
}
