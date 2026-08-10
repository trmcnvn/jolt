//! Settings → Notifications: device-local alert preferences.

use gpui::{Context, EventEmitter, Render, SharedString, Stateful, div, prelude::*, px};

use crate::settings::widgets;
use crate::theme::Theme;

#[derive(Debug, Clone, Copy)]
pub enum NotificationsEvent {
    SystemNotificationsEnabledChanged(bool),
}

pub struct NotificationsPage {
    system_notifications_enabled: bool,
}

impl EventEmitter<NotificationsEvent> for NotificationsPage {}

impl NotificationsPage {
    pub fn new(system_notifications_enabled: bool) -> Self {
        Self {
            system_notifications_enabled,
        }
    }
}

fn toggle(theme: &Theme, id: &'static str, enabled: bool) -> Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .w(px(32.0))
        .h(px(18.0))
        .rounded_full()
        .bg(if enabled {
            theme.text
        } else {
            crate::theme::ink(0.15)
        })
        .relative()
        .cursor_pointer()
        .child(
            div()
                .absolute()
                .top(px(2.0))
                .left(px(if enabled { 16.0 } else { 2.0 }))
                .size(px(14.0))
                .rounded_full()
                .bg(if enabled {
                    theme.on_solid
                } else {
                    crate::theme::ink(0.7)
                }),
        )
}

impl Render for NotificationsPage {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let system_notifications_toggle = toggle(
            &theme,
            "system-notifications-toggle",
            self.system_notifications_enabled,
        )
        .on_click(cx.listener(|page, _, _, cx| {
            page.system_notifications_enabled = !page.system_notifications_enabled;
            cx.emit(NotificationsEvent::SystemNotificationsEnabledChanged(
                page.system_notifications_enabled,
            ));
            cx.notify();
        }));
        div()
            .id("notifications-settings-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Notifications", None))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Choose how Jolt delivers app-wide alerts."
                    ))
                    .child(
                        widgets::section_card(&theme)
                            .child(
                                widgets::card_row(&theme, true)
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .child(widgets::row_title(
                                                &theme,
                                                "System notifications",
                                            ))
                                            .child(
                                                div()
                                                    .mt(px(3.0))
                                                    .text_size(px(11.5))
                                                    .text_color(theme.text_muted.opacity(0.7))
                                                    .child(SharedString::from(
                                                        "Deliver Claude and Codex usage warnings, Jolt updates, and app-wide errors through your operating system instead of inside Jolt.",
                                                    )),
                                            ),
                                    )
                                    .child(system_notifications_toggle),
                            ),
                    ),
            )
    }
}
