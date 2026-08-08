//! Current-session transcript search command center.

use std::ops::Range;

use gpui::{StyledText, TextRun, font};
use jolt_api::{SearchTranscript, call as call_api};
use jolt_session_doc::{MessageRole, TranscriptSearchResult};

use super::*;
use crate::settings::{ShortcutId, display_combo};

const SEARCH_DEBOUNCE_MS: u64 = 120;

#[derive(Clone)]
struct TranscriptSearchMatches {
    query: String,
    rows: Vec<TranscriptSearchResult>,
}

fn begin_transcript_search(results: &mut Loadable<TranscriptSearchMatches>) {
    if results.ready().is_none() {
        *results = Loadable::Loading;
    }
}

pub(super) struct TranscriptSearchFlow {
    search: Entity<ComposerInput>,
    results: Loadable<TranscriptSearchMatches>,
    active: usize,
    focus: FocusHandle,
    list_scroll: gpui::ScrollHandle,
    focus_pending: bool,
    task: Option<Task<()>>,
    _search_events: Subscription,
}

impl Shell {
    pub(super) fn open_transcript_search(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.route, Route::Chat) || self.state.read(cx).selected_chat.is_none() {
            return;
        }
        self.add_space = None;
        self.session_search = None;
        self.spaces_menu = None;
        self.user_menu_open = false;
        let search =
            cx.new(|cx| ComposerInput::with_context("Search transcript…", "PaletteSearch", cx));
        let search_events = cx.subscribe(&search, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                if let Some(flow) = this.transcript_search.as_mut() {
                    flow.active = 0;
                    flow.list_scroll.set_offset(gpui::Point::default());
                }
                this.request_transcript_search(cx);
            }
        });
        self.transcript_search = Some(TranscriptSearchFlow {
            search,
            results: Loadable::Idle,
            active: 0,
            focus: cx.focus_handle(),
            list_scroll: gpui::ScrollHandle::new(),
            focus_pending: true,
            task: None,
            _search_events: search_events,
        });
        cx.notify();
    }

    fn request_transcript_search(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = self.transcript_search.as_mut() else {
            return;
        };
        flow.task = None;
        let query = flow.search.read(cx).text().trim().to_string();
        if query.is_empty() {
            flow.results = Loadable::Idle;
            cx.notify();
            return;
        }
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            self.transcript_search = None;
            cx.notify();
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            flow.results = Loadable::Error("Engine not connected".into());
            cx.notify();
            return;
        };

        // Keep useful rows mounted while the replacement request runs. This
        // avoids flashing back to the empty "Searching…" state on every edit.
        begin_transcript_search(&mut flow.results);
        let request_query = query.clone();
        let request_chat = chat_id.clone();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(SEARCH_DEBOUNCE_MS))
                .await;
            let result = call_api(
                engine.client(),
                &SearchTranscript {
                    chat_id: request_chat,
                    query: request_query,
                    target_device_id: None,
                },
            )
            .await;
            this.update(cx, |shell, cx| {
                let Some(flow) = shell.transcript_search.as_mut() else {
                    return;
                };
                let current_query = flow.search.read(cx).text().trim().to_string();
                if current_query != query
                    || shell.state.read(cx).selected_chat.as_deref() != Some(chat_id.as_str())
                {
                    return;
                }
                flow.task = None;
                flow.results = match result {
                    Ok(rows) => Loadable::Ready(TranscriptSearchMatches { query, rows }),
                    Err(error) => {
                        tracing::warn!(%chat_id, %error, "transcript search failed");
                        Loadable::Error("Search is unavailable".into())
                    }
                };
                cx.notify();
            })
            .ok();
        });
        if let Some(flow) = self.transcript_search.as_mut() {
            flow.task = Some(task);
        }
        cx.notify();
    }

    fn activate_transcript_search(&mut self, cx: &mut Context<Self>) {
        let result = self.transcript_search.as_ref().and_then(|flow| {
            flow.results
                .ready()
                .and_then(|results| results.rows.get(flow.active))
                .cloned()
        });
        let Some(result) = result else {
            return;
        };
        self.transcript_search = None;
        self.transcript.update(cx, |transcript, cx| {
            transcript.scroll_to_message(result.message_id, result.page_id, cx);
        });
        cx.notify();
    }

    fn transcript_search_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            popover::MenuKey::Escape => {
                self.transcript_search = None;
                cx.notify();
            }
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let count = self
                    .transcript_search
                    .as_ref()
                    .and_then(|flow| flow.results.ready())
                    .map_or(0, |results| results.rows.len());
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                if let Some(flow) = self.transcript_search.as_mut() {
                    flow.active = popover::menu_step(Some(flow.active), count, delta).unwrap_or(0);
                    flow.list_scroll.scroll_to_item(flow.active);
                    cx.notify();
                }
            }
            popover::MenuKey::Enter | popover::MenuKey::ModEnter => {
                self.activate_transcript_search(cx)
            }
            popover::MenuKey::Backspace | popover::MenuKey::Other => {}
        }
    }

    pub(super) fn render_transcript_search_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        {
            let flow = self.transcript_search.as_mut()?;
            if std::mem::take(&mut flow.focus_pending) {
                window.focus(&flow.search.focus_handle(cx), cx);
            }
        }
        let (search, results, active, focus, list_scroll) = {
            let flow = self.transcript_search.as_ref()?;
            (
                flow.search.clone(),
                flow.results.clone(),
                flow.active,
                flow.focus.clone(),
                flow.list_scroll.clone(),
            )
        };
        let shortcut: SharedString =
            display_combo(self.settings.keymap.get(ShortcutId::SearchTranscript)).into();
        let hairline = crate::theme::hairline(0.06);
        let band = popover::band();
        let key_chip = |theme: &Theme| {
            div()
                .h(px(22.0))
                .px(px(6.0))
                .rounded(px(5.0))
                .flex_none()
                .flex()
                .items_center()
                .bg(crate::theme::ink(0.05))
                .text_size(px(11.0))
                .font_family(theme.font_mono.clone())
                .text_color(theme.text_muted.opacity(0.7))
        };
        let input_row = div()
            .h(px(46.0))
            .flex_none()
            .pl(px(12.0))
            .pr(px(10.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .bg(band)
            .border_b_1()
            .border_color(hairline)
            .child(key_chip(&theme).child(shortcut))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(14.0))
                    .child(search.into_any_element()),
            )
            .child(
                key_chip(&theme)
                    .id("transcript-search-esc")
                    .cursor_pointer()
                    .hover(|style| style.bg(crate::theme::ink(0.09)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.transcript_search = None;
                        cx.notify();
                    }))
                    .child(SharedString::from("esc")),
            );

        let result_count = results.ready().map_or(0, |results| results.rows.len());
        let list = match results {
            Loadable::Idle => transcript_search_message(&theme, "Type to search this transcript"),
            Loadable::Loading => transcript_search_message(&theme, "Searching…"),
            Loadable::Error(message) => {
                let retry = popover::btn_ghost(&theme, "Retry", "transcript-search-retry")
                    .id("transcript-search-retry")
                    .on_click(cx.listener(|this, _, _, cx| this.request_transcript_search(cx)));
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(8.0))
                    .text_size(px(12.5))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(message))
                    .child(retry)
                    .into_any_element()
            }
            Loadable::Ready(results) if results.rows.is_empty() => {
                transcript_search_message(&theme, "No matches")
            }
            Loadable::Ready(results) => {
                let query = results.query;
                div()
                    .id("transcript-search-results")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&list_scroll)
                    .px(px(8.0))
                    .py(px(6.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .children(results.rows.into_iter().enumerate().map(|(index, result)| {
                        let message_id = result.message_id.clone();
                        let page_id = result.page_id.clone();
                        let role = match result.role {
                            MessageRole::User => "You",
                            MessageRole::Assistant => "Assistant",
                            MessageRole::System => "System",
                        };
                        let timestamp =
                            transcript::format_timestamp(result.created_at, &chrono::Local);
                        let preview = highlighted_preview(&result.preview, &query, &theme);
                        popover::menu_row_nav(
                            &theme,
                            false,
                            index == active.min(result_count.saturating_sub(1)),
                            format!("transcript-search-row-{index}"),
                        )
                        .id(("transcript-search-row", index))
                        .h(px(52.0))
                        .when(
                            index == active.min(result_count.saturating_sub(1)),
                            |element| element.shadow(crate::theme::card_selected_shadows()),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.transcript_search = None;
                            this.transcript.update(cx, |transcript, cx| {
                                transcript.scroll_to_message(
                                    message_id.clone(),
                                    page_id.clone(),
                                    cx,
                                );
                            });
                            cx.notify();
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(3.0))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .justify_between()
                                        .text_size(px(10.5))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(theme.text_muted.opacity(0.6))
                                        .child(SharedString::from(role))
                                        .child(SharedString::from(timestamp)),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(13.0))
                                        .text_color(theme.text)
                                        .child(preview),
                                ),
                        )
                    }))
                    .into_any_element()
            }
        };

        let footer = div()
            .flex_none()
            .bg(band)
            .border_t_1()
            .border_color(hairline)
            .px(px(12.0))
            .py(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .child(popover::key_hint_pair(
                        &theme,
                        icons::ARROW_UP,
                        icons::ARROW_DOWN,
                        "Navigate",
                    ))
                    .child(popover::key_hint(&theme, icons::CORNER_DOWN_LEFT, "Open")),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.text_muted.opacity(0.5))
                    .child(SharedString::from(match result_count {
                        0 => String::new(),
                        1 => "1 result".into(),
                        count => format!("{count} results"),
                    })),
            );

        let card = div()
            .id("transcript-search-palette")
            .w(px(620.0))
            .rounded(px(14.0))
            .border_1()
            .border_color(crate::theme::hairline(0.10))
            .bg(if theme.is_glass() {
                theme.glass_overlay()
            } else {
                theme.surface_overlay
            })
            .shadow_lg()
            .overflow_hidden()
            .flex()
            .flex_col()
            .text_color(theme.text)
            .track_focus(&focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                this.transcript_search_key(event, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.transcript_search = None;
                cx.notify();
            }))
            .child(input_row)
            .child(div().h(px(330.0)).flex().flex_col().child(list))
            .child(footer)
            .into_any_element();
        Some(popover::modal("transcript-search-dialog", viewport, card))
    }
}

