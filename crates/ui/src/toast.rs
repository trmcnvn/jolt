//! App-wide notification delivery: stacked in-app toasts or native OS notices.
//!
//! The center is process-global so engine/UI features can publish notices without
//! knowing which shell route is visible. Delivery is device-local; changing it
//! clears notices from the previous destination.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    App, Context, Entity, Global, Render, SharedString, SystemNotification,
    SystemNotificationAction, Task, Window, div, prelude::*, px,
};

use crate::icons;
use crate::theme::Theme;

const DEFAULT_DURATION: Duration = Duration::from_secs(10);
const MAX_VISIBLE: usize = 4;
const PRIMARY_ACTION_ID: &str = "primary";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

impl ToastKind {
    fn icon(self) -> &'static str {
        match self {
            Self::Info => icons::INFO_CIRCLE,
            Self::Success => icons::CHECK,
            Self::Warning | Self::Error => icons::DANGER_TRIANGLE,
        }
    }

    fn color(self, theme: &Theme) -> gpui::Hsla {
        match self {
            Self::Info => theme.accent,
            Self::Success => theme.success,
            Self::Warning => theme.warning,
            Self::Error => theme.danger,
        }
    }
}

#[derive(Clone)]
pub struct ToastAction {
    label: SharedString,
    handler: Rc<dyn Fn(&mut App)>,
}

impl ToastAction {
    pub fn new(label: impl Into<SharedString>, handler: impl Fn(&mut App) + 'static) -> Self {
        Self {
            label: label.into(),
            handler: Rc::new(handler),
        }
    }
}

#[derive(Clone)]
pub struct Toast {
    id: SharedString,
    title: SharedString,
    body: Option<SharedString>,
    kind: ToastKind,
    action: Option<ToastAction>,
    auto_dismiss: bool,
}

impl Toast {
    pub fn new(
        id: impl Into<SharedString>,
        title: impl Into<SharedString>,
        kind: ToastKind,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            body: None,
            kind,
            action: None,
            auto_dismiss: true,
        }
    }

    pub fn body(mut self, body: impl Into<SharedString>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn action(mut self, action: ToastAction) -> Self {
        self.action = Some(action);
        self
    }

    pub fn persistent(mut self) -> Self {
        self.auto_dismiss = false;
        self
    }
}

struct ActiveToast {
    toast: Toast,
    dismiss_task: Option<Task<()>>,
}

#[derive(Clone)]
struct ToastCenterGlobal(Entity<ToastCenter>);

impl Global for ToastCenterGlobal {}

pub struct ToastCenter {
    system_notifications_enabled: bool,
    active: Vec<ActiveToast>,
    system_actions: HashMap<SharedString, ToastAction>,
    system_tags: HashSet<SharedString>,
}

impl ToastCenter {
    fn new(system_notifications_enabled: bool, cx: &mut Context<Self>) -> Self {
        let center = cx.entity().downgrade();
        cx.on_system_notification_response(move |response, cx| {
            cx.activate(true);
            let action = center
                .update(cx, |center, _| {
                    center.system_tags.remove(&response.tag);
                    center.system_actions.remove(&response.tag)
                })
                .ok()
                .flatten();
            if let Some(action) = action {
                cx.defer(move |cx| (action.handler)(cx));
            }
        });
        Self {
            system_notifications_enabled,
            active: Vec::new(),
            system_actions: HashMap::new(),
            system_tags: HashSet::new(),
        }
    }

    fn configure(&mut self, system_notifications_enabled: bool, cx: &mut Context<Self>) {
        if self.system_notifications_enabled != system_notifications_enabled {
            self.active.clear();
            for tag in self.system_tags.drain() {
                cx.dismiss_system_notification(tag.as_ref());
            }
            self.system_actions.clear();
        }
        self.system_notifications_enabled = system_notifications_enabled;
        cx.notify();
    }

    fn show(&mut self, toast: Toast, cx: &mut Context<Self>) {
        if self.system_notifications_enabled {
            self.show_system(toast, cx);
        } else {
            self.show_in_app(toast, cx);
        }
    }

    fn show_system(&mut self, toast: Toast, cx: &mut Context<Self>) {
        let actions = toast
            .action
            .as_ref()
            .map(|action| {
                vec![SystemNotificationAction {
                    id: PRIMARY_ACTION_ID.into(),
                    label: action.label.clone(),
                }]
            })
            .unwrap_or_default();
        if let Some(action) = toast.action.clone() {
            self.system_actions.insert(toast.id.clone(), action);
        } else {
            self.system_actions.remove(&toast.id);
        }
        self.system_tags.insert(toast.id.clone());
        cx.show_system_notification(SystemNotification {
            tag: toast.id,
            title: toast.title,
            body: toast.body.unwrap_or_default(),
            actions,
        });
    }

    fn show_in_app(&mut self, toast: Toast, cx: &mut Context<Self>) {
        self.active.retain(|active| active.toast.id != toast.id);
        if self.active.len() == MAX_VISIBLE {
            self.active.remove(0);
        }
        let dismiss_task = toast
            .auto_dismiss
            .then(|| Self::schedule_dismiss(toast.id.clone(), cx));
        self.active.push(ActiveToast {
            toast,
            dismiss_task,
        });
        cx.notify();
    }

