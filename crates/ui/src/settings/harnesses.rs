//! Settings → Harnesses: device-local CLI versions, release checks, and
//! user-approved updates.

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, Context, Entity, Render, SharedString, Subscription, Task, div, prelude::*, px,
};
use jolt_api::{CheckHarnessUpdates, call as call_api};
use jolt_proto::{HarnessId, HarnessUpdateState, HarnessUpdateStatus};

use super::device_switcher::{DeviceSelected, DeviceSwitcher};
use super::devices::device_row_online;
use crate::state::{AppState, format_time_ago};
use crate::theme::Theme;
use crate::{icons, settings::widgets};

const HARNESSES: [(HarnessId, &str); 3] = [
    (HarnessId::ClaudeCode, "Claude Code"),
    (HarnessId::Codex, "Codex"),
    (HarnessId::Pi, "Pi"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusTone {
    Neutral,
    Accent,
    Success,
    Warning,
    Danger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowAction {
    None,
    Update,
    Badge(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RowPresentation {
    label: String,
    detail: Option<String>,
    tone: StatusTone,
    action: RowAction,
}

pub struct HarnessesPage {
    state: Entity<AppState>,
    /// `None` addresses the connected engine; remote ids relay-forward.
    target_device: Option<String>,
    device_switcher: Entity<DeviceSwitcher>,
    refreshing: bool,
    error: Option<SharedString>,
    refresh_task: Option<Task<()>>,
    _observe: Subscription,
    _device_switcher_events: Subscription,
    _ticker: Task<()>,
}

impl HarnessesPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let device_switcher = cx.new(|cx| DeviceSwitcher::new(state.clone(), cx));
        let device_switcher_events = cx.subscribe(
            &device_switcher,
            |this: &mut Self, _, DeviceSelected(target), cx| {
                this.target_device = target.clone();
                this.refreshing = false;
                this.error = None;
                cx.notify();
            },
        );
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(30))
                    .await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        Self {
            state,
            target_device: None,
            device_switcher,
            refreshing: false,
            error: None,
            refresh_task: None,
            _observe: observe,
            _device_switcher_events: device_switcher_events,
            _ticker: ticker,
        }
    }

    fn target_online(&self, now: DateTime<Utc>, cx: &gpui::App) -> bool {
        let Some(target) = self.target_device.as_deref() else {
            return true;
        };
        let state = self.state.read(cx);
        state
            .devices
            .iter()
            .find(|device| device.id == target)
            .is_some_and(|device| {
                device_row_online(
                    &device.id,
                    state.local_device_id.as_deref(),
                    device.last_seen_at,
                    now,
                )
            })
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refreshing || !self.target_online(Utc::now(), cx) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let target = self.target_device.clone();
        self.refreshing = true;
        self.error = None;
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(
                engine.client(),
                &CheckHarnessUpdates {
                    target_device_id: target.clone(),
                },
            )
            .await;
            this.update(cx, |page, cx| {
                if page.target_device != target {
                    return;
                }
                page.refreshing = false;
                if let Err(error) = result {
                    page.error = Some(format!("Refresh failed: {error}").into());
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

impl Render for HarnessesPage {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let online = self.target_online(now, cx);
        let statuses = {
            let state = self.state.read(cx);
            match self.target_device.as_deref() {
                Some(device_id) => state
                    .remote_harness_updates
                    .get(device_id)
                    .cloned()
                    .unwrap_or_default(),
                None => state.harness_updates.clone(),
            }
        };
        let checking = self.refreshing
            || statuses
                .iter()
                .any(|status| status.state == HarnessUpdateState::Checking);
        let checked = checked_label(&statuses, checking, now);

        let rows: Vec<AnyElement> = HARNESSES
            .into_iter()
            .enumerate()
            .map(|(index, (harness, name))| {
                let status = statuses.iter().find(|status| status.harness == harness);
                let (pending, request_failed) = {
                    let state = self.state.read(cx);
                    (
                        state.harness_update_pending(self.target_device.as_deref(), harness),
                        state.harness_update_failed(self.target_device.as_deref(), harness),
                    )
                };
                let presentation = row_presentation(status, online, pending, request_failed);
                let color = tone_color(presentation.tone, &theme);
                let version = status
                    .and_then(|status| status.current_version.as_deref())
                    .map(|version| format!("v{version}"));
                let (mark, mark_color) = harness_mark(harness, &theme);
                let tile = div()
                    .flex_none()
                    .relative()
                    .size(px(36.0))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(crate::theme::ink(0.03))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icons::icon(mark).size(px(17.0)).text_color(mark_color))
                    .child(
                        div()
                            .absolute()
                            .bottom(px(-3.0))
                            .right(px(-3.0))
                            .size(px(9.0))
                            .rounded_full()
                            .border_2()
                            .border_color(theme.surface)
                            .bg(color),
                    );
                let action = match presentation.action {
                    RowAction::None => None,
                    RowAction::Badge(label) => {
                        Some(widgets::badge(&theme, label).into_any_element())
                    }
                    RowAction::Update => {
                        let state = self.state.clone();
                        let target = self.target_device.clone();
                        Some(
                            widgets::ghost_action(&theme)
                                .id(format!("harness-{index}-update"))
                                .text_color(theme.warning_muted)
                                .hover(|style| {
                                    style
                                        .bg(theme.warning.opacity(0.08))
                                        .text_color(theme.warning_muted)
                                })
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    state.update(cx, |state, cx| {
                                        state.begin_harness_update(harness, target.clone(), cx);
                                    });
                                }))
                                .child(SharedString::from("Update"))
                                .into_any_element(),
                        )
                    }
                };

                widgets::card_row(&theme, index == 0)
                    .child(tile)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(widgets::row_title(&theme, name))
                                    .when_some(version, |row, version| {
                                        row.child(
                                            div()
                                                .font_family(theme.font_mono.clone())
                                                .text_size(px(11.5))
                                                .text_color(theme.text_muted.opacity(0.7))
                                                .child(version),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .mt(px(4.0))
                                    .text_size(px(11.5))
                                    .text_color(color)
                                    .child(presentation.label),
                            )
                            .when_some(presentation.detail, |column, detail| {
                                column.child(
                                    div()
                                        .mt(px(2.0))
                                        .text_size(px(10.5))
                                        .text_color(theme.text_muted.opacity(0.7))
                                        .child(detail),
                                )
                            }),
                    )
                    .when_some(action, |row, action| row.child(action))
                    .into_any_element()
            })
            .collect();

        div()
            .id("harnesses-settings-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .child(widgets::page_header(&theme, "Harnesses", None))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(11.5))
                                    .text_color(theme.text_muted.opacity(0.65))
                                    .child(checked),
                            )
                            .child(
                                widgets::ghost_action(&theme)
                                    .id("harnesses-refresh")
                                    .flex_none()
                                    .hover(|style| widgets::ghost_hover(&theme, style))
                                    .when(checking || !online, |button| button.opacity(0.5))
                                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)))
                                    .child(
                                        icons::icon(icons::REFRESH)
                                            .size(px(16.0))
                                            .text_color(theme.text_muted),
                                    )
                                    .child(SharedString::from("Refresh")),
                            )
                            .child(self.device_switcher.clone()),
                    )
                    .child(widgets::page_subtitle(
                        &theme,
                        "Check installed coding harnesses and apply updates on each device.",
                    ))
                    .when_some(self.error.clone(), |column, message| {
                        column.child(
                            widgets::error_strip(&theme, message)
                                .id("harnesses-error")
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .child(widgets::section_card(&theme).children(rows))
                    .child(
                        div()
                            .mt(px(16.0))
                            .px(px(4.0))
                            .text_size(px(12.0))
                            .line_height(px(18.0))
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from(
                                "Checks run automatically in the background. Updates start only when you choose Update.",
                            )),
                    ),
            )
    }
}

