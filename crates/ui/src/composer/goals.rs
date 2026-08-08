//! Goal creation, mutation, and rendering.

use super::*;

impl Composer {
    pub(super) fn open_goal_dialog(&mut self, cx: &mut Context<Self>) {
        if self.state.read(cx).selected_chat.is_none()
            && self.state.read(cx).selected_space_row().is_none()
        {
            self.failure = Some("Add a space before creating a goal".into());
            return;
        }
        let existing = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|chat| chat.goal.clone())
            .filter(|goal| goal.status != GoalStatus::Complete);
        let objective = cx.new(|cx| ComposerInput::new("Goal objective", cx));
        let budget = cx.new(|cx| ComposerInput::new("Token budget (optional)", cx));
        if let Some(goal) = &existing {
            objective.update(cx, |input, cx| input.set_text(goal.objective.clone(), cx));
            if let Some(value) = goal.token_budget {
                budget.update(cx, |input, cx| input.set_text(value.to_string(), cx));
            }
        }
        let events = cx.subscribe(&objective, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_goal_dialog(cx);
            }
        });
        self.goal_dialog = Some(GoalDialog {
            objective,
            budget,
            goal_id: existing.as_ref().map(|goal| goal.id.clone()),
            expected_revision: existing.as_ref().map(|goal| goal.revision),
            tokens_used: existing.as_ref().map_or(0, |goal| goal.tokens_used),
            _objective_events: events,
        });
        self.input.update(cx, |input, cx| input.set_text("", cx));
        self.drafts.remove(&self.current_key);
        self.reset_command(cx);
        cx.notify();
    }

    pub(super) fn submit_goal_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.goal_dialog.as_ref() else {
            return;
        };
        let objective = dialog.objective.read(cx).text().trim().to_string();
        if objective.is_empty() {
            self.failure = Some("Goal objective is required".into());
            cx.notify();
            return;
        }
        let budget_text = dialog.budget.read(cx).text().trim().to_string();
        let token_budget = if budget_text.is_empty() {
            None
        } else {
            match budget_text.parse::<u64>() {
                Ok(value) if value > 0 => Some(value),
                _ => {
                    self.failure = Some("Token budget must be a positive integer".into());
                    cx.notify();
                    return;
                }
            }
        };
        if token_budget.is_some_and(|budget| budget <= dialog.tokens_used) {
            self.failure = Some("Token budget must exceed the tokens already used".into());
            cx.notify();
            return;
        }
        let operation = match (&dialog.goal_id, dialog.expected_revision) {
            (Some(goal_id), Some(expected_revision)) => GoalOperation::Edit {
                goal_id: goal_id.clone(),
                expected_revision,
                objective,
                token_budget,
            },
            _ => GoalOperation::Create {
                objective,
                token_budget,
            },
        };
        self.goal_dialog = None;
        self.goal_expanded = true;
        self.queue_goal_operation(operation, cx);
    }

    pub(super) fn queue_goal_operation(
        &mut self,
        operation: GoalOperation,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.failure = Some("Engine not connected".into());
            return;
        };
        let existing = self.state.read(cx).selected_chat_row().cloned();
        let new_space = existing
            .is_none()
            .then(|| self.state.read(cx).selected_space_row().cloned())
            .flatten();
        let checkout_plan = self.pickers.read(cx).checkout_plan();
        let local_device_id = self.state.read(cx).local_device_id.clone();
        let Some((chat_id, device_id, create_chat)) = existing
            .map(|chat| (chat.id, chat.device_id, None))
            .or_else(|| {
                let space = new_space?;
                let chat_id = uuid::Uuid::new_v4().to_string();
                let config = self.pickers.read(cx).resolved(cx).chat_config();
                let remote = local_device_id.as_deref() != Some(space.device_id.as_str());
                Some((
                    chat_id.clone(),
                    space.device_id,
                    Some((chat_id, space.id, config, space.path, checkout_plan, remote)),
                ))
            })
        else {
            self.failure = Some("Add a space before creating a goal".into());
            return;
        };
        if create_chat.is_some() {
            self.state
                .update(cx, |state, cx| state.select_chat(Some(chat_id.clone()), cx));
            cx.emit(ComposerEvent::Sent {
                chat_id: chat_id.clone(),
                new_thread: true,
            });
        }
        cx.spawn(async move |this, cx| {
            let result: Result<(), String> = async {
                if let Some((chat_id, space_id, config, space_path, plan, remote)) = create_chat {
                    let mut cwd = space_path.clone();
                    let branch = match plan {
                        crate::pickers::CheckoutPlan::CurrentCheckout { branch } => branch,
                        crate::pickers::CheckoutPlan::ReuseWorktree { path, branch } => {
                            cwd = path;
                            Some(branch)
                        }
                        crate::pickers::CheckoutPlan::NewWorktree { base: None } => None,
                        crate::pickers::CheckoutPlan::NewWorktree { base: Some(base) } => {
                            let worktree = call_api(
                                engine.client(),
                                &CreateWorktree {
                                    repo_path: space_path.clone(),
                                    branch: base,
                                    target_device_id: remote.then(|| device_id.clone()),
                                },
                            )
                            .await
                            .map_err(|error| format!("Worktree failed: {error}"))?;
                            cwd = worktree.path;
                            Some(worktree.branch)
                        }
                    };
                    call_api(
                        engine.client(),
                        &Mutate::CreateChat {
                            chat_id: chat_id.clone(),
                            space_id,
                            config,
                            branch,
                            cwd: Some(cwd),
                        },
                    )
                    .await
                    .map_err(|error| format!("Couldn't create goal thread: {error}"))?;
                }
                call_api(
                    engine.client(),
                    &QueueCommand {
                        chat_id,
                        command: SessionCommandPayload::Goal { operation },
                        target_device_id: Some(device_id),
                    },
                )
                .await
                .map_err(|error| format!("Couldn't update goal: {error}"))?;
                Ok(())
            }
            .await;
            if let Err(error) = result {
                this.update(cx, |composer, cx| {
                    composer.failure = Some(error.into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_goal_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let theme = Theme::of(cx).clone();
        let dialog = self.goal_dialog.as_ref()?;
        let objective = dialog.objective.clone();
        let budget = dialog.budget.clone();
        let goal = self.state.read(cx).selected_chat_row()?.goal.clone();
        let editing = dialog.goal_id.is_some();
        let mut actions = div().flex().flex_row().items_center().gap(px(8.0));
        if let Some(goal) = goal.filter(|_| editing) {
            if goal.status == GoalStatus::Active {
                let operation = GoalOperation::Pause {
                    goal_id: goal.id.clone(),
                    expected_revision: goal.revision,
                };
                actions = actions.child(
                    crate::popover::btn_ghost(&theme, "Pause", "goal-dialog-pause")
                        .id("goal-dialog-pause")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.goal_dialog = None;
                            this.queue_goal_operation(operation.clone(), cx);
                        })),
                );
            } else if matches!(
                goal.status,
                GoalStatus::Paused | GoalStatus::Blocked | GoalStatus::UsageLimited
            ) {
                let operation = GoalOperation::Resume {
                    goal_id: goal.id.clone(),
                    expected_revision: goal.revision,
                };
                actions = actions.child(
                    crate::popover::btn_ghost(&theme, "Resume", "goal-dialog-resume")
                        .id("goal-dialog-resume")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.goal_dialog = None;
                            this.queue_goal_operation(operation.clone(), cx);
                        })),
                );
            }
            let operation = GoalOperation::Clear {
                goal_id: goal.id,
                expected_revision: goal.revision,
            };
            actions = actions.child(
                crate::popover::btn_danger(&theme, "Clear")
                    .id("goal-dialog-clear")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.goal_dialog = None;
                        this.queue_goal_operation(operation.clone(), cx);
                    })),
            );
        }
        let card = crate::popover::dialog_card(&theme)
            .child(crate::popover::dialog_title(
                &theme,
                if editing {
                    "Manage goal"
                } else {
                    "Create goal"
                },
            ))
            .child(
                div()
                    .mt(px(12.0))
                    .child(crate::popover::dialog_field(objective.into_any_element())),
            )
            .child(
                div()
                    .mt(px(8.0))
                    .child(crate::popover::dialog_field(budget.into_any_element())),
            )
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(actions)
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .child(
                                crate::popover::btn_ghost(&theme, "Cancel", "goal-dialog-cancel")
                                    .id("goal-dialog-cancel")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.goal_dialog = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                crate::popover::btn_primary(
                                    &theme,
                                    if editing { "Save" } else { "Create" },
                                )
                                .id("goal-dialog-save")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.submit_goal_dialog(cx)),
                                ),
                            ),
                    ),
            )
            .into_any_element();
        Some(crate::popover::modal("goal-dialog", viewport, card))
    }

    pub(super) fn render_goal(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let goal = self.state.read(cx).selected_chat_row()?.goal.clone()?;
        let theme = Theme::of(cx).clone();
        let (status, color) = match goal.status {
            GoalStatus::Active => ("ACTIVE", theme.accent),
            GoalStatus::Paused => ("PAUSED", theme.warning),
            GoalStatus::Blocked => ("BLOCKED", theme.warning),
            GoalStatus::UsageLimited => ("USAGE LIMITED", theme.warning),
            GoalStatus::BudgetLimited => ("BUDGET REACHED", theme.warning),
            GoalStatus::Complete => ("COMPLETE", theme.success),
        };
        let usage = goal.token_budget.map_or_else(
            || format!("{} tokens", goal.tokens_used),
            |budget| format!("{} / {} tokens", goal.tokens_used, budget),
        );
        let expanded = self.goal_expanded;
        let mut card = div()
            .id("goal-card")
            .mx(px(4.0))
            .rounded(px(14.0))
            .border_1()
            .border_color(color.opacity(0.30))
            .bg(theme.input_bg)
            .overflow_hidden()
            .child(
                div()
                    .id("goal-card-header")
                    .h(px(38.0))
                    .px(px(11.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.goal_expanded = !this.goal_expanded;
                        cx.notify();
                    }))
                    .child(div().size(px(7.0)).rounded_full().bg(color))
                    .child(
                        div()
                            .text_size(px(10.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(color)
                            .child(status),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(theme.text)
                            .child(SharedString::from(goal.objective.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.5))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(usage)),
                    ),
            );
        if expanded {
            let can_pause = goal.status == GoalStatus::Active;
            let can_resume = matches!(
                goal.status,
                GoalStatus::Paused | GoalStatus::Blocked | GoalStatus::UsageLimited
            );
            let action = |id: &'static str,
                          label: &'static str,
                          operation: GoalOperation,
                          cx: &mut Context<Self>| {
                div()
                    .id(id)
                    .px(px(9.0))
                    .h(px(25.0))
                    .flex()
                    .items_center()
                    .rounded(px(7.0))
                    .cursor_pointer()
                    .bg(crate::theme::ink(0.07))
                    .hover(|style| style.bg(crate::theme::ink(0.12)))
                    .text_size(px(11.0))
                    .text_color(theme.text)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.queue_goal_operation(operation.clone(), cx)
                    }))
                    .child(label)
            };
            let goal_id = goal.id.clone();
            let revision = goal.revision;
            card = card.child(
                div()
                    .border_t_1()
                    .border_color(crate::theme::hairline(0.06))
                    .p(px(11.0))
                    .flex()
                    .flex_col()
                    .gap(px(9.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text.opacity(0.88))
                            .child(SharedString::from(goal.objective)),
                    )
                    .when_some(goal.status_message, |body, message| {
                        body.child(
                            div()
                                .text_size(px(11.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(message)),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .id("edit-goal")
                                    .px(px(9.0))
                                    .h(px(25.0))
                                    .flex()
                                    .items_center()
                                    .rounded(px(7.0))
                                    .cursor_pointer()
                                    .bg(crate::theme::ink(0.07))
                                    .hover(|style| style.bg(crate::theme::ink(0.12)))
                                    .text_size(px(11.0))
                                    .text_color(theme.text)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.open_goal_dialog(cx)),
                                    )
                                    .child(if goal.status == GoalStatus::Complete {
                                        "New goal"
                                    } else {
                                        "Edit"
                                    }),
                            )
                            .when(can_pause, |row| {
                                row.child(action(
                                    "pause-goal",
                                    "Pause",
                                    GoalOperation::Pause {
                                        goal_id: goal_id.clone(),
                                        expected_revision: revision,
                                    },
                                    cx,
                                ))
                            })
                            .when(can_resume, |row| {
                                row.child(action(
                                    "resume-goal",
                                    "Resume",
                                    GoalOperation::Resume {
                                        goal_id: goal_id.clone(),
                                        expected_revision: revision,
                                    },
                                    cx,
                                ))
                            })
                            .child(action(
                                "clear-goal",
                                "Clear",
                                GoalOperation::Clear {
                                    goal_id,
                                    expected_revision: revision,
                                },
                                cx,
                            )),
                    ),
            );
        }
        Some(card.into_any_element())
    }
}
