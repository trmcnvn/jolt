//! Settings → Appearance: theme and typography preferences.
//!
//! Uses [`widgets::option_card_row`] — a preview-card picker, because the choice
//! is a *look*, and a miniature of the result says more than a sentence about it.
//! The control itself is theme-agnostic; only the previews below know what a
//! theme is.
//!
//! Choices live in the [`crate::appearance`] global and repaint every window.
//! This page only holds ephemeral font-menu state and the installed-font list.

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Context, Hsla, IntoElement, Render, SharedString, Window, div, prelude::*, px,
};

use crate::appearance::{self, AppearanceMode, FontRole};
use crate::popover;
use crate::settings::widgets;
use crate::theme::{Appearance, DEFAULT_CODE_FONT, DEFAULT_UI_FONT, Theme};

pub struct AppearancePage {
    open_font: Option<FontRole>,
    font_names: Vec<SharedString>,
    menu_dismissed_at: Option<Instant>,
}

impl AppearancePage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut font_names: Vec<SharedString> = cx
            .text_system()
            .all_font_names()
            .into_iter()
            .filter(|name| !name.starts_with('.'))
            .map(SharedString::from)
            .collect();
        for bundled in [DEFAULT_UI_FONT, DEFAULT_CODE_FONT] {
            if !font_names.iter().any(|name| name.as_ref() == bundled) {
                font_names.push(bundled.into());
            }
        }
        font_names.sort_by(|a, b| {
            let rank = |name: &str| match name {
                DEFAULT_UI_FONT => 0,
                DEFAULT_CODE_FONT => 1,
                _ => 2,
            };
            rank(a.as_ref())
                .cmp(&rank(b.as_ref()))
                .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
        });
        font_names.dedup();
        Self {
            open_font: None,
            font_names,
            menu_dismissed_at: None,
        }
    }

    fn font_picker(
        &mut self,
        role: FontRole,
        current: SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.open_font == Some(role);
        let trigger_font = current.clone();
        let mut trigger = div()
            .id(match role {
                FontRole::Ui => "ui-font-picker",
                FontRole::Code => "code-font-picker",
                FontRole::Terminal => "terminal-font-picker",
            })
            .relative()
            .w(px(260.0))
            .h(px(36.0))
            .px(px(10.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(if open {
                theme.border_strong
            } else {
                theme.border
            })
            .bg(theme.bg)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .when(!open, |el| el.hover(|s| s.bg(crate::theme::ink(0.03))))
            .on_click(cx.listener(move |this, _, _, cx| {
                let just_dismissed = this
                    .menu_dismissed_at
                    .is_some_and(|at| at.elapsed() < Duration::from_millis(300));
                this.open_font = if this.open_font == Some(role) || just_dismissed {
                    None
                } else {
                    Some(role)
                };
                this.menu_dismissed_at = None;
                cx.notify();
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(trigger_font)
                    .text_size(px(13.0))
                    .text_color(theme.text)
                    .child(current.clone()),
            )
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            );

        if open {
            let selected = current.clone();
            let rows = self
                .font_names
                .clone()
                .into_iter()
                .enumerate()
                .map(|(ix, font)| {
                    let active = font == selected;
                    let pick = font.clone();
                    popover::menu_row(theme, active, format!("font-row-{role:?}-{ix}"))
                        .id((
                            match role {
                                FontRole::Ui => "ui-font",
                                FontRole::Code => "code-font",
                                FontRole::Terminal => "terminal-font",
                            },
                            ix,
                        ))
                        .font_family(font.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            appearance::set_font(role, pick.as_ref(), cx);
                            this.open_font = None;
                            cx.notify();
                        }))
                        .child(div().flex_1().min_w_0().truncate().child(font))
                        .when(active, |el| el.child(popover::menu_check(theme)))
                });
            let menu = popover::popover_card(theme)
                .w(px(300.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.open_font = None;
                    this.menu_dismissed_at = Some(Instant::now());
                    cx.notify();
                }))
                .child(popover::menu_heading(theme, "Installed fonts"))
                .child(
                    div()
                        .id(match role {
                            FontRole::Ui => "ui-font-scroll",
                            FontRole::Code => "code-font-scroll",
                            FontRole::Terminal => "terminal-font-scroll",
                        })
                        .max_h(px(300.0))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .children(rows),
                )
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu("font-family-menu", menu));
        }
        trigger.into_any_element()
    }
}

