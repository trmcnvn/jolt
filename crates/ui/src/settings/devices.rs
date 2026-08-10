//! Settings → Devices: the device registry — name,
//! platform, last-seen, presence dot, a "This device" badge, click-to-copy id,
//! and Rename/Remove dialogs (workspace mutations).

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, ClipboardItem, Context, Entity, SharedString, Stateful, Subscription, Task, Window,
    div, prelude::*, px,
};
use std::time::Duration;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover;
use crate::state::{AppState, RemoteJoltUpdateAction};
use crate::theme::Theme;
use jolt_api::{Mutate, call as call_api};

/// A device that pinged within this window shows a presence dot (engines
/// heartbeat every 15s; 70s tolerates a couple of missed beats).
pub const DEVICE_ONLINE_WINDOW_SECS: i64 = 70;

/// Presence: last-seen within the online window (future timestamps count). Pure.
pub fn device_online(last_seen: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    last_seen
        .is_some_and(|at| now.signed_duration_since(at).num_seconds() <= DEVICE_ONLINE_WINDOW_SECS)
}

pub(super) fn device_row_online(
    device_id: &str,
    local_device_id: Option<&str>,
    last_seen: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    local_device_id == Some(device_id) || device_online(last_seen, now)
}

/// Compact last-seen line. Pure.
pub fn format_last_seen(last_seen: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(at) = last_seen else {
        return "never seen".to_string();
    };
    let secs = now.signed_duration_since(at).num_seconds();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

struct RenameDialog {
    device_id: String,
    input: Entity<ComposerInput>,
    _events: Subscription,
}

pub struct DevicesPage {
    state: Entity<AppState>,
    background_service: crate::background_service::BackgroundServiceController,
    rename: Option<RenameDialog>,
    delete_confirm: Option<String>,
    /// Device id whose id-chip shows "Copied" right now.
    copied: Option<String>,
    error: Option<SharedString>,
    task: Option<Task<()>>,
    copy_task: Option<Task<()>>,
    _observe: Subscription,
}

impl DevicesPage {
    pub(crate) fn new(
        state: Entity<AppState>,
        background_service: crate::background_service::BackgroundServiceController,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let error = background_service.take_error().map(SharedString::from);
        Self {
            state,
            background_service,
            rename: None,
            delete_confirm: None,
            copied: None,
            error,
            task: None,
            copy_task: None,
            _observe: observe,
        }
    }

    fn open_rename(&mut self, device_id: String, current: String, cx: &mut Context<Self>) {
        self.delete_confirm = None;
        let input = cx.new(|cx| ComposerInput::new("Device name", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename(cx);
            }
        });
        self.rename = Some(RenameDialog {
            device_id,
            input,
            _events: events,
        });
        cx.notify();
    }

    fn submit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename.take() else {
            return;
        };
        let name = dialog.input.read(cx).text().trim().to_string();
        if name.is_empty() {
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let request = Mutate::RenameDevice {
            device_id: dialog.device_id,
            name,
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(engine.client(), &request).await;
            this.update(cx, |page, cx| {
                if let Err(err) = result {
                    page.error = Some(format!("Rename failed: {err}").into());
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn delete_device(&mut self, cx: &mut Context<Self>) {
        let Some(device_id) = self.delete_confirm.take() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let request = Mutate::DeleteDevice { device_id };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(engine.client(), &request).await;
            this.update(cx, |page, cx| {
                if let Err(err) = result {
                    page.error = Some(format!("Remove failed: {err}").into());
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn set_background_service_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if let Err(err) = crate::background_service::relaunch_after_exit() {
            self.error = Some(format!("Could not restart Jolt: {err:#}").into());
            cx.notify();
            return;
        }
        self.background_service.request(enabled);
        cx.quit();
    }

    fn copy_id(&mut self, device_id: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(device_id.clone()));
        self.copied = Some(device_id);
        self.copy_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            this.update(cx, |page, cx| {
                page.copied = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub(crate) fn dismiss_modal(&mut self, cx: &mut Context<Self>) -> bool {
        let was_open = self.rename.is_some() || self.delete_confirm.is_some();
        if was_open {
            self.rename = None;
            self.delete_confirm = None;
            cx.notify();
        }
        was_open
    }

    fn render_rename_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let dialog = self.rename.as_ref()?;
        let input = dialog.input.clone();
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(&theme, "Rename device"))
            .child(
                div()
                    .mt(px(12.0))
                    .child(popover::dialog_field(input.into_any_element())),
            )
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "rename-cancel")
                            .id("rename-cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.rename = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        popover::btn_primary(&theme, "Rename")
                            .id("rename-save")
                            .on_click(cx.listener(|this, _, _, cx| this.submit_rename(cx))),
                    ),
            )
            .into_any_element();
        Some(popover::modal("rename-device-dialog", viewport, card))
    }

    fn render_delete_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let device_id = self.delete_confirm.as_deref()?;
        let state = self.state.read(cx);
        let device = state.devices.iter().find(|device| device.id == device_id)?;
        let spaces: Vec<&str> = state
            .spaces
            .iter()
            .filter(|space| space.device_id == device_id)
            .map(|space| space.id.as_str())
            .collect();
        let session_count = state
            .chats
            .iter()
            .filter(|chat| {
                chat.space_id
                    .as_deref()
                    .is_some_and(|space_id| spaces.contains(&space_id))
            })
            .count();
        let copy = format!(
            "Removing “{}” permanently deletes its {} {} and {} {}. Folders and local files aren’t affected.",
            device.name,
            spaces.len(),
            if spaces.len() == 1 { "space" } else { "spaces" },
            session_count,
            if session_count == 1 {
                "thread"
            } else {
                "threads"
            },
        );
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(&theme, "Remove device?"))
            .child(div().mt(px(6.0)).child(popover::dialog_body(&theme, copy)))
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "delete-device-cancel")
                            .id("delete-device-cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.delete_confirm = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        popover::btn_danger(&theme, "Remove")
                            .id("delete-device-confirm")
                            .on_click(cx.listener(|this, _, _, cx| this.delete_device(cx))),
                    ),
            )
            .into_any_element();
        Some(popover::modal("delete-device-dialog", viewport, card))
    }
}

fn background_service_toggle(theme: &Theme, enabled: bool) -> Stateful<gpui::Div> {
    div()
        .id("background-service-toggle")
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

/// Human-readable platform label.
pub fn platform_label(platform: &str) -> &str {
    match platform {
        "macos" | "darwin" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        "web" => "Web",
        "ios" => "iOS",
        "android" => "Android",
        other => other,
    }
}

/// Short device id for the click-to-copy chip (`abcd1234…wxyz`).
pub fn short_id(id: &str) -> String {
    if id.len() > 12 {
        format!("{}…{}", &id[..8], &id[id.len() - 4..])
    } else {
        id.to_string()
    }
}

impl Render for DevicesPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let (devices, local_id, remote_updates, remote_update_actions) = {
            let state = self.state.read(cx);
            (
                state.devices.clone(),
                state.local_device_id.clone(),
                state.remote_updates.clone(),
                state.remote_update_actions.clone(),
            )
        };
        let copied = self.copied.clone();
        let rename_dialog = self.render_rename_dialog(window.viewport_size(), cx);
        let delete_dialog = self.render_delete_dialog(window.viewport_size(), cx);
        let emerald = theme.success; // emerald-400
        let count = devices.len();
        let background_service_card = crate::background_service::supported().then(|| {
            let enabled = self.background_service.enabled();
            let toggle = background_service_toggle(&theme, enabled).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.set_background_service_enabled(!enabled, cx);
                },
            ));
            widgets::section_card(&theme).child(
                widgets::card_row(&theme, true)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(widgets::row_title(
                                &theme,
                                "Keep this device available",
                            ))
                            .child(
                                div()
                                    .mt(px(3.0))
                                    .text_size(px(11.5))
                                    .text_color(theme.text_muted.opacity(0.7))
                                    .child(SharedString::from(
                                        "Run Jolt’s engine in the background and start it when you sign in, so agents and remote threads continue after you quit Jolt. Changing this setting restarts the app.",
                                    )),
                            ),
                    )
                    .child(toggle),
            )
        });

        let rows: Vec<AnyElement> = devices
            .into_iter()
            .enumerate()
            .map(|(ix, device)| {
                let is_local = local_id.as_deref() == Some(device.id.as_str());
                let online =
                    device_row_online(&device.id, local_id.as_deref(), device.last_seen_at, now);
                let id_copied = copied.as_deref() == Some(device.id.as_str());
                let copy_id = device.id.clone();
                let rename_id = device.id.clone();
                let rename_name = device.name.clone();
                let delete_id = device.id.clone();
                let remote_update = remote_updates.get(&device.id).cloned();
                let remote_update_action = remote_update_actions.get(&device.id).cloned();
                let update_state = self.state.clone();
                let platform_icon = match device.platform.as_str() {
                    "macos" | "darwin" => crate::icons::DEVICE_LAPTOP,
                    "web" => crate::icons::WORLD,
                    "ios" | "android" => crate::icons::DEVICE_MOBILE,
                    _ => crate::icons::DEVICE_DESKTOP,
                };
                // Presence lives ON the identity tile: a corner dot (emerald
                // online with a soft glow, faint offline), ringed by the card
                // tone so it cuts through the tile, plus
                // `shadow-[0_0_6px_rgba(52,211,153,0.55)]`.
                let tile = widgets::row_tile(&theme, platform_icon).relative().child(
                    div()
                        .absolute()
                        .bottom(px(-3.0))
                        .right(px(-3.0))
                        .size(px(9.0))
                        .rounded_full()
                        .border_2()
                        .border_color(theme.surface)
                        .when(online, |el| {
                            el.bg(emerald).shadow(vec![gpui::BoxShadow {
                                color: emerald.opacity(0.55),
                                offset: gpui::point(px(0.0), px(0.0)),
                                blur_radius: px(6.0),
                                spread_radius: px(0.0),
                                inset: false,
                            }])
                        })
                        .when(!online, |el| el.bg(crate::theme::ink(0.22))),
                );
                // One quiet meta line: platform · version · (offline: last
                // seen) · id chip.
                let mut meta: Vec<AnyElement> = vec![
                    div()
                        .child(SharedString::from(
                            platform_label(&device.platform).to_string(),
                        ))
                        .into_any_element(),
                ];
                if let Some(version) = device.version.as_deref().filter(|v| !v.is_empty()) {
                    meta.push(
                        div()
                            .child(SharedString::from(format!("v{version}")))
                            .into_any_element(),
                    );
                }
                if !online {
                    meta.push(
                        div()
                            .child(SharedString::from(format!(
                                "Last seen {}",
                                format_last_seen(device.last_seen_at, now)
                            )))
                            .into_any_element(),
                    );
                }
                // "Added {time ago}" is always present.
                if let Some(created) = device.created_at {
                    meta.push(
                        div()
                            .child(SharedString::from(format!(
                                "Added {}",
                                format_last_seen(Some(created), now)
                            )))
                            .into_any_element(),
                    );
                }
                meta.push(
                    div()
                        .id(("device-id", ix))
                        .font_family(theme.font_mono.clone())
                        .text_size(px(10.5))
                        .text_color(if id_copied {
                            theme.success_muted.opacity(0.9)
                        } else {
                            theme.text_muted.opacity(0.5)
                        })
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.text_muted))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.copy_id(copy_id.clone(), cx);
                        }))
                        .child(SharedString::from(if id_copied {
                            "Copied".to_string()
                        } else {
                            short_id(&device.id)
                        }))
                        .into_any_element(),
                );

                let (update_action, update_detail): (Option<AnyElement>, Option<SharedString>) =
                    if is_local {
                        (None, None)
                    } else if let Some(action) = remote_update_action {
                        match action {
                            RemoteJoltUpdateAction::Applying { target_version } => (
                                Some(widgets::badge_active(&theme, "Updating…").into_any_element()),
                                Some(
                                    format!(
                                        "Preparing Jolt v{target_version}; active work can finish"
                                    )
                                    .into(),
                                ),
                            ),
                            RemoteJoltUpdateAction::Verifying { target_version } => (
                                Some(widgets::badge(&theme, "Reconnecting…").into_any_element()),
                                Some(format!("Verifying Jolt v{target_version}").into()),
                            ),
                            RemoteJoltUpdateAction::Failed {
                                target_version,
                                message,
                            } => {
                                let device_id = device.id.clone();
                                let retry_version = target_version.clone();
                                (
                                    Some(
                                        widgets::ghost_action(&theme)
                                            .id(("device-update-retry", ix))
                                            .text_color(theme.danger)
                                            .hover(|style| {
                                                style
                                                    .bg(theme.danger.opacity(0.08))
                                                    .text_color(theme.danger)
                                            })
                                            .on_click(cx.listener(move |_, _, _, cx| {
                                                update_state.update(cx, |state, cx| {
                                                    state.begin_remote_jolt_update(
                                                        device_id.clone(),
                                                        retry_version.clone(),
                                                        cx,
                                                    );
                                                });
                                            }))
                                            .child(SharedString::from("Retry"))
                                            .into_any_element(),
                                    ),
                                    Some(message.into()),
                                )
                            }
                        }
                    } else if let Some(status) = remote_update {
                        if status.update_available {
                            let target_version = status.latest_version.unwrap_or_default();
                            if status.can_apply && online {
                                let device_id = device.id.clone();
                                let requested_version = target_version.clone();
                                (
                                    Some(
                                        widgets::ghost_action(&theme)
                                            .id(("device-update", ix))
                                            .text_color(theme.success_muted)
                                            .hover(|style| {
                                                style
                                                    .bg(theme.success.opacity(0.08))
                                                    .text_color(theme.success_muted)
                                            })
                                            .on_click(cx.listener(move |_, _, _, cx| {
                                                update_state.update(cx, |state, cx| {
                                                    state.begin_remote_jolt_update(
                                                        device_id.clone(),
                                                        requested_version.clone(),
                                                        cx,
                                                    );
                                                });
                                            }))
                                            .child(SharedString::from("Update"))
                                            .into_any_element(),
                                    ),
                                    Some(
                                        format!(
                                            "Jolt {} → {target_version}",
                                            status.current_version
                                        )
                                        .into(),
                                    ),
                                )
                            } else {
                                (
                                    Some(
                                        widgets::badge(
                                            &theme,
                                            if status.can_apply {
                                                "Update available"
                                            } else {
                                                "Manual update"
                                            },
                                        )
                                        .into_any_element(),
                                    ),
                                    Some(
                                        format!(
                                            "Jolt {} → {target_version}{}",
                                            status.current_version,
                                            if status.can_apply {
                                                " · device offline"
                                            } else {
                                                " · unmanaged install"
                                            }
                                        )
                                        .into(),
                                    ),
                                )
                            }
                        } else {
                            (
                                None,
                                status
                                    .error
                                    .map(|error| format!("Update check failed: {error}").into()),
                            )
                        }
                    } else {
                        (None, None)
                    };

                widgets::card_row(&theme, ix == 0)
                    .child(tile)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(&theme, device.name.clone()))
                            .child(widgets::meta_line(&theme, meta))
                            .when_some(update_detail, |el, detail| {
                                el.child(
                                    div()
                                        .mt(px(3.0))
                                        .text_size(px(11.0))
                                        .text_color(theme.text_muted)
                                        .child(detail),
                                )
                            }),
                    )
                    .when(is_local, |el| {
                        el.child(widgets::badge(&theme, "This device"))
                    })
                    .when_some(update_action, |el, action| el.child(action))
                    .child(
                        // `opacity-70 hover:opacity-100` (jolt: also rises on
                        // row hover — gpui has no group-hover, so the button's
                        // own hover carries the reveal).
                        widgets::ghost_action(&theme)
                            .id(("device-rename", ix))
                            .opacity(0.7)
                            .hover(|s| {
                                s.opacity(1.0)
                                    .bg(crate::theme::ink(0.06))
                                    .text_color(theme.text)
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_rename(rename_id.clone(), rename_name.clone(), cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::PENCIL)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Rename")),
                    )
                    .child(
                        widgets::ghost_action(&theme)
                            .id(("device-remove", ix))
                            .opacity(0.7)
                            .text_color(theme.danger)
                            .hover(|style| {
                                style
                                    .opacity(1.0)
                                    .bg(theme.danger.opacity(0.08))
                                    .text_color(theme.danger)
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.rename = None;
                                this.delete_confirm = Some(delete_id.clone());
                                cx.notify();
                            }))
                            .child(
                                crate::icons::icon(crate::icons::TRASH)
                                    .size(px(14.0))
                                    .text_color(theme.danger),
                            )
                            .child(SharedString::from("Remove")),
                    )
                    .into_any_element()
            })
            .collect();

        let card = widgets::section_card(&theme);
        let card = if rows.is_empty() {
            card.child(
                div()
                    .px(px(20.0))
                    .py(px(40.0))
                    .text_center()
                    .text_size(px(14.0))
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from("No devices registered")),
            )
        } else {
            card.children(rows)
        };

        div()
            .id("devices-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(
                        &theme,
                        "Devices",
                        (count > 0).then_some(count),
                    ))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Manage availability, versions, updates, names, and synced metadata.",
                    ))
                    .when_some(background_service_card, |el, card| el.child(card))
                    .when_some(self.error.clone(), |el, message| {
                        el.child(
                            widgets::error_strip(&theme, message)
                                .id("devices-error")
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .child(card),
            )
            .when_some(rename_dialog, |element, dialog| element.child(dialog))
            .when_some(delete_dialog, |element, dialog| element.child(dialog))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn presence_window() {
        let now = Utc::now();
        assert!(device_online(Some(now - TimeDelta::seconds(10)), now));
        assert!(device_online(Some(now - TimeDelta::seconds(70)), now));
        assert!(!device_online(Some(now - TimeDelta::seconds(71)), now));
        assert!(!device_online(None, now));
        // Clock skew (future) counts as online.
        assert!(device_online(Some(now + TimeDelta::seconds(30)), now));
    }

    #[test]
    fn local_device_is_online_without_fresh_presence() {
        let now = Utc::now();
        assert!(device_row_online("local", Some("local"), None, now));
        assert!(!device_row_online("remote", Some("local"), None, now));
    }

    #[test]
    fn last_seen_formatting() {
        let now = Utc::now();
        assert_eq!(format_last_seen(None, now), "never seen");
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::seconds(30)), now),
            "just now"
        );
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::minutes(5)), now),
            "5m ago"
        );
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::hours(3)), now),
            "3h ago"
        );
        assert_eq!(
            format_last_seen(Some(now - TimeDelta::days(2)), now),
            "2d ago"
        );
    }
}