fn checked_label(statuses: &[HarnessUpdateStatus], checking: bool, now: DateTime<Utc>) -> String {
    if checking {
        return "Checking…".into();
    }
    let Some(checked_at) = statuses
        .iter()
        .filter_map(|status| status.checked_at)
        .min()
        .and_then(DateTime::<Utc>::from_timestamp_millis)
    else {
        return "Not checked yet".into();
    };
    let relative = format_time_ago(checked_at, now);
    if relative == "now" {
        "Checked just now".into()
    } else {
        format!("Checked {relative} ago")
    }
}

fn harness_mark(harness: HarnessId, theme: &Theme) -> (&'static str, gpui::Hsla) {
    match harness {
        HarnessId::ClaudeCode => (icons::CLAUDE_MARK, icons::claude_brand()),
        HarnessId::Codex => (icons::OPENAI_MARK, theme.text),
        HarnessId::Pi => (icons::PI_MARK, theme.text_muted),
        HarnessId::Mock => (icons::TERMINAL_2, theme.text_muted),
    }
}

fn tone_color(tone: StatusTone, theme: &Theme) -> gpui::Hsla {
    match tone {
        StatusTone::Neutral => crate::theme::ink(0.3),
        StatusTone::Accent => theme.accent,
        StatusTone::Success => theme.success,
        StatusTone::Warning => theme.warning_muted,
        StatusTone::Danger => theme.danger_muted,
    }
}

