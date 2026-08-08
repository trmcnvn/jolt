//! Settings → Secrets: device-local values exposed as environment variables to
//! selected harnesses. Values are write-only and remain in the OS credential
//! store; this page receives metadata only after creation.

use std::collections::HashSet;

use gpui::{Context, Entity, Render, SharedString, Subscription, Task, div, prelude::*, px};
use jolt_api::{DeleteHarnessSecret, ListHarnessSecrets, UpsertHarnessSecret, call as call_api};
use jolt_proto::{HarnessId, HarnessSecretsSnapshot};

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::Loadable;
use crate::state::AppState;
use crate::theme::Theme;
use crate::{icons, popover, settings::widgets};

const HARNESSES: &[(HarnessId, &str)] = &[
    (HarnessId::ClaudeCode, "Claude Code"),
    (HarnessId::Codex, "Codex"),
    (HarnessId::Pi, "Pi"),
];

pub struct SecretsPage {
    state: Entity<AppState>,
    snapshot: Loadable<HarnessSecretsSnapshot>,
    label: Entity<ComposerInput>,
    environment_variable: Entity<ComposerInput>,
    value: Entity<ComposerInput>,
    selected_harnesses: HashSet<HarnessId>,
    form_open: bool,
    busy: bool,
    delete_confirm: Option<String>,
    deleting: Option<String>,
    error: Option<SharedString>,
    load_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    _observe: Subscription,
    _label_events: Subscription,
    _environment_events: Subscription,
    _value_events: Subscription,
}

