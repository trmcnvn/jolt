//! Archived sessions page: searchable archived chats across devices, with
//! Unarchive (`Mutate setChatArchived false`).

use gpui::{
    AnyElement, App, Context, Entity, ListAlignment, ListState, SharedString, Subscription, Task,
    Window, div, list, prelude::*, px,
};

use jolt_proto::Chat;
use jolt_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::state::AppState;
use crate::theme::Theme;

const ROW_HEIGHT: f32 = 58.0;

/// Archived rows in sidebar (recency) order, filtered by title. Pure.
pub fn archived_chats<'a>(chats: &'a [Chat], query: &str) -> Vec<&'a Chat> {
    let query = query.trim().to_lowercase();
    chats
        .iter()
        .filter(|chat| {
            chat.archived
                && (query.is_empty()
                    || chat
                        .title
                        .as_deref()
                        .unwrap_or("Untitled session")
                        .to_lowercase()
                        .contains(&query))
        })
        .collect()
}

pub struct ArchivedPage {
    state: Entity<AppState>,
    search: Entity<ComposerInput>,
    rows: Vec<Chat>,
    list: ListState,
    error: Option<SharedString>,
    /// Chat with an in-flight unarchive (button shows working state).
    busy: Option<String>,
    /// Chat id under the pointer, used to reveal the Unarchive action.
    hovered: Option<String>,
    task: Option<Task<()>>,
    _observe: Subscription,
    _search_events: Subscription,
}

