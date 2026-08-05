//! Settings → Terminal: defaults applied when a new terminal tab opens.

use gpui::{
    Context, Entity, EventEmitter, Render, SharedString, Subscription, div, prelude::*, px,
};

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::settings::widgets;
use crate::theme::Theme;

#[derive(Debug, Clone)]
pub enum TerminalSettingsEvent {
    Changed(String),
}

pub struct TerminalPage {
    command: Entity<ComposerInput>,
    _events: Subscription,
}

impl EventEmitter<TerminalSettingsEvent> for TerminalPage {}

impl TerminalPage {
    pub fn new(command: String, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| ComposerInput::new("Default login shell", cx));
        input.update(cx, |input, cx| input.set_text(command, cx));
        let event_input = input.clone();
        let events = cx.subscribe(&input, move |_, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                cx.emit(TerminalSettingsEvent::Changed(
                    event_input.read(cx).text().to_string(),
                ));
            }
        });
        Self {
            command: input,
            _events: events,
        }
    }

    fn use_default(&mut self, cx: &mut Context<Self>) {
        self.command
            .update(cx, |input, cx| input.set_text(String::new(), cx));
    }
}

impl Render for TerminalPage {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let custom = !self.command.read(cx).text().trim().is_empty();

        div()
            .id("terminal-settings-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Terminal", None))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Customize how new terminal tabs start. Changes apply to terminals opened after this setting is changed.",
                    ))
                    .child(
                        widgets::section_card(&theme).child(
                            widgets::card_row(&theme, true)
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(widgets::row_title(&theme, "Launch command"))
                                        .child(
                                            div()
                                                .mt(px(3.0))
                                                .text_size(px(11.5))
                                                .text_color(theme.text_muted.opacity(0.7))
                                                .child(SharedString::from(
                                                    "Runs in the session directory through your login shell.",
                                                )),
                                        ),
                                )
                                .child(
                                    div()
                                        .w(px(340.0))
                                        .h(px(36.0))
                                        .px(px(10.0))
                                        .py(px(8.0))
                                        .rounded(px(8.0))
                                        .border_1()
                                        .border_color(theme.border)
                                        .bg(theme.bg)
                                        .overflow_hidden()
                                        .child(self.command.clone()),
                                ),
                        ),
                    )
                    .child(
                        div()
                            .mt(px(10.0))
                            .px(px(4.0))
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(16.0))
                            .text_size(px(12.0))
                            .line_height(px(18.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(
                                "Leave this blank to open the default interactive login shell.",
                            ))
                            .when(custom, |row| {
                                row.child(
                                    widgets::ghost_action(&theme)
                                        .id("terminal-command-default")
                                        .hover(|style| widgets::ghost_hover(&theme, style))
                                        .on_click(cx.listener(|page, _, _, cx| {
                                            page.use_default(cx);
                                        }))
                                        .child(SharedString::from("Use default")),
                                )
                            }),
                    ),
            )
    }
}