impl SecretsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let label = cx.new(|cx| ComposerInput::new("Production API key", cx));
        let environment_variable = cx.new(|cx| ComposerInput::new("SERVICE_API_KEY", cx));
        let value = cx.new(|cx| {
            let mut input = ComposerInput::new("Secret value", cx);
            input.set_masked(true, cx);
            input
        });
        let label_events = cx.subscribe(&label, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit(cx);
            }
        });
        let environment_events =
            cx.subscribe(&environment_variable, |this: &mut Self, _, event, cx| {
                if matches!(event, ComposerInputEvent::Submitted) {
                    this.submit(cx);
                }
            });
        let value_events = cx.subscribe(&value, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit(cx);
            }
        });
        let mut page = Self {
            state,
            snapshot: Loadable::Idle,
            label,
            environment_variable,
            value,
            selected_harnesses: HashSet::new(),
            form_open: false,
            busy: false,
            delete_confirm: None,
            deleting: None,
            error: None,
            load_task: None,
            action_task: None,
            _observe: observe,
            _label_events: label_events,
            _environment_events: environment_events,
            _value_events: value_events,
        };
        page.load(cx);
        page
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.snapshot = Loadable::Error("Engine not connected".into());
            return;
        };
        self.snapshot = Loadable::Loading;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(engine.client(), &ListHarnessSecrets::default()).await;
            this.update(cx, |page, cx| {
                page.snapshot = match result {
                    Ok(snapshot) => Loadable::Ready(snapshot),
                    Err(error) => Loadable::Error(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn toggle_harness(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        if !self.selected_harnesses.remove(&harness) {
            self.selected_harnesses.insert(harness);
        }
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let storage_available = self
            .snapshot
            .ready()
            .is_some_and(|snapshot| snapshot.storage_available);
        if self.busy || !storage_available {
            return;
        }
        let label = self.label.read(cx).text().trim().to_owned();
        let environment_variable = self.environment_variable.read(cx).text().trim().to_owned();
        let value = self.value.read(cx).text().to_owned();
        if label.is_empty() || environment_variable.is_empty() || value.is_empty() {
            self.error = Some("Label, environment variable, and value are required.".into());
            cx.notify();
            return;
        }
        let harnesses: Vec<_> = HARNESSES
            .iter()
            .map(|(harness, _)| *harness)
            .filter(|harness| self.selected_harnesses.contains(harness))
            .collect();
        if harnesses.is_empty() {
            self.error = Some("Select at least one harness.".into());
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.busy = true;
        self.error = None;
        let request = UpsertHarnessSecret {
            id: None,
            label,
            environment_variable,
            harnesses,
            value: Some(value),
        };
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(engine.client(), &request).await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(snapshot) => {
                        page.snapshot = Loadable::Ready(snapshot);
                        page.clear_form(cx);
                        page.form_open = false;
                    }
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn request_delete(&mut self, id: String, cx: &mut Context<Self>) {
        if self.delete_confirm.as_deref() == Some(&id) {
            self.delete_confirm = None;
            self.delete(id, cx);
        } else {
            self.delete_confirm = Some(id);
            cx.notify();
        }
    }

    fn delete(&mut self, id: String, cx: &mut Context<Self>) {
        if self.busy || self.deleting.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.deleting = Some(id.clone());
        self.error = None;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(engine.client(), &DeleteHarnessSecret { id }).await;
            this.update(cx, |page, cx| {
                page.deleting = None;
                page.delete_confirm = None;
                match result {
                    Ok(snapshot) => page.snapshot = Loadable::Ready(snapshot),
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn clear_form(&mut self, cx: &mut Context<Self>) {
        self.label.update(cx, |input, cx| input.set_text("", cx));
        self.environment_variable
            .update(cx, |input, cx| input.set_text("", cx));
        self.value.update(cx, |input, cx| input.set_text("", cx));
        self.selected_harnesses.clear();
        self.error = None;
    }

    fn open_form(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.form_open = true;
        cx.notify();
    }

    fn close_form(&mut self, cx: &mut Context<Self>) {
        self.dismiss_modal(cx);
    }

    pub(crate) fn dismiss_modal(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.form_open {
            return false;
        }
        self.clear_form(cx);
        self.form_open = false;
        cx.notify();
        true
    }

    fn input_field(
        &self,
        theme: &Theme,
        label: &'static str,
        input: Entity<ComposerInput>,
    ) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .child(widgets::field_label(theme, label))
            .child(popover::dialog_field(input.into_any_element()))
    }

    fn render_add_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        storage_available: bool,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !self.form_open {
            return None;
        }
        let theme = Theme::of(cx).clone();
        let harnesses = div().flex().flex_row().flex_wrap().gap(px(8.0)).children(
            HARNESSES
                .iter()
                .enumerate()
                .map(|(index, (harness, name))| {
                    let harness = *harness;
                    let selected = self.selected_harnesses.contains(&harness);
                    widgets::ghost_action(&theme)
                        .id(("secret-harness", index))
                        .border_1()
                        .border_color(if selected {
                            theme.accent.opacity(0.7)
                        } else {
                            theme.border
                        })
                        .bg(if selected {
                            theme.accent.opacity(0.08)
                        } else {
                            crate::theme::ink(0.03)
                        })
                        .text_color(if selected {
                            theme.text
                        } else {
                            theme.text_muted
                        })
                        .hover(|style| widgets::ghost_hover(&theme, style))
                        .on_click(
                            cx.listener(move |page, _, _, cx| page.toggle_harness(harness, cx)),
                        )
                        .when(selected, |button| {
                            button.child(
                                icons::icon(icons::CHECK)
                                    .size(px(13.0))
                                    .text_color(theme.accent),
                            )
                        })
                        .child(SharedString::from(*name))
                }),
        );
        let card = popover::dialog_card(&theme)
            .w(px(460.0))
            .child(popover::dialog_title(&theme, "Add secret"))
            .child(div().mt(px(5.0)).child(popover::dialog_body(
                &theme,
                "The value is write-only and stays in this device’s secure credential store.",
            )))
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_col()
                    .gap(px(14.0))
                    .child(self.input_field(&theme, "Label", self.label.clone()))
                    .child(self.input_field(
                        &theme,
                        "Environment variable",
                        self.environment_variable.clone(),
                    ))
                    .child(self.input_field(&theme, "Secret value", self.value.clone()))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.0))
                            .child(widgets::field_label(&theme, "Available to"))
                            .child(harnesses),
                    ),
            )
            .when_some(self.error.clone(), |card, error| {
                card.child(
                    div()
                        .mt(px(12.0))
                        .text_size(px(12.0))
                        .text_color(theme.danger)
                        .child(error),
                )
            })
            .child(
                div()
                    .mt(px(18.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "add-secret-cancel")
                            .id("add-secret-cancel")
                            .when(!self.busy, |button| {
                                button.on_click(cx.listener(|page, _, _, cx| page.close_form(cx)))
                            })
                            .when(self.busy, |button| button.opacity(0.5)),
                    )
                    .child(
                        popover::btn_primary(
                            &theme,
                            if self.busy { "Saving…" } else { "Add secret" },
                        )
                        .id("add-harness-secret")
                        .when(storage_available && !self.busy, |button| {
                            button.on_click(cx.listener(|page, _, _, cx| page.submit(cx)))
                        })
                        .when(!storage_available || self.busy, |button| {
                            button.opacity(0.5)
                        }),
                    ),
            )
            .into_any_element();
        Some(popover::modal("add-secret-dialog", viewport, card))
    }
}

impl Render for SecretsPage {
    fn render(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let snapshot = self.snapshot.ready().cloned();
        let storage_available = snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.storage_available);
        let secret_count = snapshot.as_ref().map(|snapshot| snapshot.secrets.len());
        let dialog = self.render_add_dialog(window.viewport_size(), storage_available, cx);

        let mut column = widgets::page_column()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(widgets::page_header(&theme, "Secrets", secret_count))
                    .child(div().flex_1())
                    .when(snapshot.is_some(), |header| {
                        header.child(
                            popover::btn_primary(&theme, "Add secret")
                                .id("open-add-harness-secret")
                                .when(storage_available && !self.busy, |button| {
                                    button.on_click(
                                        cx.listener(|page, _, _, cx| page.open_form(cx)),
                                    )
                                })
                                .when(!storage_available || self.busy, |button| {
                                    button.opacity(0.5)
                                }),
                        )
                    }),
            )
            .child(widgets::page_subtitle(
                &theme,
                "Device-local credentials shared with only the harnesses you choose. Stored values can’t be viewed after they’re added.",
            ));

        match &self.snapshot {
            Loadable::Idle | Loadable::Loading => {
                column = column.child(
                    widgets::section_card(&theme).child(
                        div()
                            .h(px(116.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(crate::loaders::activity_spinner(
                                "secrets-settings-loading",
                                &theme,
                                16.0,
                                cx.entity_id(),
                                cx,
                            )),
                    ),
                );
            }
            Loadable::Error(message) => {
                column = column.child(widgets::error_strip(&theme, message.clone()));
            }
            Loadable::Ready(snapshot) => {
                if let Some(error) = &snapshot.storage_error {
                    column = column.child(widgets::error_strip(
                        &theme,
                        format!("Secure storage is unavailable: {error}"),
                    ));
                }

                let mut list = widgets::section_card(&theme).child(
                    div()
                        .px(px(20.0))
                        .py(px(14.0))
                        .child(widgets::row_title(&theme, "Saved secrets")),
                );
                if snapshot.secrets.is_empty() {
                    list = list.child(
                        div()
                            .border_t_1()
                            .border_color(theme.border)
                            .px(px(20.0))
                            .py(px(30.0))
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(10.0))
                            .child(widgets::row_tile(&theme, icons::KEY))
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child("No secrets yet"),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(theme.text_muted)
                                    .child("Add a credential to make it available to a harness."),
                            )
                            .child(
                                popover::btn_ghost(
                                    &theme,
                                    "Add your first secret",
                                    "empty-add-secret",
                                )
                                .id("empty-add-secret")
                                .when(storage_available, |button| {
                                    button
                                        .on_click(cx.listener(|page, _, _, cx| page.open_form(cx)))
                                })
                                .when(!storage_available, |button| button.opacity(0.5)),
                            ),
                    );
                } else {
                    for (index, secret) in snapshot.secrets.iter().cloned().enumerate() {
                        let id = secret.id.clone();
                        let deleting = self.deleting.as_deref() == Some(id.as_str());
                        let confirming = self.delete_confirm.as_deref() == Some(id.as_str());
                        let harnesses = secret
                            .harnesses
                            .iter()
                            .filter_map(|id| {
                                HARNESSES
                                    .iter()
                                    .find(|(candidate, _)| candidate == id)
                                    .map(|(_, label)| *label)
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        list = list.child(
                            widgets::card_row(&theme, false)
                                .child(widgets::row_tile(&theme, icons::KEY))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(widgets::row_title(&theme, secret.label))
                                        .child(widgets::meta_line(
                                            &theme,
                                            vec![
                                                div()
                                                    .font_family(theme.font_mono.clone())
                                                    .child(SharedString::from(
                                                        secret.environment_variable,
                                                    ))
                                                    .into_any_element(),
                                                div()
                                                    .child(SharedString::from(harnesses))
                                                    .into_any_element(),
                                            ],
                                        )),
                                )
                                .child(
                                    widgets::ghost_action(&theme)
                                        .id(("delete-harness-secret", index))
                                        .hover(|style| widgets::ghost_hover(&theme, style))
                                        .when(!deleting, |button| {
                                            button.on_click(cx.listener(move |page, _, _, cx| {
                                                page.request_delete(id.clone(), cx)
                                            }))
                                        })
                                        .child(SharedString::from(if deleting {
                                            "Deleting…"
                                        } else if confirming {
                                            "Confirm delete"
                                        } else {
                                            "Delete"
                                        })),
                                ),
                        );
                    }
                }
                column = column.child(list);
            }
        }

        if !self.form_open
            && let Some(error) = &self.error
        {
            column = column.child(widgets::error_strip(&theme, error.clone()));
        }
        column = column.child(widgets::warning_strip(
            &theme,
            "Harnesses and commands they launch can read granted environment variables. Only store credentials you intend to expose to those tools.",
        ));

        div()
            .id("secrets-settings-page")
            .size_full()
            .overflow_y_scroll()
            .child(column)
            .children(dialog)
    }
}
