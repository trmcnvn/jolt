//! Shell layout behavior.

use super::*;

impl Shell {
    pub(super) fn sidebar_target(&self) -> f32 {
        if self.settings.sidebar_collapsed {
            0.0
        } else {
            self.settings.sidebar_width
        }
    }

    /// Does the selected space's folder have git? Owner-stamped and synced —
    /// gates the Changes pane, its toggle, and Cmd-B with zero RPCs.
    pub(super) fn space_git_detected(&self, cx: &App) -> bool {
        self.state.read(cx).selected_space_git()
    }

    /// The current chat's changes-pane flag (per-session, in-memory), gated on
    /// the space having git at all: a stale per-chat open flag must not reopen
    /// the pane after switching into a non-git space.
    /// The per-session panel key. The new-chat canvas (no selection) keys per
    /// SPACE — one shared "" key made a canvas toggle read as global state
    /// (user report).
    pub(super) fn panel_key(&self, cx: &App) -> String {
        if self.active_chat.is_empty() {
            let space = self
                .state
                .read(cx)
                .selected_space
                .clone()
                .unwrap_or_default();
            format!("space-canvas:{space}")
        } else {
            self.active_chat.clone()
        }
    }

    pub(super) fn right_pane_open(&self, cx: &App) -> bool {
        self.panels.get(&self.panel_key(cx)).changes_open && self.space_git_detected(cx)
    }

    /// The current chat's terminal flag (per-session, in-memory).
    pub(super) fn terminal_open(&self, cx: &App) -> bool {
        self.panels.get(&self.panel_key(cx)).terminal_open
    }

    pub(super) fn right_target(&self, cx: &App) -> f32 {
        if self.right_pane_open(cx) {
            self.settings.right_pane_width
        } else {
            0.0
        }
    }

