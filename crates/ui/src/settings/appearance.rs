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

use crate::appearance::{self, AppearanceMode, FontRole};
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover;
use crate::settings::widgets;
use crate::theme::{Appearance, DEFAULT_CODE_FONT, DEFAULT_UI_FONT, Theme};
use crate::themes::{
    EditableTheme, ThemeColorGroup, ThemeColorRole, ThemeSummary, format_hex_color, parse_hex_color,
};

struct ThemeEditor {
    draft: EditableTheme,
    appearance: Appearance,
    role: ThemeColorRole,
    error: Option<SharedString>,
}

pub struct AppearancePage {
    open_font: Option<FontRole>,
    font_names: Vec<SharedString>,
    font_search: Entity<ComposerInput>,
    font_active: usize,
    font_focus: FocusHandle,
    font_scroll: gpui::ScrollHandle,
    menu_dismissed_at: Option<Instant>,
    theme_editor: Option<ThemeEditor>,
    theme_name: Entity<ComposerInput>,
    theme_color: Entity<ComposerInput>,
    _font_search_events: Subscription,
    _theme_name_events: Subscription,
    _theme_color_events: Subscription,
}

impl AppearancePage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        appearance::reload_theme_files(cx);
        let font_search =
            cx.new(|cx| ComposerInput::with_context("Search fonts…", "PaletteSearch", cx));
        let font_search_events = cx.subscribe(&font_search, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                this.font_active = 0;
                this.font_scroll.scroll_to_item(0);
                cx.notify();
            }
        });
        let theme_name = cx.new(|cx| ComposerInput::with_context("Theme name", "ThemeName", cx));
        let theme_color =
            cx.new(|cx| ComposerInput::with_context("#RRGGBB or #RRGGBBAA", "ThemeColor", cx));
        let theme_name_events = cx.subscribe(&theme_name, |this: &mut Self, input, event, cx| {
            if matches!(event, ComposerInputEvent::Edited)
                && let Some(editor) = &mut this.theme_editor
            {
                editor.draft.name = input.read(cx).text().to_string();
                editor.error = None;
                cx.notify();
            }
        });
        let theme_color_events = cx.subscribe(&theme_color, |this: &mut Self, input, event, cx| {
            if !matches!(event, ComposerInputEvent::Edited) {
                return;
            }
            let Some(editor) = &mut this.theme_editor else {
                return;
            };
            match parse_hex_color(input.read(cx).text()) {
                Ok(color) => {
                    let theme = match editor.appearance {
                        Appearance::Light => &mut editor.draft.light,
                        Appearance::Dark => &mut editor.draft.dark,
                    };
                    editor.role.set_color(theme, color);
                    editor.error = None;
                    appearance::preview_theme(theme.clone(), cx);
                }
                Err(err) => editor.error = Some(err.to_string().into()),
            }
            cx.notify();
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
            font_names,
            font_search,
            font_active: 0,
            font_focus: cx.focus_handle(),
            font_scroll: gpui::ScrollHandle::new(),
            menu_dismissed_at: None,
            theme_editor: None,
            theme_name,
            theme_color,
            _font_search_events: font_search_events,
            _theme_name_events: theme_name_events,
            _theme_color_events: theme_color_events,
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
                crate::icons::icon(crate::icons::CHEVRON_DOWN)
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

    fn open_theme_editor(&mut self, id: &str, cx: &mut Context<Self>) {
        let draft = appearance::editable_theme(id, cx);
        let appearance = Theme::of(cx).appearance;
        let role = ThemeColorRole::Background;
        let color = role.color(match appearance {
            Appearance::Light => &draft.light,
            Appearance::Dark => &draft.dark,
        });
        self.theme_editor = Some(ThemeEditor {
            draft: draft.clone(),
            appearance,
            role,
            error: None,
        });
        self.theme_name
            .update(cx, |input, cx| input.set_text(draft.name, cx));
        self.theme_color
            .update(cx, |input, cx| input.set_text(format_hex_color(color), cx));
        appearance::preview_theme(
            match appearance {
                Appearance::Light => draft.light,
                Appearance::Dark => draft.dark,
            },
            cx,
        );
        cx.notify();
    }

    fn close_theme_editor(&mut self, cx: &mut Context<Self>) {
        self.theme_editor = None;
        appearance::clear_theme_preview(cx);
        cx.notify();
    }

    pub(crate) fn dismiss_modal(&mut self, cx: &mut Context<Self>) -> bool {
        if self.theme_editor.is_none() {
            return false;
        }
        self.close_theme_editor(cx);
        true
    }

    fn set_editor_appearance(&mut self, appearance: Appearance, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.theme_editor else {
            return;
        };
        editor.appearance = appearance;
        editor.error = None;
        let theme = match appearance {
            Appearance::Light => &editor.draft.light,
            Appearance::Dark => &editor.draft.dark,
        };
        self.theme_color.update(cx, |input, cx| {
            input.set_text(format_hex_color(editor.role.color(theme)), cx)
        });
        appearance::preview_theme(theme.clone(), cx);
        cx.notify();
    }

    fn select_theme_color(&mut self, role: ThemeColorRole, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.theme_editor else {
            return;
        };
        editor.role = role;
        editor.error = None;
        let theme = match editor.appearance {
            Appearance::Light => &editor.draft.light,
            Appearance::Dark => &editor.draft.dark,
        };
        self.theme_color.update(cx, |input, cx| {
            input.set_text(format_hex_color(role.color(theme)), cx)
        });
        cx.notify();
    }

    fn save_theme_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.theme_editor else {
            return;
        };
        editor.draft.name = self.theme_name.read(cx).text().trim().to_string();
        if editor.draft.name.is_empty() {
            editor.error = Some("Enter a theme name.".into());
            cx.notify();
            return;
        }
        match appearance::save_custom_theme(&editor.draft, cx) {
            Ok(_) => {
                self.theme_editor = None;
                cx.notify();
            }
            Err(err) => {
                if let Some(editor) = &mut self.theme_editor {
                    editor.error = Some(format!("Couldn’t save theme: {err}").into());
                }
                cx.notify();
            }
        }
    }

    fn delete_theme(&mut self, id: &str, cx: &mut Context<Self>) {
        match appearance::delete_custom_theme(id, cx) {
            Ok(_) => {
                self.theme_editor = None;
                cx.notify();
            }
            Err(err) => {
                if let Some(editor) = &mut self.theme_editor {
                    editor.error = Some(format!("Couldn’t delete theme: {err}").into());
                }
                cx.notify();
            }
        }
    }

    fn theme_card(
        &mut self,
        definition: ThemeSummary,
        light_selected: bool,
        dark_selected: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let light_id = definition.id.clone();
        let dark_id = definition.id.clone();
        let pair_id = definition.id.clone();
        let edit_id = definition.id.clone();
        let selected_label = match (light_selected, dark_selected) {
            (true, true) => Some("Light + dark"),
            (true, false) => Some("Light"),
            (false, true) => Some("Dark"),
            (false, false) => None,
        };
        let preview = div()
            .h(px(132.0))
            .w_full()
            .flex()
            .flex_row()
            .overflow_hidden()
            .child(
                div()
                    .id(SharedString::from(format!("theme-light-{}", definition.id)))
                    .relative()
                    .w_1_2()
                    .h_full()
                    .rounded_tl(px(widgets::OPTION_CARD_RADIUS))
                    .rounded_bl(px(widgets::OPTION_CARD_RADIUS))
                    .border_2()
                    .border_color(if light_selected {
                        theme.accent
                    } else {
                        gpui::transparent_black()
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |_, _, _, cx| {
                        appearance::set_theme(Appearance::Light, &light_id, cx);
                        cx.notify();
                    }))
                    .child(miniature(
                        &definition.light,
                        Corners::Left,
                        widgets::OPTION_CARD_RADIUS - 2.0,
                    ))
                    .child(
                        div()
                            .absolute()
                            .top(px(8.0))
                            .left(px(8.0))
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded_full()
                            .bg(definition.light.surface_overlay)
                            .text_size(px(10.0))
                            .text_color(definition.light.text_muted)
                            .child("Light"),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(format!("theme-dark-{}", definition.id)))
                    .relative()
                    .w_1_2()
                    .h_full()
                    .rounded_tr(px(widgets::OPTION_CARD_RADIUS))
                    .rounded_br(px(widgets::OPTION_CARD_RADIUS))
                    .border_2()
                    .border_color(if dark_selected {
                        theme.accent
                    } else {
                        gpui::transparent_black()
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |_, _, _, cx| {
                        appearance::set_theme(Appearance::Dark, &dark_id, cx);
                        cx.notify();
                    }))
                    .child(miniature(
                        &definition.dark,
                        Corners::Right,
                        widgets::OPTION_CARD_RADIUS - 2.0,
                    ))
                    .child(
                        div()
                            .absolute()
                            .top(px(8.0))
                            .right(px(8.0))
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded_full()
                            .bg(definition.dark.surface_overlay)
                            .text_size(px(10.0))
                            .text_color(definition.dark.text_muted)
                            .child("Dark"),
                    ),
            );
        div()
            .w(px(335.0))
            .flex_none()
            .rounded(px(12.0))
            .border_1()
            .border_color(if light_selected || dark_selected {
                theme.border_strong
            } else {
                theme.border
            })
            .bg(theme.surface_card)
            .overflow_hidden()
            .child(preview)
            .child(
                div()
                    .p(px(10.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(definition.name)),
                    )
                    .when_some(selected_label, |row, label| {
                        row.child(widgets::badge(theme, label))
                    })
                    .when(!(light_selected && dark_selected), |row| {
                        row.child(
                            popover::btn_ghost(theme, "Use both", format!("theme-pair-{pair_id}"))
                                .id(SharedString::from(format!("theme-pair-{pair_id}")))
                                .px(px(7.0))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    appearance::set_theme_pair(&pair_id, cx);
                                    cx.notify();
                                })),
                        )
                    })
                    .child(
                        popover::btn_ghost(theme, "Customize", format!("theme-edit-{edit_id}"))
                            .id(SharedString::from(format!("theme-edit-{edit_id}")))
                            .px(px(7.0))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_theme_editor(&edit_id, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_theme_editor(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let editor = self.theme_editor.as_ref()?;
        let appearance = editor.appearance;
        let selected_role = editor.role;
        let error = editor.error.clone();
        let editing_id = editor.draft.id.clone();
        let active_theme = match appearance {
            Appearance::Light => &editor.draft.light,
            Appearance::Dark => &editor.draft.dark,
        };
        let color_rows = ThemeColorGroup::ALL.into_iter().map(|group| {
            let rows = ThemeColorRole::ALL
                .iter()
                .copied()
                .filter(move |role| role.group() == group)
                .map(|role| {
                    let selected = role == selected_role;
                    let color = role.color(active_theme);
                    div()
                        .id(SharedString::from(format!(
                            "theme-color-role-{}",
                            role.key()
                        )))
                        .px(px(10.0))
                        .py(px(7.0))
                        .rounded(px(7.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(9.0))
                        .cursor_pointer()
                        .bg(if selected {
                            crate::theme::card_selected_bg()
                        } else {
                            gpui::transparent_black()
                        })
                        .hover(|style| style.bg(crate::theme::ink(0.05)))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.select_theme_color(role, cx)),
                        )
                        .child(
                            div()
                                .size(px(18.0))
                                .rounded(px(5.0))
                                .border_1()
                                .border_color(theme.border)
                                .bg(color),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(12.0))
                                .text_color(theme.text)
                                .child(role.label()),
                        )
                        .child(
                            div()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(11.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(format_hex_color(color))),
                        )
                });
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .px(px(6.0))
                        .pt(px(10.0))
                        .pb(px(4.0))
                        .text_size(px(11.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text_faint)
                        .child(group.label()),
                )
                .children(rows)
        });
        let card = popover::dialog_card(&theme)
            .w(px(680.0))
            .child(popover::dialog_title(&theme, "Edit theme"))
            .child(div().mt(px(12.0)).child(popover::dialog_field(
                self.theme_name.clone().into_any_element(),
            )))
            .child(div().mt(px(12.0)).flex().flex_row().gap(px(6.0)).children(
                [Appearance::Light, Appearance::Dark].map(|variant| {
                    let selected = variant == appearance;
                    div()
                        .id(match variant {
                            Appearance::Light => "theme-editor-light",
                            Appearance::Dark => "theme-editor-dark",
                        })
                        .px(px(12.0))
                        .py(px(6.0))
                        .rounded(px(8.0))
                        .bg(if selected {
                            crate::theme::card_selected_bg()
                        } else {
                            gpui::transparent_black()
                        })
                        .text_size(px(12.0))
                        .text_color(if selected {
                            theme.text
                        } else {
                            theme.text_muted
                        })
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_editor_appearance(variant, cx)
                        }))
                        .child(match variant {
                            Appearance::Light => "Light",
                            Appearance::Dark => "Dark",
                        })
                }),
            ))
            .child(
                div()
                    .mt(px(12.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .size(px(26.0))
                            .rounded(px(7.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(selected_role.color(active_theme)),
                    )
                    .child(div().w(px(220.0)).child(popover::dialog_field(
                        self.theme_color.clone().into_any_element(),
                    )))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(selected_role.label()),
                    ),
            )
            .child(
                div()
                    .id("theme-editor-color-list")
                    .mt(px(10.0))
                    .max_h(px(330.0))
                    .overflow_y_scroll()
                    .pr(px(4.0))
                    .children(color_rows),
            )
            .when_some(error, |card, error| {
                card.child(
                    div()
                        .mt(px(8.0))
                        .text_size(px(12.0))
                        .text_color(theme.danger)
                        .child(error),
                )
            })
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .justify_between()
                    .child(div().when_some(editing_id, |row, id| {
                        row.child(
                            popover::btn_danger(&theme, "Delete")
                                .id("theme-editor-delete")
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.delete_theme(&id, cx)),
                                ),
                        )
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .child(
                                popover::btn_ghost(&theme, "Cancel", "theme-editor-cancel")
                                    .id("theme-editor-cancel")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.close_theme_editor(cx)),
                                    ),
                            )
                            .child(
                                popover::btn_primary(&theme, "Save")
                                    .id("theme-editor-save")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.save_theme_editor(cx)),
                                    ),
                            ),
                    ),
            )
            .into_any_element();
        Some(popover::modal("theme-editor-dialog", viewport, card))
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
/// inset — so rounding here is enough. `radius` lets bordered containers match
/// the miniature to their inner curve.
fn miniature(theme: &Theme, corners: Corners, radius: f32) -> AnyElement {
    let line = theme.text.opacity(0.22);
    let strong = theme.text.opacity(0.34);
    let r = px(radius);
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
fn miniature_split(light: &Theme, dark: &Theme) -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_row()
        .child(div().w_1_2().h_full().overflow_hidden().child(miniature(
            light,
            Corners::Left,
            widgets::OPTION_CARD_RADIUS,
        )))
        .child(div().w_1_2().h_full().overflow_hidden().child(miniature(
            dark,
            Corners::Right,
            widgets::OPTION_CARD_RADIUS,
        )))
        .into_any_element()
}

