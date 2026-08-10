//! Current-session identity in the unified desktop titlebar.
//!
//! Threads are navigated from the sidebar; the header only identifies the
//! selected thread (or the New Thread page) and hosts thread-scoped panel
//! controls.

use super::*;
use jolt_proto::ChatIndicator;

impl Shell {
    pub(super) fn render_session_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let selected = self.state.read(cx).selected_chat.clone();
        let (title, location, offline, status) = {
            let state = self.state.read(cx);
            if let Some(chat) = selected
                .as_deref()
                .and_then(|id| state.chats.iter().find(|chat| chat.id == id))
            {
                let (location, offline) = state
                    .space_for_chat(chat)
                    .map(|space| {
                        let (device, offline) = state.space_device_tag(space, now);
                        (format!("{} {device}", space.display_name()), offline)
                    })
                    .unwrap_or_else(|| ("Unknown space".into(), false));
                (
                    transcript::single_line(
                        &chat.title.clone().unwrap_or_else(|| "New thread".into()),
                    ),
                    location,
                    offline,
                    Some(state.display_status_for(chat, now)),
                )
            } else {
                let (location, offline) = state
                    .selected_space_row()
                    .map(|space| {
                        let (device, offline) = state.space_device_tag(space, now);
                        (format!("{} {device}", space.display_name()), offline)
                    })
                    .unwrap_or_else(|| ("Choose a space".into(), false));
                ("New thread".into(), location, offline, None)
            }
        };
        let working = status.as_ref() == Some(&ChatIndicator::Working);
        let location_marker = if working {
            loaders::activity_spinner("header-session-working", &theme, 12.0, cx.entity_id(), cx)
                .into_any_element()
        } else {
            icon(icons::FOLDER)
                .size(px(13.0))
                .flex_none()
                .text_color(if offline {
                    theme.warning.opacity(0.8)
                } else {
                    theme.text_muted.opacity(0.6)
                })
                .into_any_element()
        };
        let indicator = match status {
            Some(status) if status != ChatIndicator::Working && status != ChatIndicator::Idle => {
                Some(
                    div()
                        .size(px(6.0))
                        .rounded_full()
                        .bg(spaces::status_dot_color(status, &theme))
                        .into_any_element(),
                )
            }
            _ => None,
        };

        // T3 Code's chat header keeps orientation in one quiet breadcrumb:
        // project first, slash, then the thread title. Jolt's space + device is
        // the project identity; status stays a compact signal beside the title.
        let identity = div()
            .min_w_0()
            .flex_1()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .max_w(px(180.0))
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(if offline {
                        theme.warning.opacity(0.8)
                    } else {
                        theme.text_muted.opacity(0.7)
                    })
                    .child(location_marker)
                    .child(div().min_w_0().truncate().child(location)),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(12.0))
                    .text_color(theme.text_faint.opacity(0.7))
                    .child("/"),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(title),
            )
            .when_some(indicator, |el, indicator| el.child(indicator));

        let has_space = !self.state.read(cx).spaces.is_empty();
        let git = self.space_git_detected(cx);
        let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
        let header_left = (sidebar_now + Theme::SPACE_LG).max(self.title_bar_content_start());
        let inner = div()
            .size_full()
            .flex()
            .items_center()
            .pt(px(Theme::TITLEBAR_TOP_PAD))
            .gap(px(6.0))
            .pl(px(header_left))
            .pr(px(titlebar_right_padding(
                cfg!(target_os = "windows"),
                Theme::SPACE_LG,
            )))
            .child(identity)
            .when(selected.is_some() && has_space, |el| {
                el.child(header_icon_button(
                    "new-session",
                    icons::PLUS,
                    &theme,
                    cx.listener(|this, _, _, cx| this.open_new_session(cx)),
                ))
            })
            .when(git && selected.is_some(), |el| {
                el.child(self.render_vcs_actions_control(cx))
            })
            .child(header_icon_button(
                "toggle-terminal",
                icons::TERMINAL_2,
                &theme,
                cx.listener(|this, _, window, cx| this.toggle_terminal(window, cx)),
            ))
            .when(git, |el| {
                el.child(header_icon_button(
                    "toggle-changes",
                    icons::GIT_BRANCH,
                    &theme,
                    cx.listener(|this, _, _, cx| this.toggle_right_pane(cx)),
                ))
            });

        let bar = div().h(px(Theme::TITLEBAR_HEIGHT)).flex_none().child(inner);
        self.titlebar_drag_region("session-header-titlebar", bar, cx)
            .into_any_element()
    }
}
