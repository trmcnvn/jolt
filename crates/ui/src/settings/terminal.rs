//! Settings → Terminal: one launch command per engine device.

use std::time::Duration;

use gpui::{Context, Entity, Render, SharedString, Subscription, Task, div, prelude::*, px};
use jolt_api::{SetTerminalCommand, TerminalSettings, call as call_api};
use jolt_proto::TerminalSettingsSnapshot;

use super::device_switcher::{DeviceSelected, DeviceSwitcher};
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::Loadable;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

const SAVE_DEBOUNCE_MS: u64 = 400;

pub struct TerminalPage {
    state: Entity<AppState>,
    /// `None` addresses the connected engine; remote ids relay-forward.
    target_device: Option<String>,
    device_switcher: Entity<DeviceSwitcher>,
    command: Entity<ComposerInput>,
    snapshot: Loadable<TerminalSettingsSnapshot>,
    error: Option<SharedString>,
    setting_input: bool,
    load_task: Option<Task<()>>,
    save_task: Option<Task<()>>,
    _observe: Subscription,
    _events: Subscription,
    _device_switcher_events: Subscription,
}

impl TerminalPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let command = cx.new(|cx| ComposerInput::new("Default login shell", cx));
        let events = cx.subscribe(&command, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) && !this.setting_input {
                this.schedule_save(cx);
            }
        });
        let device_switcher = cx.new(|cx| DeviceSwitcher::new(state.clone(), cx));
        let device_switcher_events = cx.subscribe(
            &device_switcher,
            |this: &mut Self, _, DeviceSelected(target), cx| {
                this.select_device(target.clone(), cx);
            },
        );
        let mut page = Self {
            state,
            target_device: None,
            device_switcher,
            command,
            snapshot: Loadable::Idle,
            error: None,
            setting_input: false,
            load_task: None,
            save_task: None,
            _observe: observe,
            _events: events,
            _device_switcher_events: device_switcher_events,
        };
        page.load(cx);
        page
    }

    fn select_device(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        if self.target_device == target {
            return;
        }
        self.flush_save(cx);
        self.target_device = target;
        self.error = None;
        self.load(cx);
    }

    fn flush_save(&mut self, cx: &mut Context<Self>) {
        if self.save_task.take().is_none() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let request = SetTerminalCommand {
            command: self.command.read(cx).text().to_string(),
            target_device_id: self.target_device.clone(),
        };
        cx.spawn(async move |_, _| {
            if let Err(error) = call_api(engine.client(), &request).await {
                tracing::warn!(%error, "failed to flush terminal settings before device switch");
            }
        })
        .detach();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.snapshot = Loadable::Error("Engine not connected".into());
            return;
        };
        self.snapshot = Loadable::Loading;
        let target = self.target_device.clone();
        let request = TerminalSettings {
            target_device_id: target.clone(),
        };
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(engine.client(), &request).await;
            this.update(cx, |page, cx| {
                if page.target_device != target {
                    return;
                }
                match result {
                    Ok(snapshot) => {
                        page.setting_input = true;
                        page.command
                            .update(cx, |input, cx| input.set_text(snapshot.command.clone(), cx));
                        page.setting_input = false;
                        page.snapshot = Loadable::Ready(snapshot);
                    }
                    Err(error) => page.snapshot = Loadable::Error(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.snapshot, Loadable::Ready(_)) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let command = self.command.read(cx).text().to_string();
        let target = self.target_device.clone();
        self.snapshot = Loadable::Ready(TerminalSettingsSnapshot {
            command: command.clone(),
        });
        self.error = None;
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(SAVE_DEBOUNCE_MS))
                .await;
            let result = call_api(
                engine.client(),
                &SetTerminalCommand {
                    command,
                    target_device_id: target.clone(),
                },
            )
            .await;
            this.update(cx, |page, cx| {
                if page.target_device != target {
                    return;
                }
                match result {
                    Ok(snapshot) => {
                        page.snapshot = Loadable::Ready(snapshot);
                        page.error = None;
                    }
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
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
        let mut column = widgets::page_column()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(widgets::page_header(&theme, "Terminal", None))
                    .child(div().flex_1())
                    .child(self.device_switcher.clone()),
            )
            .child(widgets::page_subtitle(
                &theme,
                "Customize how new terminal tabs start on each device. Changes apply only to terminals opened afterward.",
            ));

        if let Some(error) = self.error.clone() {
            column = column.child(widgets::error_strip(&theme, error));
        }

        column = match &self.snapshot {
            Loadable::Idle | Loadable::Loading => column.child(
                widgets::section_card(&theme).child(
                    div()
                        .h(px(96.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(crate::loaders::activity_spinner(
                            "terminal-settings-loading",
                            &theme,
                            16.0,
                            cx.entity_id(),
                            cx,
                        )),
                ),
            ),
            Loadable::Error(message) => {
                column.child(widgets::error_strip(&theme, message.clone()))
            }
            Loadable::Ready(_) => column
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
                                                "Runs in the thread directory through this device’s login shell.",
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
        };

        div()
            .id("terminal-settings-page")
            .size_full()
            .overflow_y_scroll()
            .child(column)
    }
}