fn highlighted_preview(preview: &str, query: &str, theme: &Theme) -> StyledText {
    let ranges = transcript_match_ranges(preview, query);
    let mut runs = Vec::with_capacity(ranges.len().saturating_mul(2).saturating_add(1));
    let mut offset = 0;
    let base_font = font(theme.font_sans.clone());
    let mut push_run = |len: usize, highlighted: bool| {
        if len == 0 {
            return;
        }
        let mut run_font = base_font.clone();
        if highlighted {
            run_font.weight = gpui::FontWeight::SEMIBOLD;
        }
        runs.push(TextRun {
            len,
            font: run_font,
            color: theme.text,
            background_color: highlighted.then_some(theme.warning.opacity(0.22)),
            underline: None,
            strikethrough: None,
        });
    };
    for range in ranges {
        push_run(range.start.saturating_sub(offset), false);
        push_run(range.end.saturating_sub(range.start), true);
        offset = range.end;
    }
    push_run(preview.len().saturating_sub(offset), false);
    StyledText::new(preview.to_string()).with_runs(runs)
}

fn transcript_match_ranges(text: &str, query: &str) -> Vec<Range<usize>> {
    let mut normalized = String::new();
    let mut source_ranges = Vec::new();
    for (start, character) in text.char_indices() {
        let end = start + character.len_utf8();
        let lowercase = character.to_lowercase().to_string();
        normalized.push_str(&lowercase);
        source_ranges.extend(std::iter::repeat_n(start..end, lowercase.len()));
    }

    let mut ranges = Vec::new();
    for term in query.split_whitespace().map(str::to_lowercase) {
        if term.is_empty() {
            continue;
        }
        let mut offset = 0;
        while let Some(relative) = normalized[offset..].find(&term) {
            let start = offset + relative;
            let end = start + term.len();
            if let (Some(first), Some(last)) = (
                source_ranges.get(start),
                source_ranges.get(end.saturating_sub(1)),
            ) {
                ranges.push(first.start..last.end);
            }
            offset = end;
        }
    }
    ranges.sort_unstable_by_key(|range| (range.start, range.end));
    ranges
        .into_iter()
        .fold(Vec::<Range<usize>>::new(), |mut merged, range| {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                _ => merged.push(range),
            }
            merged
        })
}

