//! Attachments, slash commands, mentions, and message history.

use super::*;

impl Composer {
    pub(super) fn staged(&self) -> &[StagedAttachment] {
        self.attachments
            .get(&self.current_key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub(super) fn add_staged(&mut self, staged: Vec<StagedAttachment>, cx: &mut Context<Self>) {
        if staged.is_empty() {
            return;
        }
        self.attachments
            .entry(self.current_key.clone())
            .or_default()
            .extend(staged);
        cx.notify();
    }

    /// Stage image files (picker / drop / pasted paths). Non-images are
    /// skipped silently; read failures and oversize files surface in the
    /// failure notice.
    pub(crate) fn add_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let mut staged = Vec::new();
        for path in &paths {
            if attachments::format_by_extension(path).is_none() {
                continue;
            }
            match attachments::stage_file(path) {
                Ok(att) => staged.push(att),
                Err(message) => {
                    self.failure = Some(message.into());
                    cx.notify();
                }
            }
        }
        self.add_staged(staged, cx);
    }

    pub(super) fn remove_attachment(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(list) = self.attachments.get_mut(&self.current_key) {
            list.retain(|a| a.id != id);
            if list.is_empty() {
                self.attachments.remove(&self.current_key);
            }
        }
        cx.notify();
    }

    /// Drop a deleted chat's per-chat composer state — staged attachments hold
    /// raw image bytes, and a deleted chat's stage could never be sent again.
    pub fn purge_chat(&mut self, chat_id: &str) {
        self.attachments.remove(chat_id);
        self.extracted_answer_stash.remove(chat_id);
        if self
            .extracted_answers
            .as_ref()
            .is_some_and(|flow| flow.chat_id == chat_id)
        {
            self.extracted_answers = None;
            self.extraction_task = None;
        }
    }

    /// The staged-thumbnail strip: wrapping 56px rounded thumbnails, a remove
    /// button
    /// revealed on hover, click opens the full-size preview.
    pub(super) fn render_attachment_strip(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        let staged = self.staged();
        if staged.is_empty() {
            return None;
        }
        let mut strip = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(STRIP_GAP))
            .px(px(STRIP_PAD_X))
            .pt(px(STRIP_PAD_TOP));
        for (ix, att) in staged.iter().enumerate() {
            let group: SharedString = format!("composer-att-{}", att.id).into();
            let preview = attachments::PreviewImage {
                name: att.name.clone().into(),
                image: att.image.clone(),
            };
            let remove_id = att.id.clone();
            strip = strip.child(
                div()
                    .group(group.clone())
                    .relative()
                    .child(
                        div()
                            .id(("composer-att-thumb", ix))
                            .size(px(STRIP_THUMB))
                            .rounded(px(8.0))
                            .overflow_hidden()
                            .border_1()
                            .border_color(crate::theme::hairline(0.10))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.preview = Some(preview.clone());
                                cx.notify();
                            }))
                            .child(
                                img(att.image.clone())
                                    .size_full()
                                    .object_fit(ObjectFit::Cover),
                            ),
                    )
                    .child(
                        div()
                            .id(("composer-att-remove", ix))
                            .absolute()
                            .top(px(-6.0))
                            .right(px(-6.0))
                            .size(px(18.0))
                            .rounded_full()
                            .bg(theme.bg)
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .shadow_sm()
                            .opacity(0.0)
                            .group_hover(group, |s| s.opacity(1.0))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_attachment(&remove_id, cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::CIRCLE_X)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            ),
                    ),
            );
        }
        Some(strip)
    }

    /// Paperclip action: open the native multi-image picker.
    pub(super) fn open_file_picker(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        self.picker_task = Some(cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update(cx, |composer, cx| composer.add_paths(paths, cx))
                    .ok();
            }
        }));
    }

    pub(super) fn command_rpc_context(&self, cx: &App) -> Option<(ListCommands, CommandCacheKey)> {
        let resolved = self.pickers.read(cx).resolved(cx);
        let harness = resolved.harness?;
        let selected_worktree = match self.pickers.read(cx).checkout_plan() {
            crate::pickers::CheckoutPlan::ReuseWorktree { path, .. } => Some(path),
            _ => None,
        };
        let state = self.state.read(cx);
        let (cwd, target_device) = if let Some(chat) = state.selected_chat_row() {
            let cwd = chat
                .cwd
                .clone()
                .or_else(|| state.selected_space_row().map(|space| space.path.clone()))?;
            (cwd, chat.device_id.clone())
        } else {
            let space = state.selected_space_row()?;
            (
                selected_worktree.unwrap_or_else(|| space.path.clone()),
                space.device_id.clone(),
            )
        };
        let cache_key = CommandCacheKey {
            harness,
            target_device: target_device.clone(),
            cwd: cwd.clone(),
            model_options: command_model_options_key(&resolved.model_options),
        };
        Some((
            ListCommands {
                harness,
                cwd: Some(cwd),
                model_options: resolved.model_options,
                target_device_id: Some(target_device),
            },
            cache_key,
        ))
    }

    pub(super) fn reset_command(&mut self, cx: &mut Context<Self>) {
        self.command.request = self.command.request.wrapping_add(1);
        self.command_task = None;
        self.command = SlashCommandState {
            request: self.command.request,
            ..SlashCommandState::default()
        };
        self.command_scroll.set_offset(Point::default());
        self.sync_mention_controls(cx);
    }

    pub(super) fn update_command_completion(
        &mut self,
        text: &str,
        cursor: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let token = slash_command_token(text, cursor);
        let still_dismissed = token.as_ref().is_some_and(|token| {
            self.command
                .dismissed
                .as_ref()
                .is_some_and(|(range, value)| {
                    token.range == *range && text.get(range.clone()) == Some(value.as_str())
                })
        });
        if still_dismissed {
            self.command.token = None;
            self.sync_mention_controls(cx);
            return false;
        }
        self.command.dismissed = None;
        let Some(token) = token else {
            self.command.token = None;
            self.command.results.clear();
            self.command.active = None;
            self.command.error = None;
            self.command.notice = None;
            self.sync_mention_controls(cx);
            return false;
        };
        let opening = self.command.token.is_none();
        let query_changed = self
            .command
            .token
            .as_ref()
            .is_none_or(|previous| previous.query != token.query);
        let Some((params, cache_key)) = self.command_rpc_context(cx) else {
            self.command.request = self.command.request.wrapping_add(1);
            self.command_task = None;
            self.command.token = Some(token);
            self.command.results.clear();
            self.command.active = None;
            self.command.loading = false;
            self.command.error = None;
            self.command.notice = None;
            self.command.cache_key = None;
            self.sync_mention_controls(cx);
            return true;
        };
        if self.command.cache_key.as_ref() != Some(&cache_key) {
            self.command.request = self.command.request.wrapping_add(1);
            self.command_task = None;
            self.command.loading = false;
            self.command.error = None;
            self.command.notice = None;
            self.command.cache_key = Some(cache_key.clone());
            self.command_scroll.set_offset(Point::default());
        } else if opening {
            self.command_scroll.set_offset(Point::default());
        }
        self.command.token = Some(token.clone());

        let now = Instant::now();
        let snapshot = self.command_cache.get_mut(&cache_key).map(|entry| {
            entry.last_used = now;
            (
                entry.catalog.clone(),
                entry.fetched_at,
                entry.failed_at,
                entry.error.clone(),
            )
        });
        let (catalog, fetched_at, failed_at, cached_error) = snapshot.unwrap_or_default();
        self.command.results = filtered_commands(&catalog, &token.query);
        self.command.active = (!self.command.results.is_empty()).then_some(0);
        if query_changed {
            self.command_scroll.set_offset(Point::default());
        }

        let should_fetch =
            self.command_task.is_none() && command_cache_should_fetch(fetched_at, failed_at, now);
        let has_cached_catalog = fetched_at.is_some();
        self.command.error = (!has_cached_catalog)
            .then_some(cached_error.clone())
            .flatten();
        self.command.notice = if has_cached_catalog && cached_error.is_some() {
            Some("Couldn't refresh — showing cached commands".into())
        } else if has_cached_catalog && should_fetch {
            Some("Refreshing commands…".into())
        } else {
            None
        };
        self.command.loading = should_fetch;
        if !should_fetch {
            self.sync_mention_controls(cx);
            return true;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.command.loading = false;
            if has_cached_catalog {
                self.command.notice = Some("Offline — showing cached commands".into());
            } else {
                self.command.error = Some("Engine not connected".into());
            }
            self.sync_mention_controls(cx);
            return true;
        };

        self.command.request = self.command.request.wrapping_add(1);
        let request = self.command.request;
        self.command_task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(engine.client(), &params).await;
            this.update(cx, |composer, cx| {
                if composer.command.request != request
                    || composer.command.cache_key.as_ref() != Some(&cache_key)
                {
                    return;
                }
                composer.command_task = None;
                composer.command.loading = false;
                let now = Instant::now();
                let entry = composer
                    .command_cache
                    .entry(cache_key.clone())
                    .or_insert_with(|| CommandCacheEntry::empty(now));
                entry.last_used = now;
                match result {
                    Ok(catalog) => {
                        entry.catalog = catalog;
                        entry.fetched_at = Some(now);
                        entry.failed_at = None;
                        entry.error = None;
                    }
                    Err(error) => {
                        let message: SharedString = match error {
                            RpcError::UnknownMethod(_) => {
                                "Update the thread's device to use slash commands".into()
                            }
                            RpcError::Transport(_) | RpcError::Closed => {
                                "The thread's device is unreachable".into()
                            }
                            RpcError::BadParams(_) | RpcError::Failed(_) => {
                                "Couldn't load slash commands".into()
                            }
                        };
                        entry.failed_at = Some(now);
                        entry.error = Some(message);
                    }
                }
                let catalog = entry.catalog.clone();
                let fetched = entry.fetched_at.is_some();
                let cached_error = entry.error.clone();
                prune_command_cache(&mut composer.command_cache);

                let active_name = composer
                    .command
                    .active
                    .and_then(|active| composer.command.results.get(active))
                    .map(|command| command.name.clone());
                if let Some(token) = &composer.command.token {
                    composer.command.results = filtered_commands(&catalog, &token.query);
                    composer.command.active = active_name
                        .and_then(|name| {
                            composer
                                .command
                                .results
                                .iter()
                                .position(|command| command.name == name)
                        })
                        .or_else(|| (!composer.command.results.is_empty()).then_some(0));
                    if let Some(active) = composer.command.active {
                        composer.command_scroll.scroll_to_item(active);
                    }
                }
                composer.command.error = (!fetched).then_some(cached_error.clone()).flatten();
                composer.command.notice = if fetched && cached_error.is_some() {
                    Some("Couldn't refresh — showing cached commands".into())
                } else {
                    None
                };
                composer.sync_mention_controls(cx);
                cx.notify();
            })
            .ok();
        }));
        self.sync_mention_controls(cx);
        true
    }

    pub(super) fn sync_mention_controls(&mut self, cx: &mut Context<Self>) {
        let command_open = self.command.token.is_some();
        let open = command_open || self.mention.token.is_some();
        let has_selection = if command_open {
            self.command.active.is_some()
        } else {
            self.mention.active.is_some()
        };
        self.input.update(cx, |input, cx| {
            input.set_mention_controls(open, has_selection, cx)
        });
    }

    /// Tear down the entire completion lifecycle. Advancing the generation is
    /// important even when the spawned task is dropped: an RPC response may
    /// already be queued for delivery on the UI executor.
    pub(super) fn reset_mention(
        &mut self,
        dismissed: Option<(Range<usize>, String)>,
        cx: &mut Context<Self>,
    ) {
        let request = self.mention.request.wrapping_add(1);
        self.mention_task = None;
        self.mention = FileMentionState {
            request,
            dismissed,
            ..FileMentionState::default()
        };
        self.sync_mention_controls(cx);
    }

    pub(super) fn leave_message_history_after_edit(&mut self, cx: &App) {
        let Some(expected) = self.message_history_text.as_deref() else {
            return;
        };
        if self.input.read(cx).text() != expected {
            self.message_history_position = None;
            self.message_history_text = None;
            self.message_history_draft = None;
        }
    }

    pub(super) fn navigate_message_history(
        &mut self,
        direction: MessageHistoryDirection,
        cx: &mut Context<Self>,
    ) {
        let can_navigate_history = {
            let input = self.input.read(cx);
            can_navigate_message_history(self.message_history_position, input.text())
        };
        if self.wizard.is_some() || self.extracted_answers.is_some() || !can_navigate_history {
            let direction = match direction {
                MessageHistoryDirection::Older => -1.0,
                MessageHistoryDirection::Newer => 1.0,
            };
            self.input
                .update(cx, |input, cx| input.move_vertically(direction, cx));
            return;
        }

        let history = {
            let state = self.state.read(cx);
            user_message_history(&state.transcript, state.pending_echoes())
        };
        let next =
            message_history_position(self.message_history_position, history.len(), direction);
        if next == self.message_history_position {
            return;
        }
        if self.message_history_position.is_none() && next.is_some() {
            self.message_history_draft = Some(self.input.read(cx).text().to_string());
        }
        let draft = self.message_history_draft.as_deref().unwrap_or_default();
        let text = message_history_text(&history, next, draft);
        if next.is_none() {
            self.message_history_draft = None;
        }
        self.message_history_position = next;
        self.message_history_text = next.map(|_| text.clone());
        self.input.update(cx, |input, cx| input.set_text(text, cx));
    }

    pub(super) fn on_input_edited(&mut self, cx: &mut Context<Self>) {
        if self.wizard.is_some() {
            if self.mention.token.is_some() || self.mention_task.is_some() {
                self.reset_mention(None, cx);
            }
            return;
        }
        let (text, cursor) = {
            let input = self.input.read(cx);
            (input.text().to_string(), input.cursor_offset())
        };
        if self.update_command_completion(&text, cursor, cx) {
            if self.mention.token.is_some() || self.mention_task.is_some() {
                self.reset_mention(None, cx);
            }
            cx.notify();
            return;
        }
        let token = mention_token(&text, cursor);
        let still_dismissed = token.as_ref().is_some_and(|token| {
            self.mention
                .dismissed
                .as_ref()
                .is_some_and(|(range, value)| {
                    token.range == *range && text.get(range.clone()) == Some(value.as_str())
                })
        });
        if still_dismissed {
            self.mention.token = None;
            self.mention_task = None;
            self.sync_mention_controls(cx);
            cx.notify();
            return;
        }
        self.mention.dismissed = None;
        if token == self.mention.token {
            self.sync_mention_controls(cx);
            cx.notify();
            return;
        }
        self.mention.request = self.mention.request.wrapping_add(1);
        self.mention_task = None;
        // Refining an open menu keeps the stale rows visible until the new
        // response lands — clearing here made the popup bounce through the
        // skeleton (and a different height) on every keystroke.
        let refining = self.mention.token.is_some() && token.is_some();
        self.mention.token = token.clone();
        if !refining {
            self.mention.results.clear();
            self.mention.active = None;
        }
        self.mention.error = None;
        self.mention.loading = token.is_some();
        self.sync_mention_controls(cx);
        let Some(token) = token else {
            cx.notify();
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.mention.loading = false;
            cx.notify();
            return;
        };
        let selected_worktree = match self.pickers.read(cx).checkout_plan() {
            crate::pickers::CheckoutPlan::ReuseWorktree { path, .. } => Some(path),
            _ => None,
        };
        let request = {
            let state = self.state.read(cx);
            if let Some(chat) = state.selected_chat_row() {
                Some(SearchFiles {
                    query: token.query.clone(),
                    chat_id: Some(chat.id.clone()),
                    space_id: None,
                    path: None,
                    target_device_id: Some(chat.device_id.clone()),
                })
            } else {
                state.selected_space_row().map(|space| SearchFiles {
                    query: token.query.clone(),
                    chat_id: None,
                    space_id: Some(space.id.clone()),
                    path: selected_worktree,
                    target_device_id: Some(space.device_id.clone()),
                })
            }
        };
        let Some(request_params) = request else {
            self.mention.loading = false;
            cx.notify();
            return;
        };
        let request = self.mention.request;
        self.mention_task = Some(cx.spawn(async move |this, cx| {
            // A short debounce prevents one full workspace walk per keystroke
            // during normal typing. The generation check below still guards
            // requests that were already in flight when the query changed.
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            let mut result = call_api(engine.client(), &request_params).await;
            if matches!(result, Err(RpcError::Transport(_)) | Err(RpcError::Closed)) {
                // One retry rides out a cold relay dial to the host device
                // (the diffs pane retries forever; a keystroke-scoped search
                // gets a single second chance).
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                result = call_api(engine.client(), &request_params).await;
            }
            this.update(cx, |composer, cx| {
                if !mention_response_is_current(&composer.mention, request) {
                    return;
                }
                composer.mention.loading = false;
                match result {
                    Ok(results) => {
                        composer.mention.error = None;
                        composer.mention.active = (!results.is_empty()).then_some(0);
                        composer.mention.results = results;
                    }
                    Err(err) => {
                        tracing::warn!(%err, "file mention search failed");
                        composer.mention.results.clear();
                        composer.mention.active = None;
                        composer.mention.error = Some(mention_error_message(&err));
                    }
                }
                composer.sync_mention_controls(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub(super) fn move_mention(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.command.token.is_some() {
            self.command.active =
                crate::popover::menu_step(self.command.active, self.command.results.len(), delta);
            if let Some(active) = self.command.active {
                self.command_scroll.scroll_to_item(active);
            }
            self.sync_mention_controls(cx);
            cx.notify();
            return;
        }
        self.mention.active =
            crate::popover::menu_step(self.mention.active, self.mention.results.len(), delta);
        self.sync_mention_controls(cx);
        cx.notify();
    }

    pub(super) fn dismiss_mention(&mut self, cx: &mut Context<Self>) {
        if let Some(token) = self.command.token.clone() {
            self.command.dismissed = self
                .input
                .read(cx)
                .text()
                .get(token.range.clone())
                .map(|text| (token.range, text.to_string()));
            self.command.token = None;
            self.command.results.clear();
            self.command.active = None;
            self.sync_mention_controls(cx);
            cx.notify();
            return;
        }
        let dismissed = self.mention.token.as_ref().and_then(|token| {
            self.input
                .read(cx)
                .text()
                .get(token.range.clone())
                .map(|text| (token.range.clone(), text.to_string()))
        });
        self.reset_mention(dismissed, cx);
        cx.notify();
    }

    pub(super) fn accept_mention(&mut self, cx: &mut Context<Self>) {
        if let Some(token) = self.command.token.clone() {
            let Some(name) = self
                .command
                .active
                .and_then(|active| self.command.results.get(active))
                .map(|command| command.name.clone())
            else {
                return;
            };
            self.input.update(cx, |input, cx| {
                input.replace_command(token.range, &name, cx)
            });
            self.reset_command(cx);
            cx.notify();
            return;
        }
        let Some(token) = self.mention.token.clone() else {
            return;
        };
        let Some((path, is_dir)) = self
            .mention
            .active
            .and_then(|active| self.mention.results.get(active))
            .map(|result| (result.path.clone(), result.is_dir))
        else {
            return;
        };
        self.input.update(cx, |input, cx| {
            input.replace_mention(token.range, &path, is_dir, cx)
        });
        self.reset_mention(None, cx);
        cx.notify();
    }

    pub(super) fn render_command_popup(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let token = self.command.token.as_ref()?;
        let mut card = crate::popover::popover_card(theme)
            .w(px(420.0))
            .max_h(px(320.0))
            .overflow_hidden()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.dismiss_mention(cx)));
        if self.command.loading && self.command.results.is_empty() {
            card = card.child(crate::popover::skeleton_rows(
                "slash-command-loading",
                theme,
                3,
                cx.entity_id(),
                cx,
            ));
        } else if let Some(error) = self.command.error.clone() {
            card = card.child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .text_size(px(12.0))
                    .text_color(theme.danger_muted)
                    .child(error),
            );
        } else if self.command.results.is_empty() {
            card = card.child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(if token.query.is_empty() {
                        "No commands available"
                    } else {
                        "No matching commands"
                    }),
            );
        } else {
            let mut rows = div()
                .id("slash-command-scroll")
                .max_h(px(280.0))
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .track_scroll(&self.command_scroll);
            for (ix, command) in self.command.results.iter().enumerate() {
                let selected = self.command.active == Some(ix);
                rows = rows.child(
                    crate::popover::menu_row(theme, selected, format!("slash-command-{ix}"))
                        .id(("slash-command", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.command.active = Some(ix);
                            this.accept_mention(cx);
                        }))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .child(
                                    div()
                                        .text_size(px(12.5))
                                        .text_color(theme.text)
                                        .child(format!("/{}", command.name)),
                                )
                                .children(command.description.as_ref().map(|description| {
                                    div()
                                        .overflow_hidden()
                                        .truncate()
                                        .text_size(px(11.0))
                                        .text_color(theme.text_muted)
                                        .child(description.clone())
                                })),
                        ),
                );
            }
            card = card.child(rows);
        }
        if let Some(notice) = self.command.notice.clone() {
            card = card.child(
                div()
                    .flex_none()
                    .border_t_1()
                    .border_color(crate::theme::hairline(0.06))
                    .px(px(12.0))
                    .py(px(6.0))
                    .text_size(px(10.5))
                    .text_color(theme.text_muted)
                    .child(notice),
            );
        }
        let anchor = self
            .input
            .read(cx)
            .visible_point_for_index(token.range.start)?;
        Some(crate::popover::anchored_menu_above_at(
            "slash-command-popup",
            anchor,
            card.into_any_element(),
        ))
    }

    pub(super) fn render_file_mention_popup(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let token = self.mention.token.as_ref()?;
        let mut card = crate::popover::popover_card(theme)
            .w(px(380.0))
            .max_h(px(280.0))
            .overflow_hidden()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.dismiss_mention(cx)));
        if self.mention.loading && self.mention.results.is_empty() {
            card = card.child(crate::popover::skeleton_rows(
                "file-mention-loading",
                theme,
                3,
                cx.entity_id(),
                cx,
            ));
        } else if let Some(error) = self.mention.error.clone() {
            card = card.child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .text_size(px(12.0))
                    .text_color(theme.danger_muted)
                    .child(error),
            );
        } else if self.mention.results.is_empty() {
            card = card.child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(if token.query.is_empty() {
                        "No files available"
                    } else {
                        "No matching files"
                    }),
            );
        } else {
            for (ix, result) in self.mention.results.iter().enumerate() {
                let selected = self.mention.active == Some(ix);
                let path = result.path.clone();
                let tooltip_path: SharedString = path.clone().into();
                card = card.child(
                    crate::popover::menu_row(theme, selected, format!("file-mention-result-{ix}"))
                        .id(("file-mention-result", ix))
                        .tooltip(move |_, cx| {
                            cx.new(|_| MentionPathTooltip {
                                path: tooltip_path.clone(),
                                activation: ix as u64,
                            })
                            .into()
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.mention.active = Some(ix);
                            this.accept_mention(cx);
                        }))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    crate::icons::icon(if result.is_dir {
                                        crate::icons::FOLDER
                                    } else {
                                        crate::icons::FILE
                                    })
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .flex_1()
                                        .overflow_hidden()
                                        .truncate()
                                        .text_size(px(12.5))
                                        .text_color(theme.text)
                                        .child(path),
                                ),
                        ),
                );
            }
        }
        let anchor = self
            .input
            .read(cx)
            .visible_point_for_index(token.range.start)?;
        Some(crate::popover::anchored_menu_above_at(
            "file-mention-popup",
            anchor,
            card.into_any_element(),
        ))
    }

    pub(super) fn render_input_with_completion(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        div()
            .relative()
            .child(self.input.clone())
            .children(self.render_command_popup(theme, cx))
            .children(self.render_file_mention_popup(theme, cx))
    }
}
