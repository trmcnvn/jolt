//! Shared page-header switcher for device-local settings.

use std::time::{Duration, Instant};

use gpui::{
    Context, Entity, EventEmitter, Render, SharedString, Subscription, div, prelude::*, px,
};

use crate::icons::{self, icon};
use crate::popover;
use crate::state::AppState;
use crate::theme::Theme;

#[derive(Clone)]
pub(super) struct DeviceSelected(pub Option<String>);

pub(super) struct DeviceSwitcher {
    state: Entity<AppState>,
    /// `None` addresses the local device directly; remote ids relay-forward.
    target_device: Option<String>,
    menu_open: bool,
    /// Suppresses the trigger click following the same outside mouse-down.
    dismissed_at: Option<Instant>,
    _observe: Subscription,
}

impl DeviceSwitcher {
    pub(super) fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            state,
            target_device: None,
            menu_open: false,
            dismissed_at: None,
            _observe: observe,
        }
    }

    fn select(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        self.menu_open = false;
        if self.target_device != target {
            self.target_device = target.clone();
            cx.emit(DeviceSelected(target));
        }
        cx.notify();
    }
}

impl EventEmitter<DeviceSelected> for DeviceSwitcher {}

impl Render for DeviceSwitcher {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let (mut devices, local_id) = {
            let state = self.state.read(cx);
            (state.devices.clone(), state.local_device_id.clone())
        };
        devices.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let effective = self.target_device.clone().or_else(|| local_id.clone());
        let selected = devices
            .iter()
            .find(|device| Some(device.id.as_str()) == effective.as_deref())
            .cloned();
        let platform_glyph = |platform: &str| match platform {
            "macos" | "darwin" => icons::LAPTOP,
            "ios" | "android" => icons::SMARTPHONE,
            _ => icons::MONITOR,
        };
        let trigger_glyph = platform_glyph(
            selected
                .as_ref()
                .map(|device| device.platform.as_str())
                .unwrap_or("macos"),
        );
        let trigger_label: SharedString = selected
            .as_ref()
            .map(|device| device.name.clone().into())
            .unwrap_or_else(|| SharedString::from("This device"));
        let open = self.menu_open;

        let mut trigger =
            div()
                .id("settings-device-switcher")
                .flex_none()
                .h(px(28.0))
                .px(px(8.0))
                .rounded(px(6.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .bg(if open {
                    crate::theme::ink(0.06)
                } else {
                    gpui::transparent_black()
                })
                .when(!open, |element| {
                    element.hover(|style| style.bg(crate::theme::ink(0.04)))
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    let just_dismissed = this
                        .dismissed_at
                        .is_some_and(|at| at.elapsed() < Duration::from_millis(400));
                    this.menu_open = !this.menu_open && !just_dismissed;
                    this.dismissed_at = None;
                    cx.notify();
                }))
                .child(
                    icon(trigger_glyph)
                        .size(px(16.0))
                        .flex_none()
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(trigger_label),
                )
                .child(div().size(px(6.0)).rounded_full().flex_none().bg(
                    if effective == local_id {
                        theme.success
                    } else {
                        crate::theme::ink(0.2)
                    },
                ))
                .child(
                    icon(icons::SORT_VERTICAL)
                        .size(px(14.0))
                        .flex_none()
                        .text_color(theme.text_muted.opacity(if open { 0.9 } else { 0.4 })),
                );

        if open {
            let menu = popover::popover_card(&theme)
                .w(px(220.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.menu_open = false;
                    this.dismissed_at = Some(Instant::now());
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(popover::menu_heading(&theme, "Devices"))
                .children(devices.into_iter().enumerate().map(|(index, device)| {
                    let active = Some(device.id.as_str()) == effective.as_deref();
                    let local = local_id.as_deref() == Some(device.id.as_str());
                    let glyph = platform_glyph(&device.platform);
                    let name: SharedString = device.name.clone().into();
                    let id = device.id.clone();
                    popover::menu_row(&theme, active, format!("settings-device-row-{index}"))
                        .id(("settings-device-row", index))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select((!local).then(|| id.clone()), cx);
                        }))
                        .child(
                            icon(glyph)
                                .size(px(16.0))
                                .flex_none()
                                .text_color(theme.text_muted),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        .when(local, |element| {
                            element.child(
                                div()
                                    .flex_none()
                                    .text_size(px(10.5))
                                    .text_color(theme.text_muted.opacity(0.35))
                                    .child(SharedString::from("You")),
                            )
                        })
                        .child(div().size(px(6.0)).rounded_full().flex_none().bg(if local {
                            theme.success
                        } else {
                            crate::theme::ink(0.2)
                        }))
                }))
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu("settings-device-menu", menu));
        }

        trigger
    }
}
