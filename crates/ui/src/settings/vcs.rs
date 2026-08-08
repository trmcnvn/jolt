//! Settings → Version control: one active command-line backend per device.

use gpui::{Context, Entity, Render, SharedString, Subscription, Task, div, prelude::*, px};

use jolt_api::{SetVcsBackend, VcsSettings, call as call_api};
use jolt_proto::{VcsKind, VcsSettingsSnapshot};

use super::device_switcher::{DeviceSelected, DeviceSwitcher};
use crate::popover::Loadable;
use crate::state::AppState;
use crate::theme::Theme;
use crate::{icons, settings::widgets};

pub struct VcsPage {
    state: Entity<AppState>,
    /// `None` addresses the connected engine; remote ids relay-forward.
    target_device: Option<String>,
    device_switcher: Entity<DeviceSwitcher>,
    snapshot: Loadable<VcsSettingsSnapshot>,
    busy: Option<VcsKind>,
    load_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    _observe: Subscription,
    _device_switcher_events: Subscription,
}

impl VcsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
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
            snapshot: Loadable::Idle,
            busy: None,
            load_task: None,
            action_task: None,
            _observe: observe,
            _device_switcher_events: device_switcher_events,
        };
        page.load(cx);
        page
    }

    fn select_device(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        if self.target_device == target {
            return;
        }
        self.target_device = target;
        self.busy = None;
        self.load(cx);
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.snapshot = Loadable::Error("Engine not connected".into());
            return;
        };
        self.snapshot = Loadable::Loading;
        let request = VcsSettings {
            target_device_id: self.target_device.clone(),
        };
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(engine.client(), &request).await;
            this.update(cx, |page, cx| {
                page.snapshot = match result {
                    Ok(snapshot) => Loadable::Ready(snapshot),
                    Err(err) => Loadable::Error(err.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn select_backend(&mut self, kind: VcsKind, cx: &mut Context<Self>) {
        let available = self
            .snapshot
            .ready()
            .and_then(|snapshot| {
                snapshot
                    .backends
                    .iter()
                    .find(|backend| backend.kind == kind)
            })
            .is_some_and(|backend| backend.available);
        if !available || self.busy.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.busy = Some(kind);
        let request = SetVcsBackend {
            backend: kind,
            target_device_id: self.target_device.clone(),
        };
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(engine.client(), &request).await;
            this.update(cx, |page, cx| {
                page.busy = None;
                page.snapshot = match result {
                    Ok(snapshot) => Loadable::Ready(snapshot),
                    Err(err) => Loadable::Error(err.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
    }
}

impl Render for VcsPage {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let mut column = widgets::page_column()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(widgets::page_header(&theme, "Version control", None))
                    .child(div().flex_1())
                    .child(self.device_switcher.clone()),
            )
            .child(widgets::page_subtitle(
                &theme,
                "Choose the command-line backend used by each device. Jujutsu is preferred when no choice has been saved.",
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
                                "vcs-settings-loading",
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
                let mut card = widgets::section_card(&theme).child(
                    div()
                        .px(px(20.0))
                        .pt(px(16.0))
                        .pb(px(4.0))
                        .child(widgets::field_label(&theme, "Backend")),
                );
                for (index, backend) in snapshot.backends.iter().cloned().enumerate() {
                    let kind = backend.kind;
                    let busy = self.busy == Some(kind);
                    let icon = match kind {
                        VcsKind::Git => icons::GIT_BRANCH,
                        VcsKind::Jujutsu => icons::JJ_MARK,
                    };
                    let detail = match (&backend.executable, &backend.version) {
                        (Some(path), Some(version)) => format!("{version} · {path}"),
                        (Some(path), None) => path.clone(),
                        _ if kind == VcsKind::Jujutsu => "Requires jj 0.43 or newer".into(),
                        _ => "Executable not found".into(),
                    };
                    card = card.child(
                        widgets::card_row(&theme, index == 0)
                            .id(("vcs-backend", index))
                            .when(backend.available && !busy, |row| {
                                row.cursor_pointer()
                                    .on_click(cx.listener(move |page, _, _, cx| {
                                        page.select_backend(kind, cx);
                                    }))
                            })
                            .when(!backend.available, |row| row.opacity(0.5))
                            .child(widgets::row_tile(&theme, icon))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(widgets::row_title(&theme, kind.label()))
                                    .child(widgets::meta_line(
                                        &theme,
                                        vec![
                                            div()
                                                .child(SharedString::from(detail))
                                                .into_any_element(),
                                        ],
                                    )),
                            )
                            .when(busy, |row| row.child(widgets::badge(&theme, "Switching…")))
                            .when(backend.selected && !busy, |row| {
                                row.child(widgets::badge_active(&theme, "Active"))
                            })
                            .when(!backend.available, |row| {
                                row.child(widgets::badge(&theme, "Unavailable"))
                            }),
                    );
                }
                column = column.child(card);
            }
        }

        div()
            .id("vcs-settings-page")
            .size_full()
            .overflow_y_scroll()
            .child(column)
    }
}
