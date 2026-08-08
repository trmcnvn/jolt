//! Queued prompt controls and rendering.

use super::*;

impl Composer {
    pub(super) fn cancel_queued_prompt(&mut self, command_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = call_api(
                engine.client(),
                &CancelQueuedPrompt {
                    chat_id,
                    command_id,
                    target_device_id: None,
                },
            )
            .await;
            if let Err(error) = result {
                this.update(cx, |composer, cx| {
                    composer.failure =
                        Some(format!("Couldn't cancel queued message: {error}").into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    pub(super) fn resume_queue(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = call_api(
                engine.client(),
                &QueueCommand {
                    chat_id,
                    command: SessionCommandPayload::ResumeQueue {},
                    target_device_id: None,
                },
            )
            .await;
            if let Err(error) = result {
                this.update(cx, |composer, cx| {
                    composer.failure = Some(format!("Couldn't resume queue: {error}").into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    pub(super) fn cancel_bro(&mut self, cx: &mut Context<Self>) {
        self.bro_runs.remove(&self.current_key);
        self.sending = false;
        self.interrupt(cx);
        self.sync_default_placeholder(cx);
        cx.notify();
    }

    pub(super) fn interrupt(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let request = QueueCommand {
            chat_id,
            command: SessionCommandPayload::Interrupt {},
            target_device_id: None,
        };
        self.send_task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(engine.client(), &request).await;
            if let Err(err) = result {
                this.update(cx, |composer, cx| {
                    composer.failure = Some(format!("Stop failed: {err}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    pub(super) fn render_queue(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let queued = self.state.read(cx).queued_prompts.clone();
        if queued.is_empty() {
            return None;
        }
        let theme = Theme::of(cx);
        let paused = !self.run_live(cx);
        let count = queued.len();
        let rows = queued.into_iter().enumerate().map(|(index, prompt)| {
            let command_id = prompt.command_id.clone();
            let label = prompt
                .prompt
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("Queued message")
                .trim()
                .to_string();
            div()
                .id(("queued-prompt", index))
                .h(px(34.0))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(10.0))
                .rounded(px(9.0))
                .bg(crate::theme::ink(0.035))
                .child(
                    div()
                        .size(px(18.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(theme.text.opacity(0.08))
                        .text_size(px(10.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from((index + 1).to_string())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(theme.text.opacity(0.86))
                        .child(SharedString::from(label)),
                )
                .when(prompt.cancellable, |row| {
                    row.child(
                        div()
                            .id(("cancel-queued-prompt", index))
                            .size(px(22.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .cursor_pointer()
                            .hover(|style| style.bg(crate::theme::ink(0.10)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.cancel_queued_prompt(command_id.clone(), cx)
                            }))
                            .child(
                                crate::icons::icon(crate::icons::X)
                                    .size(px(12.0))
                                    .text_color(theme.text_muted),
                            ),
                    )
                })
        });
        Some(
            div()
                .id("queued-prompts")
                .mx(px(4.0))
                .rounded(px(14.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.input_bg)
                .overflow_hidden()
                .child(
                    div()
                        .h(px(34.0))
                        .px(px(11.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(11.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text_muted)
                                .child(SharedString::from(if paused {
                                    format!("QUEUE PAUSED · {count}")
                                } else {
                                    format!("NEXT UP · {count}")
                                })),
                        )
                        .when(paused, |header| {
                            header.child(
                                div()
                                    .id("resume-queued-prompts")
                                    .px(px(8.0))
                                    .h(px(24.0))
                                    .flex()
                                    .items_center()
                                    .rounded(px(7.0))
                                    .cursor_pointer()
                                    .text_size(px(11.0))
                                    .text_color(theme.text)
                                    .bg(crate::theme::ink(0.07))
                                    .hover(|style| style.bg(crate::theme::ink(0.12)))
                                    .on_click(cx.listener(|this, _, _, cx| this.resume_queue(cx)))
                                    .child("Resume"),
                            )
                        }),
                )
                .child(
                    div()
                        .id("queued-prompts-scroll")
                        .max_h(px(136.0))
                        .overflow_y_scroll()
                        .px(px(6.0))
                        .pb(px(6.0))
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .children(rows),
                )
                .into_any_element(),
        )
    }
}