    pub(super) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        let from = self.sidebar_target();
        self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
        self.sidebar_tween = Some(WidthTween::new(from, self.sidebar_target()));
        self.schedule_save(cx);
        cx.notify();
    }

    /// Expanded panels unmount the sidebar. Drop its FLIP baseline and stale
    /// offsets so remounting establishes the current order without replaying a
    /// completed resort animation.
    pub(super) fn reset_sidebar_resort(&mut self) {
        self.sidebar_prev_order.clear();
        self.clear_sidebar_resort_animation();
    }

    pub(super) fn clear_sidebar_resort_animation(&mut self) {
        self.sidebar_resort.clear();
        self.sidebar_new_keys.clear();
        self.sidebar_resort_task = None;
    }

    pub(super) fn toggle_right_pane(&mut self, cx: &mut Context<Self>) {
        // No git in this space → no diff pane, Cmd-B goes dead.
        if !self.space_git_detected(cx) {
            return;
        }
        let from = self.right_target(cx);
        let key = self.panel_key(cx);
        let open = self.panels.toggle_changes(&key);
        self.right_tween = Some(WidthTween::new(from, self.right_target(cx)));
        if open {
            // Lazy: the Changes entity (and its WatchCheckoutDiffV2) exists only
            // once the pane has been opened.
            let changes = self.changes_pane(cx);
            changes.update(cx, |changes, cx| {
                changes.collapse_all(cx);
                changes.ensure_watch(cx);
            });
        } else if let Some(changes) = self.changes.clone() {
            self.set_changes_expanded(false, cx);
            changes.update(cx, Changes::stop_watch);
        }
        cx.notify();
    }

    pub(super) fn set_changes_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        if expanded {
            self.set_terminal_expanded(false, cx);
        }
        if self.changes_expanded == expanded {
            return;
        }
        self.changes_expanded = expanded;
        if expanded {
            self.reset_sidebar_resort();
        }
        if let Some(changes) = self.changes.clone() {
            changes.update(cx, |changes, cx| changes.set_expanded_view(expanded, cx));
        }
        cx.notify();
    }

    pub(super) fn open_turn_diff(
        &mut self,
        diff: jolt_proto::TurnDiffManifest,
        file_path: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        if self.state.read(cx).selected_chat.as_deref() != Some(diff.chat_id.as_str()) {
            return;
        }
        let from = self.right_target(cx);
        let key = self.panel_key(cx);
        self.panels.open_changes(&key);
        self.right_tween = Some(WidthTween::new(from, self.right_target(cx)));
        let target = (self.state.read(cx).local_device_id.as_deref()
            != Some(diff.device_id.as_str()))
        .then(|| diff.device_id.clone());
        self.changes_pane(cx).update(cx, |changes, cx| {
            changes.show_turn_diff(diff, target, file_path, cx);
        });
        cx.notify();
    }

    pub(super) fn changes_pane(&mut self, cx: &mut Context<Self>) -> Entity<Changes> {
        if let Some(changes) = &self.changes {
            return changes.clone();
        }
        let changes = cx.new(|cx| Changes::new(self.state.clone(), cx));
        changes.update(cx, |changes, cx| {
            changes.set_expanded_view(self.changes_expanded, cx)
        });
        self.changes_sub = Some(cx.subscribe(
            &changes,
            |this: &mut Shell, _, event: &ChangesEvent, cx| match event {
                ChangesEvent::ToggleExpanded => {
                    this.set_changes_expanded(!this.changes_expanded, cx)
                }
                ChangesEvent::SubmitReview {
                    review_id,
                    chat_id,
                    message,
                } => {
                    this.composer.update(cx, |composer, cx| {
                        composer.submit_generated_review(
                            review_id.clone(),
                            chat_id.clone(),
                            message.clone(),
                            cx,
                        );
                    });
                }
            },
        ));
        self.changes = Some(changes.clone());
        changes
    }
    pub(super) fn sync_changes_watch(&mut self, cx: &mut Context<Self>) {
        let on_chat = matches!(self.route, Route::Chat);
        if !on_chat {
            self.set_terminal_expanded(false, cx);
        }
        let visible = on_chat && self.panels.get(&self.panel_key(cx)).changes_open;
        if visible {
            self.changes_pane(cx).update(cx, Changes::ensure_watch);
        } else if let Some(changes) = self.changes.clone() {
            self.set_changes_expanded(false, cx);
            changes.update(cx, Changes::stop_watch);
        }
    }

    pub(super) fn terminal_panel(&mut self, cx: &mut Context<Self>) -> Entity<TerminalPanel> {
        if let Some(terminal) = &self.terminal {
            return terminal.clone();
        }
        let terminal = cx.new(|cx| TerminalPanel::new(self.state.clone(), cx));
        terminal.update(cx, |terminal, cx| {
            terminal.set_expanded_view(self.terminal_expanded, cx)
        });
        self.terminal_panel_sub = Some(cx.subscribe(
            &terminal,
            |this: &mut Shell, _, event: &TerminalPanelEvent, cx| match event {
                TerminalPanelEvent::ChatEmptied(chat_id) => {
                    this.close_terminal_for_exited_chat(chat_id, cx);
                }
                TerminalPanelEvent::ToggleExpanded => {
                    this.set_terminal_expanded(!this.terminal_expanded, cx);
                }
            },
        ));
        self.terminal = Some(terminal.clone());
        terminal
    }

    pub(super) fn set_terminal_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        if expanded {
            self.set_changes_expanded(false, cx);
        }
        if self.terminal_expanded == expanded {
            return;
        }
        self.terminal_expanded = expanded;
        if expanded {
            self.reset_sidebar_resort();
        }
        if let Some(terminal) = self.terminal.clone() {
            terminal.update(cx, |terminal, cx| terminal.set_expanded_view(expanded, cx));
        }
        cx.notify();
    }

    pub(super) fn terminal_target(&self, cx: &App) -> f32 {
        if self.terminal_open(cx) {
            self.settings.terminal_height
        } else {
            0.0
        }
    }

    pub(super) fn close_terminal_for_exited_chat(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let current = self.active_chat == chat_id;
        let from = current.then(|| self.terminal_target(cx));
        if !self.panels.close_terminal(chat_id) {
            return;
        }
        if let Some(from) = from {
            self.set_terminal_expanded(false, cx);
            self.terminal_tween = Some(WidthTween::new(from, 0.0));
            self.schedule_terminal_tween_cleanup(cx);
            cx.notify();
        }
    }

    pub(super) fn schedule_terminal_tween_cleanup(&mut self, cx: &mut Context<Self>) {
        self.terminal_tween_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(RESIZE.total().mul_f32(motion::speed_scale()) + Duration::from_millis(30))
                .await;
            this.update(cx, |shell, cx| {
                shell.terminal_tween = None;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Cmd/Ctrl+` and the header button. Height animates 200 ms; closing detaches
    /// (PTYs stay alive), opening restores. The flag is per chat.
    pub(super) fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.terminal_target(cx);
        let key = self.panel_key(cx);
        let open = self.panels.toggle_terminal(&key);
        self.terminal_tween = Some(WidthTween::new(from, self.terminal_target(cx)));
        if !open {
            self.set_terminal_expanded(false, cx);
        }
        let panel = self.terminal_panel(cx);
        panel.update(cx, |panel, cx| panel.set_open(open, cx));
        if open {
            // Opening lands keyboard focus in the shell so typing goes straight
            // to the prompt with no click needed.
            // The handle is focusable before the panel's first paint; once the
            // terminal body mounts with `track_focus` it receives the keys.
            window.focus(&panel.read(cx).focus_handle(), cx);
        } else {
            // Hiding the panel removes the (likely focused) terminal view;
            // with nothing focused, window key bindings stop dispatching, so
            // hand focus to the composer. Cmd+` is a pure toggle, so a second
            // press closes even while the terminal is focused.
            window.focus(&self.composer.focus_handle(cx), cx);
        }
        self.schedule_terminal_tween_cleanup(cx);
        cx.notify();
    }

    pub(super) fn on_terminal_drag(
        &mut self,
        event: &gpui::DragMoveEvent<TerminalResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((anchor_y, anchor_h)) = self.terminal_drag_anchor else {
            return;
        };
        let dy = anchor_y - f32::from(event.event.position.y);
        let viewport_h = f32::from(window.viewport_size().height);
        self.settings.terminal_height = clamp_terminal_height(anchor_h + dy, viewport_h);
        self.terminal_tween = None; // live drag tracks the pointer
        self.schedule_save(cx);
        cx.notify();
    }

    pub(super) fn on_sidebar_drag(
        &mut self,
        event: &gpui::DragMoveEvent<SidebarResize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let x = f32::from(event.event.position.x);
        self.settings.sidebar_width = x.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        self.settings.sidebar_collapsed = false;
        self.sidebar_tween = None; // live drag tracks the pointer directly
        self.schedule_save(cx);
        cx.notify();
    }

    pub(super) fn on_right_pane_drag(
        &mut self,
        event: &gpui::DragMoveEvent<RightPaneResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = f32::from(window.viewport_size().width);
        let width = viewport - f32::from(event.event.position.x);
        // jolt caps the pane at 52% of the window on top of the absolute range.
        let max = RIGHT_PANE_MAX.min(viewport * 0.52);
        self.settings.right_pane_width = width.clamp(RIGHT_PANE_MIN, max.max(RIGHT_PANE_MIN));
        self.right_tween = None;
        self.schedule_save(cx);
        cx.notify();
    }

    /// Debounced settings write: waits [`SAVE_DEBOUNCE_MS`], then persists the
    /// latest snapshot on the background executor. Re-scheduling drops (cancels)
    /// the previous timer.
    pub(super) fn schedule_save(&mut self, cx: &mut Context<Self>) {
        let dir = self.data_dir.clone();
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(SAVE_DEBOUNCE_MS))
                .await;
            // Re-stamp the appearance from the global before writing. The View
            // menu changes it through `appearance::set_mode`, which never touches
            // this shell's in-memory copy — without this, the next pane resize
            // would quietly write the boot-time appearance back over the user's
            // choice.
            let Ok(snapshot) = this.update(cx, |shell, cx| {
                shell.settings.appearance = crate::appearance::mode(cx);
                let (light_theme, dark_theme) = crate::appearance::theme_ids(cx);
                shell.settings.light_theme = light_theme;
                shell.settings.dark_theme = dark_theme;
                let (ui_font, prompt_font, code_font, terminal_font) =
                    crate::appearance::font_families(cx);
                shell.settings.ui_font = ui_font.to_string();
                shell.settings.prompt_font = prompt_font.to_string();
                shell.settings.code_font = code_font.to_string();
                shell.settings.terminal_font = terminal_font.to_string();
                if let Some(scope) = shell.observed_scope {
                    shell.settings.scope_navigation.insert(
                        scope_key(scope).into(),
                        ScopeNavigation {
                            last_space_id: shell.settings.last_space_id.clone(),
                            space_filter: shell.settings.space_filter.clone(),
                        },
                    );
                }
                shell.settings.clone()
            }) else {
                return;
            };
            cx.background_executor()
                .spawn(async move {
                    if let Err(err) = snapshot.save(&dir) {
                        tracing::warn!(error = %err, "failed to persist ui settings");
                    }
                })
                .await;
        }));
    }

    pub(super) fn retry_engine(&mut self, cx: &mut Context<Self>) {
        AppState::bootstrap(
            self.state.clone(),
            self.boot.clone(),
            self.connector.clone(),
            cx,
        );
    }
}