fn row_presentation(
    status: Option<&HarnessUpdateStatus>,
    online: bool,
    pending: bool,
    request_failed: bool,
) -> RowPresentation {
    if request_failed {
        return RowPresentation {
            label: "Update request failed".into(),
            detail: Some("Refresh the device status and try again.".into()),
            tone: StatusTone::Danger,
            action: RowAction::Badge("Failed"),
        };
    }
    if pending {
        return RowPresentation {
            label: "Starting update…".into(),
            detail: Some("Sending the update request to this device.".into()),
            tone: StatusTone::Warning,
            action: RowAction::Badge("Starting…"),
        };
    }
    let Some(status) = status else {
        return RowPresentation {
            label: if online {
                "Waiting for status…".into()
            } else {
                "Device is offline".into()
            },
            detail: None,
            tone: StatusTone::Neutral,
            action: if online {
                RowAction::None
            } else {
                RowAction::Badge("Offline")
            },
        };
    };

    match status.state {
        HarnessUpdateState::Unknown => RowPresentation {
            label: "Waiting for status…".into(),
            detail: status.detail.clone(),
            tone: StatusTone::Neutral,
            action: RowAction::None,
        },
        HarnessUpdateState::Checking => RowPresentation {
            label: "Checking for updates…".into(),
            detail: status.detail.clone(),
            tone: StatusTone::Accent,
            action: RowAction::Badge("Checking…"),
        },
        HarnessUpdateState::UpToDate => RowPresentation {
            label: "Up to date".into(),
            detail: status.detail.clone(),
            tone: StatusTone::Success,
            action: RowAction::None,
        },
        HarnessUpdateState::UpdateAvailable => RowPresentation {
            label: status.latest_version.as_ref().map_or_else(
                || "Update available".into(),
                |latest| format!("Version {latest} is available"),
            ),
            detail: (!online)
                .then(|| "Reconnect this device to apply the update.".into())
                .or_else(|| status.detail.clone()),
            tone: StatusTone::Warning,
            action: if !online {
                RowAction::Badge("Offline")
            } else if status.can_apply {
                RowAction::Update
            } else {
                RowAction::Badge("Manual update")
            },
        },
        HarnessUpdateState::WaitingForIdle => RowPresentation {
            label: "Waiting for active work…".into(),
            detail: status.detail.clone(),
            tone: StatusTone::Warning,
            action: RowAction::Badge("Waiting…"),
        },
        HarnessUpdateState::Updating => RowPresentation {
            label: "Updating…".into(),
            detail: status.detail.clone(),
            tone: StatusTone::Warning,
            action: RowAction::Badge("Updating…"),
        },
        HarnessUpdateState::Updated => RowPresentation {
            label: "Update complete".into(),
            detail: status.detail.clone(),
            tone: StatusTone::Success,
            action: RowAction::Badge("Updated"),
        },
        HarnessUpdateState::Failed => RowPresentation {
            label: if status.latest_version.is_some() {
                "Update failed".into()
            } else {
                "Update check failed".into()
            },
            detail: status.detail.clone(),
            tone: StatusTone::Danger,
            action: RowAction::Badge("Failed"),
        },
        HarnessUpdateState::NotInstalled => RowPresentation {
            label: "Not installed".into(),
            detail: status
                .detail
                .clone()
                .or_else(|| Some("The CLI was not found on this device.".into())),
            tone: StatusTone::Danger,
            action: RowAction::Badge("Unavailable"),
        },
        HarnessUpdateState::Manual => RowPresentation {
            label: status.latest_version.as_ref().map_or_else(
                || "Manual update required".into(),
                |latest| format!("Version {latest} requires a manual update"),
            ),
            detail: status
                .detail
                .clone()
                .or_else(|| Some("Update this installation directly on the device.".into())),
            tone: StatusTone::Warning,
            action: RowAction::Badge("Manual update"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: HarnessUpdateState) -> HarnessUpdateStatus {
        HarnessUpdateStatus {
            harness: HarnessId::Codex,
            state,
            current_version: Some("1.0.0".into()),
            latest_version: Some("2.0.0".into()),
            can_apply: true,
            checked_at: None,
            detail: None,
        }
    }

    #[test]
    fn managed_update_requires_an_online_device() {
        let update = status(HarnessUpdateState::UpdateAvailable);
        assert_eq!(
            row_presentation(Some(&update), true, false, false).action,
            RowAction::Update
        );
        assert_eq!(
            row_presentation(Some(&update), false, false, false).action,
            RowAction::Badge("Offline")
        );
    }

    #[test]
    fn request_failure_overrides_streamed_status() {
        let current = status(HarnessUpdateState::UpToDate);
        let presentation = row_presentation(Some(&current), true, false, true);
        assert_eq!(presentation.label, "Update request failed");
        assert_eq!(presentation.tone, StatusTone::Danger);
    }

    #[test]
    fn checked_label_uses_the_oldest_status_in_the_completed_sweep() {
        let now = DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap();
        let mut first = status(HarnessUpdateState::UpToDate);
        first.checked_at = Some((now.timestamp() - 120) * 1_000);
        let mut second = status(HarnessUpdateState::UpToDate);
        second.checked_at = Some((now.timestamp() - 60) * 1_000);
        assert_eq!(
            checked_label(&[first, second], false, now),
            "Checked 2m ago"
        );
        assert_eq!(checked_label(&[], true, now), "Checking…");
    }
}