/// One placeholder bar in the miniature, width given as a fraction of its
/// container.
///
/// Relative rather than fixed px because the System card renders this same
/// miniature into *half* a card. Fixed widths were wider than the squeezed
/// content pane and spilled out over the card edge.
fn bar(fraction: f32, tone: Hsla) -> gpui::Div {
    div()
        .h(px(5.0))
        .w(gpui::relative(fraction))
        .rounded(px(3.0))
        .bg(tone)
}

/// Which corners a miniature rounds — the split card needs each half to round
/// only its outer side so the two meet flush down the middle.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Corners {
    All,
    Left,
    Right,
}

/// A miniature of the app in `theme`: sidebar strip, inset content card, a few
/// placeholder lines. Built from the theme's own tokens rather than fixed
/// swatches, so the previews stay honest if the palette is retuned.
///
/// Rounds itself: the card frame cannot do it for us (see
/// [`widgets::OPTION_CARD_RADIUS`]). Only this root paints a background that
/// reaches the corners — the sidebar strip is transparent and the content card is
/// inset — so rounding here is enough.
fn miniature(theme: &Theme, corners: Corners) -> AnyElement {
    let line = theme.text.opacity(0.22);
    let strong = theme.text.opacity(0.34);
    let r = px(widgets::OPTION_CARD_RADIUS);
    let root = div().size_full().flex().flex_row().bg(theme.surface);
    let root = match corners {
        Corners::All => root.rounded(r),
        Corners::Left => root.rounded_tl(r).rounded_bl(r),
        Corners::Right => root.rounded_tr(r).rounded_br(r),
    };
    root.child(
        // Sidebar strip.
        div()
            .w(px(44.0))
            .h_full()
            .flex_none()
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .px(px(8.0))
            .pt(px(14.0))
            .child(bar(0.70, strong))
            .child(bar(1.0, line))
            .child(bar(0.85, line))
            .child(bar(1.0, line)),
    )
    .child(
        // Inset content card — the same rounded plate the real shell floats.
        div()
            .flex_1()
            .min_w_0()
            .my(px(8.0))
            .mr(px(8.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.bg)
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .p(px(10.0))
            .child(bar(0.62, strong))
            .child(bar(0.88, line))
            .child(bar(0.76, line))
            .child(bar(0.52, line)),
    )
    .into_any_element()
}

/// The System card: light on the left, dark on the right. Each half is a
/// complete miniature clipped to its side, which is what makes the card read as
/// "whichever one the system is on".
fn miniature_split() -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_row()
        .child(
            div()
                .w_1_2()
                .h_full()
                .overflow_hidden()
                .child(miniature(&Theme::light(), Corners::Left)),
        )
        .child(
            div()
                .w_1_2()
                .h_full()
                .overflow_hidden()
                .child(miniature(&Theme::dark(), Corners::Right)),
        )
        .into_any_element()
}

/// The preview graphic for a mode.
///
/// The one place `Theme::light()`/`Theme::dark()` are legitimately built outside
/// the installed global: a preview has to show the palette you are *not* using.
fn preview(mode: AppearanceMode) -> AnyElement {
    match mode {
        AppearanceMode::System => miniature_split(),
        AppearanceMode::Light => miniature(&Theme::light(), Corners::All),
        AppearanceMode::Dark => miniature(&Theme::dark(), Corners::All),
    }
}

/// Helper copy under the picker.
fn helper(mode: AppearanceMode, system: Appearance) -> SharedString {
    match mode {
        // Naming the resolved appearance makes "System" concrete — otherwise the
        // card says nothing about what you actually get right now.
        AppearanceMode::System => {
            let resolved = if system.is_dark() { "dark" } else { "light" };
            format!(
                "Following the system appearance — currently {resolved}. Jolt switches with \
                 macOS, including scheduled changes."
            )
            .into()
        }
        AppearanceMode::Light => "Always light, whatever the system is set to.".into(),
        AppearanceMode::Dark => "Always dark, whatever the system is set to.".into(),
    }
}