impl ArchivedPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            ComposerInput::with_context("Search archived sessions…", "PaletteSearch", cx)
        });
        let observe = cx.observe(&state, |this: &mut Self, _, cx| {
            this.sync_rows(cx);
            cx.notify();
        });
        let search_events = cx.subscribe(&search, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                this.sync_rows(cx);
                cx.notify();
            }
        });
        let mut page = Self {
            state,
            search,
            rows: Vec::new(),
            list: ListState::new(0, ListAlignment::Top, px(ROW_HEIGHT * 2.0)),
            error: None,
            busy: None,
            hovered: None,
            task: None,
            _observe: observe,
            _search_events: search_events,
        };
        page.sync_rows(cx);
        page
    }

    fn sync_rows(&mut self, cx: &App) {
        let query = self.search.read(cx).text();
        let rows: Vec<Chat> = archived_chats(&self.state.read(cx).chats, query)
            .into_iter()
            .cloned()
            .collect();
        let ids_changed = self.rows.len() != rows.len()
            || self
                .rows
                .iter()
                .zip(&rows)
                .any(|(old, new)| old.id != new.id);
        self.rows = rows;
        if ids_changed {
            self.list
                .reset_with_uniform_height(self.rows.len(), px(ROW_HEIGHT));
        }
    }

    fn unarchive(&mut self, chat_id: String, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.busy = Some(chat_id.clone());
        self.error = None;
        let params = serde_json::json!({
            "op": "setChatArchived",
            "chatId": chat_id,
            "archived": false,
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::MUTATE, params).await;
            this.update(cx, |page, cx| {
                page.busy = None;
                if let Err(err) = result {
                    page.error = Some(format!("Unarchive failed: {err}").into());
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_row(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(chat) = self.rows.get(index).cloned() else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        let title: SharedString = chat
            .title
            .clone()
            .unwrap_or_else(|| "Untitled session".into())
            .into();
        let device: Option<SharedString> = self
            .state
            .read(cx)
            .device_name(&chat.device_id)
            .map(|name| name.to_string().into());
        let time_ago: SharedString = crate::state::format_time_ago(
            chat.last_message_at.unwrap_or(chat.created_at),
            chrono::Utc::now(),
        )
        .into();
        let location: Option<SharedString> = crate::state::chat_location(&chat).map(Into::into);
        let is_busy = self.busy.as_deref() == Some(chat.id.as_str());
        let row_hovered = self.hovered.as_deref() == Some(chat.id.as_str());
        let chat_id = chat.id.clone();
        let hover_id = chat.id.clone();

        div()
            .h(px(ROW_HEIGHT))
            .child(
                div()
                    .id(SharedString::from(format!("archived-row-{}", chat.id)))
                    .h(px(56.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .rounded(px(8.0))
                    .px(px(12.0))
                    .hover(|style| style.bg(crate::theme::ink(0.03)))
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered {
                            this.hovered = Some(hover_id.clone());
                        } else if this.hovered.as_deref() == Some(hover_id.as_str()) {
                            this.hovered = None;
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_none()
                            .size(px(32.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme.border)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                crate::icons::icon(crate::icons::ARCHIVE_MINIMALISTIC)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted.opacity(0.6)),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(px(13.0))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(theme.text)
                                            .child(title),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_size(px(11.0))
                                            .text_color(theme.text_muted.opacity(0.5))
                                            .child(time_ago),
                                    ),
                            )
                            .child({
                                let mut meta = div()
                                    .mt(px(2.0))
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(6.0))
                                    .text_size(px(11.0))
                                    .text_color(theme.text_muted.opacity(0.55));
                                let both = device.is_some() && location.is_some();
                                if let Some(device) = device {
                                    meta = meta.child(device);
                                }
                                if both {
                                    meta = meta.child(SharedString::from("·"));
                                }
                                if let Some(location) = location {
                                    meta = meta.child(div().min_w_0().truncate().child(location));
                                }
                                meta
                            }),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("unarchive-{}", chat.id)))
                            .flex_none()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .px(px(10.0))
                            .py(px(4.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme.border)
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .opacity(if row_hovered || is_busy { 1.0 } else { 0.0 })
                            .when(is_busy, |element| element.opacity(0.4))
                            .cursor_pointer()
                            .hover(|style| style.bg(theme.surface_raised).text_color(theme.text))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.unarchive(chat_id.clone(), cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::ARCHIVE_UP_MINIMALISTIC)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from(if is_busy {
                                "Unarchiving…"
                            } else {
                                "Unarchive"
                            })),
                    ),
            )
            .into_any_element()
    }

    fn empty_state(&self, searching: bool, theme: &Theme) -> AnyElement {
        let (title, subtitle) = if searching {
            (
                "No archived sessions found",
                "Try a different session title.",
            )
        } else {
            (
                "Nothing archived",
                "Archive a session from its sidebar row to hide it here.",
            )
        };
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .text_center()
            .text_color(theme.text_muted.opacity(0.5))
            .child(
                crate::icons::icon(crate::icons::ARCHIVE_MINIMALISTIC)
                    .size(px(28.0))
                    .text_color(theme.text_muted.opacity(0.2)),
            )
            .child(
                div()
                    .mt(px(12.0))
                    .text_size(px(14.0))
                    .child(SharedString::from(title)),
            )
            .child(
                div()
                    .mt(px(4.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.4))
                    .child(SharedString::from(subtitle)),
            )
            .into_any_element()
    }
}

impl Render for ArchivedPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;

        let theme = Theme::of(cx).clone();
        let total = self
            .state
            .read(cx)
            .chats
            .iter()
            .filter(|chat| chat.archived)
            .count();
        let searching = !self.search.read(cx).text().trim().is_empty();
        let body = if self.rows.is_empty() {
            self.empty_state(searching && total > 0, &theme)
        } else {
            list(self.list.clone(), cx.processor(Self::render_row))
                .mt(px(16.0))
                .flex_1()
                .min_h_0()
                .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
                .into_any_element()
        };

        div()
            .id("archived-page")
            .size_full()
            .child(
                widgets::page_column()
                    .h_full()
                    .min_h_0()
                    .pb(px(24.0))
                    .child(widgets::page_header(
                        &theme,
                        "Archived sessions",
                        (total > 0).then_some(total),
                    ))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Hidden from the sidebar, never deleted. Unarchiving puts a session back on its device.",
                    ))
                    .child(
                        div()
                            .mt(px(24.0))
                            .h(px(36.0))
                            .flex_none()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(crate::theme::ink(0.03))
                            .px(px(10.0))
                            .child(
                                crate::icons::icon(crate::icons::MAGNIFER)
                                    .size(px(15.0))
                                    .text_color(theme.text_muted.opacity(0.65)),
                            )
                            .child(div().flex_1().min_w_0().child(self.search.clone())),
                    )
                    .when_some(self.error.clone(), |element, message| {
                        element.child(
                            widgets::error_strip(&theme, message)
                                .id("archived-error")
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .child(body),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn chat(id: &str, title: Option<&str>, archived: bool) -> Chat {
        Chat {
            id: id.into(),
            device_id: "d".into(),
            title: title.map(str::to_string),
            archived,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
            goal: None,
        }
    }

    #[test]
    fn only_archived_rows_show() {
        let chats = vec![
            chat("a", None, false),
            chat("b", None, true),
            chat("c", None, true),
        ];
        let rows = archived_chats(&chats, "");
        let ids: Vec<&str> = rows.iter().map(|chat| chat.id.as_str()).collect();
        assert_eq!(ids, ["b", "c"]);
    }

    #[test]
    fn search_matches_titles_case_insensitively() {
        let chats = vec![
            chat("a", Some("Fix navigation"), true),
            chat("b", Some("Polish composer"), true),
            chat("c", None, true),
        ];
        let rows = archived_chats(&chats, " NAV ");
        assert_eq!(
            rows.iter().map(|chat| chat.id.as_str()).collect::<Vec<_>>(),
            ["a"]
        );
        let rows = archived_chats(&chats, "untitled");
        assert_eq!(
            rows.iter().map(|chat| chat.id.as_str()).collect::<Vec<_>>(),
            ["c"]
        );
    }
}
