//! Main pane sizing, composition, and gate rendering.

use super::*;

impl Shell {
    pub(super) fn resize_handle<T>(
        &self,
        id: &'static str,
        marker: fn() -> T,
        reset: fn(&mut Shell, &mut Context<Shell>),
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div>
    where
        T: 'static,
    {
        let hover = Theme::of(cx).border_strong;
        div()
            .id(id)
            .w(px(5.0))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            .hover(move |s| s.bg(hover))
            .on_drag(marker(), |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        reset(this, cx);
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            )
    }

    pub(super) fn render_main(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme_owned = Theme::of(cx).clone();
        let theme = &theme_owned;
        let view = cx.entity_id();
        let theme_bg = theme.bg;
        let (border, text, faint) = (theme.border, theme.text, theme.text_faint);

        // Secondary routes show only their page outlet; chat-scoped composer,
        // transcript, terminal, and Changes chrome remain mounted on Chat.
        let secondary_outlet = match self.route {
            Route::Settings(section) => Some(self.settings_outlet(section, cx)),
            Route::Chat => None,
        };
        if let Some(outlet) = secondary_outlet {
            return div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0().child(outlet))
                .into_any_element();
        }

        let _ = (text, border);
        let has_selection = self.state.read(cx).selected_chat.is_some();
        let has_spaces = !self.state.read(cx).spaces.is_empty();

        // Content outlet: selected chat → transcript; nothing selected → the
        // "Send a message to start" canvas with a watermark; no spaces at all
        // → the onboarding card. The composer sits below the first two
        // (new-chat mode mints the chat id on first send).
        let outlet: AnyElement = if has_selection {
            self.transcript.clone().into_any_element()
        } else if !has_spaces {
            // Onboarding (first boot / after the destructive wipe): no folders
            // to work in yet — one clear affordance.
            let _ = faint;
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in(
                    "no-spaces-canvas",
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(crate::ascii_mark::ascii_jolt_mark(
                            theme,
                            132.0,
                            crate::ascii_mark::AsciiMarkMotion::Idle,
                            view,
                            cx,
                        ))
                        .child(
                            div()
                                .mt(px(18.0))
                                .text_size(px(16.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(SharedString::from("Add a space to get started")),
                        )
                        .child(
                            div()
                                .mt(px(6.0))
                                .text_size(px(13.0))
                                .text_color(theme.text_muted.opacity(0.7))
                                .child(SharedString::from(
                                    "A space is a folder on one of your devices.",
                                )),
                        )
                        .child(
                            popover::btn_primary(&theme_owned, "Add a space")
                                .id("onboarding-add-space")
                                .mt(px(20.0))
                                .on_click(cx.listener(|this, _, _, cx| this.open_add_space(cx))),
                        ),
                ))
                .into_any_element()
        } else {
            // New-chat canvas: the dim violet Jolt mark over the centered
            // helper line, naming the space the session will start in.
            let space_link = self
                .new_chat_space_picker
                .update(cx, |picker, cx| picker.render_new_chat_space_link(cx));
            let helper: AnyElement = if let Some(space_link) = space_link {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child("Send a message to start a thread in ")
                    .child(space_link)
                    .child(".")
                    .into_any_element()
            } else {
                div()
                    .child("Send a message to start a new thread.")
                    .into_any_element()
            };
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in(
                    "new-chat-canvas",
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(crate::ascii_mark::ascii_jolt_mark(
                            theme,
                            132.0,
                            crate::ascii_mark::AsciiMarkMotion::Idle,
                            view,
                            cx,
                        ))
                        .child(
                            div()
                                .mt(px(18.0))
                                .text_size(px(14.0))
                                .text_color(theme.text_muted.opacity(0.6))
                                .child(helper),
                        ),
                ))
                .into_any_element()
        };

        let status = self.status_strip.clone();
        // File dropzone over the ENTIRE conversation column (transcript +
        // composer, not just the pill): dragging OS files anywhere across the
        // chat area shows the "Drop images to attach" veil; a drop stages the
        // files in the composer. `has_active_drag` gates the veil so a drag
        // that left the window (FileDrop Exited) can't strand it.
        let file_drag_active = self.file_drag_active && cx.has_active_drag();
        div()
            .id("chat-dropzone")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .on_drag_move::<gpui::ExternalPaths>(cx.listener(
                |this, e: &gpui::DragMoveEvent<gpui::ExternalPaths>, _, cx| {
                    let inside = e.bounds.contains(&e.event.position);
                    if this.file_drag_active != inside {
                        this.file_drag_active = inside;
                        cx.notify();
                    }
                },
            ))
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                this.file_drag_active = false;
                let paths = paths.paths().to_vec();
                this.composer
                    .update(cx, |composer, cx| composer.add_paths(paths, cx));
                cx.notify();
            }))
            .child(
                // The conversation fades out at its bottom edge instead of
                // hard-cutting against the composer — a gradient overlay from
                // transparent into the panel background.
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(outlet)
                    .child(
                        div()
                            .absolute()
                            .bottom_0()
                            .left_0()
                            .right(px(10.0))
                            .h(px(Theme::TRANSCRIPT_FADE_BAND))
                            .bg(gpui::linear_gradient(
                                0.0,
                                gpui::linear_color_stop(theme_bg, 0.0),
                                gpui::linear_color_stop(theme_bg.opacity(0.0), 1.0),
                            )),
                    )
                    .child(self.jump_to_bottom.clone()),
            )
            // Reserved status strip (h-6) — the WorkingIndicator lives here so
            // the composer below never shifts. Both live INSIDE the
            // conversation region, above the terminal dock.
            .child(status)
            .when(has_spaces, |el| el.child(self.composer.clone()))
            .child(self.render_terminal_container(cx))
            .when(file_drag_active, |el| {
                el.child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(theme.scrim().opacity(0.4 / 0.6))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .child("Drop images to attach"),
                )
            })
            .into_any_element()
    }

    /// Terminal panel dock at the main-column bottom: a 5px height-drag handle
    /// over the panel, the whole container height-animated 200 ms on toggle.
    pub(super) fn render_terminal_container(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let target = self.terminal_target(cx);
        let tween = self.terminal_tween;
        if target <= 0.0 && tween.is_none() {
            return gpui::Empty.into_any_element();
        }
        // Defensive: an open flag needs its entity (and set_open) even if
        // toggle_terminal never created one.
        if self.terminal_open(cx) && self.terminal.is_none() {
            let panel = self.terminal_panel(cx);
            panel.update(cx, |panel, cx| panel.set_open(true, cx));
        }
        let Some(panel) = self.terminal.clone() else {
            return gpui::Empty.into_any_element();
        };
        let border = Theme::of(cx).border;
        let handle_hover = Theme::of(cx).border_strong;
        let height = self.settings.terminal_height;

        let handle = div()
            .id("terminal-resize")
            .h(px(5.0))
            .w_full()
            .flex_none()
            .cursor_row_resize()
            .hover(move |s| s.bg(handle_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, _| {
                    this.terminal_drag_anchor =
                        Some((f32::from(event.position.y), this.settings.terminal_height));
                }),
            )
            .on_drag(TerminalResize, |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        this.settings.terminal_height = TERMINAL_DEFAULT_HEIGHT;
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            );

        // Fixed-height inner clipped by the animated container: content never
        // reflows mid-transition (same trick as the side panes).
        let inner = div()
            .h(px(height))
            .w_full()
            .flex()
            .flex_col()
            .child(handle)
            .child(div().flex_1().min_h_0().child(panel));

        div()
            .w_full()
            .flex_none()
            .overflow_hidden()
            .border_t_1()
            .border_color(border)
            .h(px(self.eval_tween(tween, target)))
            .child(inner)
            .into_any_element()
    }

    pub(super) fn render_expanded_changes(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let changes = self.changes_pane(cx);
        changes.update(cx, |changes, cx| changes.ensure_watch(cx));
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .mx(px(8.0))
            .mb(px(8.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.bg)
            .overflow_hidden()
            .child(changes)
            .into_any_element()
    }

    pub(super) fn render_expanded_terminal(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let terminal = self.terminal_panel(cx);
        terminal.update(cx, |terminal, cx| terminal.set_open(true, cx));
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .mx(px(8.0))
            .mb(px(8.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.bg)
            .overflow_hidden()
            .child(terminal)
            .into_any_element()
    }

    /// Right "Changes" pane — hidden by default, drag-resizable; content is the
    /// lazy [`Changes`] diff viewer (created on first open).
    pub(super) fn render_right_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let bg = theme.bg;
        let content: AnyElement = if self.right_pane_open(cx) {
            let changes = self.changes_pane(cx);
            // Idempotent — also covers a persisted-open pane on boot.
            changes.update(cx, |changes, cx| changes.ensure_watch(cx));
            changes.into_any_element()
        } else {
            gpui::Empty.into_any_element()
        };
        // Its OWN inset card (user request): the conversation card's right
        // gutter is the gap; padding (not margins) keeps the tweened width
        // container clean, and the resize grabber floats over the gap.
        let handle = self
            .resize_handle(
                "right-pane-resize",
                || RightPaneResize,
                |shell, _| shell.settings.right_pane_width = RIGHT_PANE_DEFAULT,
                cx,
            )
            .absolute()
            .top_0()
            .bottom_0()
            // INSIDE the width-clipped container (a negative inset was
            // clipped into unreachability — user-reported dead resize),
            // overlapping the card's left border.
            .left(px(0.0));
        let card = div()
            .size_full()
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .bg(bg)
            .overflow_hidden()
            .child(content);
        let target = self.right_target(cx);
        self.pane_container(
            self.right_tween,
            target,
            // Mirrors the conversation card's box exactly: flush under the
            // titlebar (no top pad), 8px bottom/right gutters — the
            // conversation card's own right margin is the 8px gap between the
            // two insets (user-reported height/gap mismatch).
            div()
                .h_full()
                .relative()
                .pb(px(8.0))
                .pr(px(8.0))
                .child(card)
                .child(handle)
                .into_any_element(),
        )
    }

    pub(super) fn render_gate_card(
        &mut self,
        phase: &GatePhase,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let GatePhase::Failed(error) = phase else {
            unreachable!("only failed gates render the failure card");
        };
        // Backend unreachable: quiet centered copy plus a Retry affordance.
        let content = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(Theme::SPACE_MD))
            .child(
                div()
                    .text_size(px(14.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(error.clone())),
            )
            .child(
                div()
                    .id("retry-engine")
                    .px(px(12.0))
                    .py(px(6.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border)
                    .text_size(px(13.0))
                    .text_color(theme.text)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.glass_hover()))
                    .on_click(cx.listener(|this, _, _, cx| this.retry_engine(cx)))
                    .child(SharedString::from("Retry")),
            );
        div()
            .size_full()
            .relative()
            .bg(theme.bg)
            .child(grid_backdrop(&theme))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    // Keyed per phase so every gate swap replays the 0.5s
                    // entrance instead of mutating one animated element.
                    .child(motion::fade_in("gate-card-failed", div().child(content))),
            )
            .into_any_element()
    }

    /// Automatic first-sign-in setup. The organization is an internal tenancy
    /// detail, so the UI only reports progress or a retryable failure.
    pub(super) fn render_org_gate(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.ensure_org_ui(cx);
        let theme = Theme::of(cx).clone();
        let Some(org) = self.org.as_ref() else {
            return Empty.into_any_element();
        };
        let error = org.error.clone();
        let card = div()
            .w(px(400.0))
            .px(px(32.0))
            .py(px(36.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_card)
            .shadow_lg()
            .flex()
            .flex_col()
            .child(
                icon(icons::JOLT_LOGO)
                    .size(px(28.0))
                    .text_color(theme.code_text),
            )
            .child(
                div()
                    .mt(px(20.0))
                    .text_size(px(18.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from("Setting up Jolt")),
            )
            .child(div().mt(px(10.0)).flex().items_center().gap(px(8.0)).when(
                error.is_none(),
                |el| {
                    el.child(loaders::activity_spinner(
                        "account-setup-indicator",
                        &theme,
                        14.0,
                        cx.entity_id(),
                        cx,
                    ))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from("Finishing account setup…")),
                    )
                },
            ))
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .mt(px(10.0))
                        .text_size(px(12.0))
                        .line_height(px(17.0))
                        .text_color(theme.danger_muted)
                        .child(message),
                )
                .child(
                    div()
                        .id("account-setup-retry")
                        .mt(px(16.0))
                        .h(px(36.0))
                        .px(px(16.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .bg(theme.text)
                        .text_size(px(14.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.on_solid)
                        .cursor_pointer()
                        .hover(|s| s.opacity(0.9))
                        .on_click(cx.listener(|this, _, _, cx| this.provision_personal_org(cx)))
                        .child(SharedString::from("Retry")),
                )
            })
            .child(
                div().mt(px(24.0)).child(
                    div()
                        .id("org-signout")
                        .text_size(px(12.0))
                        .text_color(theme.text_muted.opacity(0.6))
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.text))
                        .on_click(cx.listener(|this, _, _, cx| this.sign_out(cx)))
                        .child(SharedString::from("Use a different account")),
                ),
            );

        div()
            .size_full()
            .relative()
            .bg(theme.bg)
            .child(grid_backdrop(&theme))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(motion::fade_in("org-gate-card", card)),
            )
            .into_any_element()
    }
}