fn transcript_search_message(theme: &Theme, message: &'static str) -> AnyElement {
    div()
        .flex_1()
        .px(px(14.0))
        .py(px(18.0))
        .text_size(px(12.5))
        .text_color(theme.text_faint)
        .child(SharedString::from(message))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beginning_search_preserves_existing_results() {
        let mut results = Loadable::Ready(TranscriptSearchMatches {
            query: "old query".into(),
            rows: Vec::new(),
        });
        begin_transcript_search(&mut results);
        assert!(matches!(
            results,
            Loadable::Ready(TranscriptSearchMatches { ref query, .. }) if query == "old query"
        ));

        let mut empty = Loadable::Idle;
        begin_transcript_search(&mut empty);
        assert!(empty.is_loading());
    }

    #[test]
    fn transcript_matches_are_case_insensitive_and_cover_each_term() {
        let text = "Alpha beta ALPHA";
        let ranges = transcript_match_ranges(text, "alpha BETA");
        assert_eq!(ranges, vec![0..5, 6..10, 11..16]);
    }

    #[test]
    fn transcript_matches_preserve_unicode_source_ranges() {
        let text = "CAFÉ and café";
        let ranges = transcript_match_ranges(text, "café");
        assert_eq!(
            ranges
                .into_iter()
                .map(|range| &text[range])
                .collect::<Vec<_>>(),
            vec!["CAFÉ", "café"]
        );
    }
}