    fn schedule_dismiss(id: SharedString, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |center, cx| {
            cx.background_executor().timer(DEFAULT_DURATION).await;
            center
                .update(cx, |center, cx| center.dismiss_in_app(&id, cx))
                .ok();
        })
    }

    fn pause_dismiss(&mut self, id: &str) {
        if let Some(active) = self.active.iter_mut().find(|active| active.toast.id == id) {
            active.dismiss_task = None;
        }
    }

    fn resume_dismiss(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(active) = self.active.iter_mut().find(|active| active.toast.id == id)
            && active.toast.auto_dismiss
            && active.dismiss_task.is_none()
        {
            active.dismiss_task = Some(Self::schedule_dismiss(active.toast.id.clone(), cx));
        }
    }

    fn dismiss_in_app(&mut self, id: &str, cx: &mut Context<Self>) {
        self.active.retain(|active| active.toast.id != id);
        cx.notify();
    }
}

impl Render for ToastCenter {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let rows = self.active.iter().rev().enumerate().map(|(index, active)| {
            let toast = active.toast.clone();
            let dismiss_id = toast.id.clone();
            let close_dismiss_id = dismiss_id.clone();
            let hover_id = toast.id.clone();
            let tone = toast.kind.color(&theme);
            let main = div()
                .w_full()
                .px(px(14.0))
                .py(px(12.0))
                .flex()
                .flex_row()
                .items_start()
                .gap(px(10.0))
                .child(
                    div().flex_none().pt(px(1.0)).child(
                        icons::icon(toast.kind.icon())
                            .size(px(16.0))
                            .text_color(tone),
                    ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_size(px(13.5))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(toast.title.clone()),
                        )
                        .when_some(toast.body.clone(), |content, body| {
                            content.child(
                                div()
                                    .mt(px(3.0))
                                    .max_h(px(68.0))
                                    .overflow_hidden()
                                    .text_size(px(12.0))
                                    .line_height(px(17.0))
                                    .text_color(theme.text_muted)
                                    .child(body),
                            )
                        }),
                )
                .child(
                    div()
                        .id(("app-toast-dismiss", index))
                        .flex_none()
                        .size(px(24.0))
                        .rounded(px(6.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .hover(|style| style.bg(crate::theme::wash(0.08)))
                        .cursor_pointer()
                        .on_click(cx.listener(move |center, _, _, cx| {
                            center.dismiss_in_app(close_dismiss_id.as_ref(), cx);
                        }))
                        .child(
                            icons::icon(icons::CLOSE)
                                .size(px(13.0))
                                .text_color(theme.text_muted),
                        ),
                );
            let mut card = div()
                .id(("app-toast", index))
                .w_full()
                .rounded(px(12.0))
                .border_1()
                .border_color(theme.border_strong)
                .bg(theme.surface_overlay)
                .shadow_lg()
                .overflow_hidden()
                .occlude()
                .on_hover(cx.listener(move |center, hovering, _, cx| {
                    if *hovering {
                        center.pause_dismiss(hover_id.as_ref());
                    } else {
                        center.resume_dismiss(hover_id.as_ref(), cx);
                    }
                }))
                .flex()
                .flex_col()
                .child(main);
            if let Some(action) = toast.action.clone() {
                card = card.child(
                    div()
                        .w_full()
                        .px(px(14.0))
                        .py(px(8.0))
                        .border_t_1()
                        .border_color(theme.border)
                        .bg(crate::theme::wash(0.03))
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .child(
                            div()
                                .id(("app-toast-action", index))
                                .flex_none()
                                .rounded(px(7.0))
                                .px(px(10.0))
                                .py(px(5.0))
                                .text_size(px(11.5))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .bg(crate::theme::wash(0.08))
                                .hover(|style| style.bg(crate::theme::wash(0.14)))
                                .cursor_pointer()
                                .on_click(cx.listener(move |center, _, _, cx| {
                                    center.dismiss_in_app(dismiss_id.as_ref(), cx);
                                    let handler = action.handler.clone();
                                    cx.defer(move |cx| handler(cx));
                                }))
                                .child(action.label),
                        ),
                );
            }
            card
        });

        div()
            .absolute()
            .top(px(Theme::TITLEBAR_HEIGHT + 8.0))
            .right(px(16.0))
            .w(px(380.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .children(rows)
    }
}

pub fn init(system_notifications_enabled: bool, cx: &mut App) {
    let center = cx.new(|cx| ToastCenter::new(system_notifications_enabled, cx));
    cx.set_global(ToastCenterGlobal(center));
}

pub fn layer(cx: &App) -> Entity<ToastCenter> {
    cx.global::<ToastCenterGlobal>().0.clone()
}

pub fn configure(system_notifications_enabled: bool, cx: &mut App) {
    layer(cx).update(cx, |center, cx| {
        center.configure(system_notifications_enabled, cx)
    });
}

pub fn show(toast: Toast, cx: &mut App) {
    layer(cx).update(cx, |center, cx| center.show(toast, cx));
}
