//! Settings → Appearance: theme and typography preferences.
//!
//! Uses [`widgets::option_card_row`] — a preview-card picker, because the choice
//! is a *look*, and a miniature of the result says more than a sentence about it.
//! The control itself is theme-agnostic; only the previews below know what a
//! theme is.
//!
//! Choices live in the [`crate::appearance`] global and repaint every window.
//! This page only holds ephemeral typography-menu state and the installed-font list.

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable as _, Hsla, IntoElement, KeyDownEvent,
    Render, SharedString, Subscription, Window, div, prelude::*, px,
};

use crate::appearance::{self, AppearanceMode, FontRole, FontSizeRole};
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover;
use crate::settings::widgets;
use crate::theme::{Appearance, DEFAULT_CODE_FONT, DEFAULT_UI_FONT, Theme};

pub struct AppearancePage {
    open_font: Option<FontRole>,
    open_size: Option<FontSizeRole>,
    font_names: Vec<SharedString>,
    font_search: Entity<ComposerInput>,
    font_active: usize,
    font_focus: FocusHandle,
    font_scroll: gpui::ScrollHandle,
    menu_dismissed_at: Option<Instant>,
    size_menu_dismissed_at: Option<Instant>,
    _font_search_events: Subscription,
}

impl AppearancePage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let font_search =
            cx.new(|cx| ComposerInput::with_context("Search fonts…", "PaletteSearch", cx));
        let font_search_events = cx.subscribe(&font_search, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                this.font_active = 0;
                this.font_scroll.scroll_to_item(0);
                cx.notify();
            }
        });
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
            open_size: None,
            font_names,
            font_search,
            font_active: 0,
            font_focus: cx.focus_handle(),
            font_scroll: gpui::ScrollHandle::new(),
            menu_dismissed_at: None,
            size_menu_dismissed_at: None,
            _font_search_events: font_search_events,
        }
    }

    fn filtered_font_indices(&self, cx: &App) -> Vec<usize> {
        popover::filter_indices(self.font_search.read(cx).text(), &self.font_names)
    }

    fn select_font(&mut self, role: FontRole, font: SharedString, cx: &mut Context<Self>) {
        appearance::set_font(role, font.as_ref(), cx);
        self.open_font = None;
        cx.notify();
    }

    fn font_picker_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(role) = self.open_font else {
            return;
        };
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            popover::MenuKey::Escape => {
                self.open_font = None;
                cx.notify();
                cx.stop_propagation();
            }
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let count = self.filtered_font_indices(cx).len();
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                self.font_active =
                    popover::menu_step(Some(self.font_active), count, delta).unwrap_or(0);
                self.font_scroll.scroll_to_item(self.font_active);
                cx.notify();
                cx.stop_propagation();
            }
            popover::MenuKey::Enter | popover::MenuKey::ModEnter => {
                let font = self
                    .filtered_font_indices(cx)
                    .get(self.font_active)
                    .map(|&index| self.font_names[index].clone());
                if let Some(font) = font {
                    self.select_font(role, font, cx);
                }
                cx.stop_propagation();
            }
            popover::MenuKey::Backspace | popover::MenuKey::Other => {}
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
        let selected_font = current.clone();
        let mut trigger = div()
            .id(match role {
                FontRole::Ui => "ui-font-picker",
                FontRole::Prompt => "prompt-font-picker",
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
            .on_click(cx.listener(move |this, _, window, cx| {
                let just_dismissed = this
                    .menu_dismissed_at
                    .is_some_and(|at| at.elapsed() < Duration::from_millis(300));
                if this.open_font == Some(role) || just_dismissed {
                    this.open_font = None;
                } else {
                    this.open_font = Some(role);
                    this.open_size = None;
                    this.font_search
                        .update(cx, |input, cx| input.set_text("", cx));
                    this.font_active = this
                        .font_names
                        .iter()
                        .position(|font| font == &selected_font)
                        .unwrap_or(0);
                    this.font_scroll.scroll_to_item(this.font_active);
                    let focus = this.font_search.read(cx).focus_handle(cx);
                    window.focus(&focus, cx);
                }
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
            let active = self.font_active;
            let fonts: Vec<(usize, SharedString)> = self
                .filtered_font_indices(cx)
                .into_iter()
                .map(|index| (index, self.font_names[index].clone()))
                .collect();
            let list: AnyElement = if fonts.is_empty() {
                div()
                    .p(px(Theme::SPACE_SM))
                    .text_size(px(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from("No fonts found."))
                    .into_any_element()
            } else {
                div()
                    .id(match role {
                        FontRole::Ui => "ui-font-scroll",
                        FontRole::Prompt => "prompt-font-scroll",
                        FontRole::Code => "code-font-scroll",
                        FontRole::Terminal => "terminal-font-scroll",
                    })
                    .max_h(px(300.0))
                    .overflow_y_scroll()
                    .track_scroll(&self.font_scroll)
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .children(fonts.into_iter().enumerate().map(
                        |(row_index, (font_index, font))| {
                            let is_selected = font == selected;
                            let pick = font.clone();
                            popover::menu_row_nav(
                                theme,
                                is_selected,
                                row_index == active,
                                format!("font-row-{role:?}-{font_index}"),
                            )
                            .id((
                                match role {
                                    FontRole::Ui => "ui-font",
                                    FontRole::Prompt => "prompt-font",
                                    FontRole::Code => "code-font",
                                    FontRole::Terminal => "terminal-font",
                                },
                                font_index,
                            ))
                            .font_family(font.clone())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_font(role, pick.clone(), cx);
                            }))
                            .child(div().flex_1().min_w_0().truncate().child(font))
                        },
                    ))
                    .into_any_element()
            };
            let menu =
                popover::popover_card(theme)
                    .w(px(300.0))
                    .track_focus(&self.font_focus)
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        this.font_picker_key(event, cx)
                    }))
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.open_font = None;
                        this.menu_dismissed_at = Some(Instant::now());
                        cx.notify();
                    }))
                    .flex()
                    .flex_col()
                    .child(popover::search_input_frame(
                        theme,
                        self.font_search.clone().into_any_element(),
                    ))
                    .child(list)
                    .into_any_element();
            trigger = trigger.child(popover::anchored_menu("font-family-menu", menu));
        }
        trigger.into_any_element()
    }

    fn size_picker(
        &mut self,
        role: FontSizeRole,
        current: u8,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.open_size == Some(role);
        let mut trigger = div()
            .id(match role {
                FontSizeRole::Interface => "interface-font-size-picker",
                FontSizeRole::Prompt => "prompt-font-size-picker",
                FontSizeRole::Code => "code-font-size-picker",
                FontSizeRole::Terminal => "terminal-font-size-picker",
            })
            .relative()
            .w(px(88.0))
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
                    .size_menu_dismissed_at
                    .is_some_and(|at| at.elapsed() < Duration::from_millis(300));
                if this.open_size == Some(role) || just_dismissed {
                    this.open_size = None;
                } else {
                    this.open_size = Some(role);
                    this.open_font = None;
                }
                this.size_menu_dismissed_at = None;
                cx.notify();
            }))
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.0))
                    .text_color(theme.text)
                    .child(SharedString::from(format!("{current} px"))),
            )
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            );

        if open {
            let rows = role.range().map(|size| {
                popover::menu_row(
                    theme,
                    size == current,
                    format!("font-size-row-{role:?}-{size}"),
                )
                .id(("font-size", size as usize))
                .on_click(cx.listener(move |this, _, _, cx| {
                    appearance::set_font_size(role, size, cx);
                    this.open_size = None;
                    cx.notify();
                }))
                .child(SharedString::from(format!("{size} px")))
            });
            let menu = popover::popover_card(theme)
                .id("font-size-options")
                .w(px(96.0))
                .max_h(px(320.0))
                .overflow_y_scroll()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.open_size = None;
                    this.size_menu_dismissed_at = Some(Instant::now());
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .children(rows)
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu("font-size-menu", menu));
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
        let (ui_font, prompt_font, code_font, terminal_font) = appearance::font_families(cx);
        let font_sizes = appearance::font_sizes(cx);
        let ui_picker = self.font_picker(FontRole::Ui, ui_font.clone(), &theme, cx);
        let ui_size_picker =
            self.size_picker(FontSizeRole::Interface, font_sizes.interface, &theme, cx);
        let prompt_picker = self.font_picker(FontRole::Prompt, prompt_font.clone(), &theme, cx);
        let prompt_size_picker =
            self.size_picker(FontSizeRole::Prompt, font_sizes.prompt, &theme, cx);
        let code_picker = self.font_picker(FontRole::Code, code_font.clone(), &theme, cx);
        let code_size_picker = self.size_picker(FontSizeRole::Code, font_sizes.code, &theme, cx);
        let terminal_picker =
            self.font_picker(FontRole::Terminal, terminal_font.clone(), &theme, cx);
        let terminal_size_picker =
            self.size_picker(FontSizeRole::Terminal, font_sizes.terminal, &theme, cx);

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
                                                            .font_family(ui_font.clone())
                                                            .text_size(px(f32::from(
                                                                font_sizes.interface,
                                                            )))
                                                            .text_color(theme.text_muted)
                                                            .child(SharedString::from(
                                                                "The quick brown fox jumps over the lazy dog.",
                                                            )),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .flex()
                                                    .flex_row()
                                                    .gap(px(8.0))
                                                    .child(ui_picker)
                                                    .child(ui_size_picker),
                                            ),
                                    )
                                    .child(
                                        widgets::card_row(&theme, false)
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .child(widgets::row_title(
                                                        &theme,
                                                        "Prompt font",
                                                    ))
                                                    .child(
                                                        div()
                                                            .mt(px(3.0))
                                                            .font_family(prompt_font)
                                                            .text_size(px(f32::from(
                                                                font_sizes.prompt,
                                                            )))
                                                            .text_color(theme.text_muted)
                                                            .child(SharedString::from(
                                                                "Only the box you write prompts in.",
                                                            )),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .flex()
                                                    .flex_row()
                                                    .gap(px(8.0))
                                                    .child(prompt_picker)
                                                    .child(prompt_size_picker),
                                            ),
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
                                                            .text_size(px(f32::from(
                                                                font_sizes.code,
                                                            )))
                                                            .text_color(theme.text_muted)
                                                            .child(SharedString::from(
                                                                "fn main() { println!(\"Hello\"); }",
                                                            )),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .flex()
                                                    .flex_row()
                                                    .gap(px(8.0))
                                                    .child(code_picker)
                                                    .child(code_size_picker),
                                            ),
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
                                                            .text_size(px(f32::from(
                                                                font_sizes.terminal,
                                                            )))
                                                            .text_color(theme.text_muted)
                                                            .child(SharedString::from(
                                                                "$ cargo test --workspace",
                                                            )),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .flex()
                                                    .flex_row()
                                                    .gap(px(8.0))
                                                    .child(terminal_picker)
                                                    .child(terminal_size_picker),
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .px(px(4.0))
                                    .text_size(px(12.0))
                                    .line_height(px(18.0))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(
                                        "Code font applies to code blocks, diffs, and hotkey chips.",
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