/// The preview graphic for a mode.
///
/// The one place `Theme::light()`/`Theme::dark()` are legitimately built outside
/// the installed global: a preview has to show the palette you are *not* using.
fn preview(mode: AppearanceMode, light: &Theme, dark: &Theme) -> AnyElement {
    match mode {
        AppearanceMode::System => miniature_split(light, dark),
        AppearanceMode::Light => miniature(light, Corners::All, widgets::OPTION_CARD_RADIUS),
        AppearanceMode::Dark => miniature(dark, Corners::All, widgets::OPTION_CARD_RADIUS),
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
                "Following the system appearance — currently {resolved}. Jolt also follows scheduled changes."
            )
            .into()
        }
        AppearanceMode::Light => "Always light, whatever the system is set to.".into(),
        AppearanceMode::Dark => "Always dark, whatever the system is set to.".into(),
    }
}

impl Render for AppearancePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let current = appearance::mode(cx);
        let (light_theme_id, dark_theme_id) = appearance::theme_ids(cx);
        let selected_light = appearance::theme_for(&light_theme_id, Appearance::Light, cx);
        let selected_dark = appearance::theme_for(&dark_theme_id, Appearance::Dark, cx);
        let system = cx
            .try_global::<appearance::AppearanceState>()
            .map(|state| state.system)
            .unwrap_or_default();
        let (ui_font, prompt_font, code_font, terminal_font) = appearance::font_families(cx);
        let font_sizes = theme.font_sizes;
        let ui_picker = self.font_picker(FontRole::Ui, ui_font.clone(), &theme, cx);
        let prompt_picker = self.font_picker(FontRole::Prompt, prompt_font.clone(), &theme, cx);
        let code_picker = self.font_picker(FontRole::Code, code_font.clone(), &theme, cx);
        let terminal_picker =
            self.font_picker(FontRole::Terminal, terminal_font.clone(), &theme, cx);

        let mut cards = Vec::new();
        for mode in AppearanceMode::ALL {
            cards.push(
                widgets::option_card(
                    &theme,
                    mode.label(),
                    mode == current,
                    preview(mode, &selected_light, &selected_dark),
                )
                .id(SharedString::from(format!("appearance-{}", mode.label())))
                .on_click(cx.listener(move |_, _, _, cx| {
                    appearance::set_mode(mode, cx);
                    cx.notify();
                })),
            );
        }
        let mut theme_cards = Vec::new();
        for definition in appearance::themes(cx) {
            let light_selected = definition.id == light_theme_id;
            let dark_selected = definition.id == dark_theme_id;
            theme_cards.push(self.theme_card(
                definition,
                light_selected,
                dark_selected,
                &theme,
                cx,
            ));
        }
        let editor = self.render_theme_editor(window.viewport_size(), cx);

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
                            "Choose how Jolt looks on this device. Appearance, themes, and fonts apply immediately.",
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
                            .child(widgets::field_label(&theme, "Appearance mode"))
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
                            .mt(px(32.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(widgets::field_label(&theme, "Theme"))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .line_height(px(18.0))
                                    .text_color(theme.text_muted)
                                    .child(
                                        "Select the light or dark half of a preview, or apply a family to both modes.",
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .gap(px(14.0))
                                    .children(theme_cards),
                            ),
                    )
                    .child(
                        div()
                            .mt(px(36.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(widgets::field_label(&theme, "Fonts"))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .line_height(px(18.0))
                                    .text_color(theme.text_muted)
                                    .child("Choose separate typefaces for the interface, composer, code, and terminal."),
                            )
                            .child(
                                widgets::section_card(&theme)
                                    .mt(px(0.0))
                                    .child(
                                        widgets::card_row(&theme, true)
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .child(widgets::row_title(&theme, "Interface"))
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
                                            .child(div().flex_none().child(ui_picker)),
                                    )
                                    .child(
                                        widgets::card_row(&theme, false)
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .child(widgets::row_title(
                                                        &theme,
                                                        "Composer",
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
                                            .child(div().flex_none().child(prompt_picker)),
                                    )
                                    .child(
                                        widgets::card_row(&theme, false)
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .child(widgets::row_title(&theme, "Code"))
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
                                            .child(div().flex_none().child(code_picker)),
                                    )
                                    .child(
                                        widgets::card_row(&theme, false)
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .child(widgets::row_title(
                                                        &theme,
                                                        "Terminal",
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
                                            .child(div().flex_none().child(terminal_picker)),
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
            .children(editor)
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