impl Render for AppearancePage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let current = appearance::mode(cx);
        let system = cx
            .try_global::<appearance::AppearanceState>()
            .map(|state| state.system)
            .unwrap_or_default();
        let (ui_font, code_font, terminal_font) = appearance::font_families(cx);
        let ui_picker = self.font_picker(FontRole::Ui, ui_font.clone(), &theme, cx);
        let code_picker = self.font_picker(FontRole::Code, code_font.clone(), &theme, cx);
        let terminal_picker =
            self.font_picker(FontRole::Terminal, terminal_font.clone(), &theme, cx);

        let cards = AppearanceMode::ALL.into_iter().map(|mode| {
            widgets::option_card(&theme, mode.label(), mode == current, preview(mode))
                .id(SharedString::from(format!("appearance-{}", mode.label())))
                .on_click(cx.listener(move |_, _, _, cx| {
                    appearance::set_mode(mode, cx);
                    cx.notify();
                }))
        });

        div()
            .id("appearance-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Appearance", None))
                    .child(
                        widgets::page_subtitle(
                            &theme,
                            "Choose Jolt’s colors and typography. These settings stay on this \
                             device.",
                        )
                        .max_w(px(512.0))
                        .line_height(px(20.0)),
                    )
                    .child(
                        div()
                            .mt(px(32.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(widgets::field_label(&theme, "Theme"))
                            .child(widgets::option_card_row().children(cards)),
                    )
                    .child(
                        div()
                            .mt(px(16.0))
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .line_height(px(18.0))
                            .child(helper(current, system)),
                    )
                    .child(
                        div()
                            .mt(px(36.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(widgets::field_label(&theme, "Typography"))
                            .child(
                                widgets::section_card(&theme)
                                    .mt(px(0.0))
                                    .child(
                                        widgets::card_row(&theme, true)
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .child(widgets::row_title(&theme, "UI font"))
                                                    .child(
                                                        div()
                                                            .mt(px(3.0))
                                                            .font_family(ui_font)
                                                            .text_size(px(12.0))
                                                            .text_color(theme.text_muted)
                                                            .child(SharedString::from(
                                                                "The quick brown fox jumps over the lazy dog.",
                                                            )),
                                                    ),
                                            )
                                            .child(ui_picker),
                                    )
                                    .child(
                                        widgets::card_row(&theme, false)
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .child(widgets::row_title(&theme, "Code font"))
                                                    .child(
                                                        div()
                                                            .mt(px(3.0))
                                                            .font_family(code_font)
                                                            .text_size(px(12.0))
                                                            .text_color(theme.text_muted)
                                                            .child(SharedString::from(
                                                                "fn main() { println!(\"Hello\"); }",
                                                            )),
                                                    ),
                                            )
                                            .child(code_picker),
                                    )
                                    .child(
                                        widgets::card_row(&theme, false)
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .child(widgets::row_title(
                                                        &theme,
                                                        "Terminal font",
                                                    ))
                                                    .child(
                                                        div()
                                                            .mt(px(3.0))
                                                            .font_family(terminal_font)
                                                            .text_size(px(12.0))
                                                            .text_color(theme.text_muted)
                                                            .child(SharedString::from(
                                                                "$ cargo test --workspace",
                                                            )),
                                                    ),
                                            )
                                            .child(terminal_picker),
                                    ),
                            )
                            .child(
                                div()
                                    .px(px(4.0))
                                    .text_size(px(12.0))
                                    .line_height(px(18.0))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(
                                        "Code font applies to code blocks, diffs, and shortcut chips.",
                                    )),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_gets_a_card() {
        assert_eq!(AppearanceMode::ALL.len(), 3);
        for mode in AppearanceMode::ALL {
            assert!(!mode.label().is_empty());
        }
    }

    #[test]
    fn system_helper_names_the_resolved_appearance() {
        let dark = helper(AppearanceMode::System, Appearance::Dark);
        let light = helper(AppearanceMode::System, Appearance::Light);
        assert!(dark.contains("currently dark"), "got {dark}");
        assert!(light.contains("currently light"), "got {light}");
    }

    /// The pinned modes must not claim to follow anything — that copy is the only
    /// thing telling the user the system setting is being ignored.
    #[test]
    fn pinned_helpers_do_not_mention_following() {
        for mode in [AppearanceMode::Light, AppearanceMode::Dark] {
            for system in [Appearance::Light, Appearance::Dark] {
                let copy = helper(mode, system).to_lowercase();
                assert!(!copy.contains("following"), "{mode:?}: {copy}");
                assert!(copy.contains("whatever the system"), "{mode:?}: {copy}");
            }
        }
    }

    /// The previews must differ from each other, or the picker is decoration.
    /// Comparing the tones they are built from is the closest we can get without
    /// a renderer.
    #[test]
    fn light_and_dark_previews_draw_from_different_palettes() {
        let (l, d) = (Theme::light(), Theme::dark());
        assert_ne!(l.surface.l, d.surface.l);
        assert_ne!(l.bg.l, d.bg.l);
    }
}
