use super::*;

use jolt_api::{GetCheckoutVcsStatus, RunVcsAction, subscribe as subscribe_api};
use jolt_proto::{
    CheckoutVcsStatus, VcsAction, VcsActionEvent, VcsActionResult, VcsCommitMessage,
    VcsCommitSelection, VcsPublicationState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VcsQuickAction {
    Commit,
    CommitAndPush,
    Push,
    Disabled,
}

pub(super) struct VcsCommitDialog {
    pub status: CheckoutVcsStatus,
    pub input: Entity<ComposerInput>,
    pub selected: std::collections::HashSet<String>,
    pub push_after: bool,
    pub focus_pending: bool,
}

pub(super) struct PendingVcsAction {
    pub action: VcsAction,
    pub target: String,
}

fn publication_target(status: &VcsPublicationState) -> Option<&jolt_proto::VcsPublishTarget> {
    match status {
        VcsPublicationState::NoCompletedChanges { target, .. }
        | VcsPublicationState::Ready { target, .. }
        | VcsPublicationState::Behind { target, .. }
        | VcsPublicationState::Diverged { target, .. } => Some(target),
        VcsPublicationState::NoRemote
        | VcsPublicationState::Ambiguous { .. }
        | VcsPublicationState::Unavailable { .. } => None,
    }
}

fn publication_is_default(status: &VcsPublicationState) -> bool {
    match status {
        VcsPublicationState::NoCompletedChanges { is_default_ref, .. }
        | VcsPublicationState::Ready { is_default_ref, .. }
        | VcsPublicationState::Behind { is_default_ref, .. }
        | VcsPublicationState::Diverged { is_default_ref, .. } => *is_default_ref,
        VcsPublicationState::NoRemote
        | VcsPublicationState::Ambiguous { .. }
        | VcsPublicationState::Unavailable { .. } => false,
    }
}

fn quick_action(status: &CheckoutVcsStatus) -> (VcsQuickAction, &'static str, Option<String>) {
    if !status.working_copy.files.is_empty() {
        return match &status.publication {
            VcsPublicationState::NoRemote => (VcsQuickAction::Commit, "Commit", None),
            VcsPublicationState::Unavailable { reason } => {
                (VcsQuickAction::Commit, "Commit", Some(reason.clone()))
            }
            VcsPublicationState::Behind { .. } | VcsPublicationState::Diverged { .. } => (
                VcsQuickAction::Commit,
                "Commit",
                Some("Push is unavailable until the remote divergence is resolved".into()),
            ),
            VcsPublicationState::NoCompletedChanges { .. } | VcsPublicationState::Ready { .. } => {
                (VcsQuickAction::CommitAndPush, "Commit & push", None)
            }
            VcsPublicationState::Ambiguous { .. } => (
                VcsQuickAction::Commit,
                "Commit",
                Some("Choose a Jolt bookmark from the Push menu after committing".into()),
            ),
        };
    }
    match &status.publication {
        VcsPublicationState::Ready { .. } => (VcsQuickAction::Push, "Push", None),
        VcsPublicationState::NoRemote => (
            VcsQuickAction::Disabled,
            "Push",
            Some("No remote is configured".into()),
        ),
        VcsPublicationState::NoCompletedChanges { .. } => (
            VcsQuickAction::Disabled,
            "Up to date",
            Some("No completed changes to push".into()),
        ),
        VcsPublicationState::Behind { behind, .. } => (
            VcsQuickAction::Disabled,
            "Behind",
            Some(format!("Remote is {behind} commit(s) ahead")),
        ),
        VcsPublicationState::Diverged { ahead, behind, .. } => (
            VcsQuickAction::Disabled,
            "Diverged",
            Some(format!("Local is {ahead} ahead and {behind} behind")),
        ),
        VcsPublicationState::Ambiguous { .. } => (
            VcsQuickAction::Disabled,
            "Choose ref",
            Some("Choose which Jolt bookmark to push".into()),
        ),
        VcsPublicationState::Unavailable { reason } => (
            VcsQuickAction::Disabled,
            "Unavailable",
            Some(reason.clone()),
        ),
    }
}

impl Shell {
    pub(super) fn ensure_vcs_status(&mut self, cx: &mut Context<Self>) {
        let selected = self.state.read(cx).selected_chat_row().cloned();
        let Some(chat) = selected else {
            self.vcs_status_chat = None;
            self.vcs_status = Loadable::Idle;
            self.vcs_status_task = None;
            return;
        };
        if self.vcs_status_chat.as_deref() == Some(chat.id.as_str())
            && !matches!(self.vcs_status, Loadable::Idle)
        {
            return;
        }
        self.load_vcs_status(chat.id, chat.device_id, cx);
    }

    pub(super) fn refresh_vcs_status(&mut self, cx: &mut Context<Self>) {
        let selected = self.state.read(cx).selected_chat_row().cloned();
        let Some(chat) = selected else {
            return;
        };
        self.load_vcs_status(chat.id, chat.device_id, cx);
    }

    fn load_vcs_status(&mut self, chat_id: String, device_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.vcs_status = Loadable::Error("Engine not connected".into());
            return;
        };
        let target_device_id = (self.state.read(cx).local_device_id.as_deref()
            != Some(device_id.as_str()))
        .then_some(device_id);
        self.vcs_status_chat = Some(chat_id.clone());
        self.vcs_status = Loadable::Loading;
        self.vcs_status_task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(
                engine.client(),
                &GetCheckoutVcsStatus {
                    chat_id: chat_id.clone(),
                    target_device_id,
                },
            )
            .await;
            this.update(cx, |shell, cx| {
                if shell.vcs_status_chat.as_deref() != Some(chat_id.as_str()) {
                    return;
                }
                shell.vcs_status_task = None;
                shell.vcs_status = match result {
                    Ok(status) => Loadable::Ready(status),
                    Err(error) => Loadable::Error(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn open_vcs_commit_dialog(&mut self, push_after: bool, cx: &mut Context<Self>) {
        let Some(status) = self.vcs_status.ready().cloned() else {
            return;
        };
        if status.working_copy.files.is_empty() {
            return;
        }
        let selected = status
            .working_copy
            .files
            .iter()
            .map(|file| file.id.clone())
            .collect();
        self.vcs_menu_open = false;
        self.vcs_commit_dialog = Some(VcsCommitDialog {
            status,
            input: cx.new(|cx| ComposerInput::new("Leave empty to generate", cx)),
            selected,
            push_after,
            focus_pending: true,
        });
        cx.notify();
    }

    fn commit_dialog_action(&self, cx: &App) -> Option<VcsAction> {
        let dialog = self.vcs_commit_dialog.as_ref()?;
        if dialog.selected.is_empty() {
            return None;
        }
        let all_selected = dialog.selected.len() == dialog.status.working_copy.files.len();
        let selection = if all_selected {
            VcsCommitSelection::All
        } else {
            VcsCommitSelection::Files {
                file_ids: dialog.selected.iter().cloned().collect(),
            }
        };
        let value = dialog.input.read(cx).text();
        let message = if value.trim().is_empty() {
            VcsCommitMessage::Generate
        } else {
            VcsCommitMessage::Provided {
                value: value.trim().to_string(),
            }
        };
        let expected_working_copy = dialog.status.working_copy.catalog_revision.clone();
        if dialog.push_after {
            let target = publication_target(&dialog.status.publication)?;
            Some(VcsAction::CommitAndPush {
                expected_working_copy,
                expected_publication: target.revision.clone(),
                selection,
                message,
                publish_ref: None,
                allow_default_ref: false,
            })
        } else {
            Some(VcsAction::Commit {
                expected_working_copy,
                selection,
                message,
            })
        }
    }

    fn submit_vcs_commit(&mut self, cx: &mut Context<Self>) {
        let Some(action) = self.commit_dialog_action(cx) else {
            return;
        };
        let default_target = self
            .vcs_commit_dialog
            .as_ref()
            .filter(|dialog| {
                dialog.push_after && publication_is_default(&dialog.status.publication)
            })
            .and_then(|dialog| publication_target(&dialog.status.publication))
            .map(|target| target.remote_ref.clone());
        self.vcs_commit_dialog = None;
        if let Some(target) = default_target {
            self.vcs_default_confirm = Some(PendingVcsAction { action, target });
            cx.notify();
        } else {
            self.start_vcs_action(action, cx);
        }
    }

    fn request_vcs_push(&mut self, publish_ref: Option<String>, cx: &mut Context<Self>) {
        let Some(status) = self.vcs_status.ready() else {
            return;
        };
        let (target, is_default_ref) = match &status.publication {
            VcsPublicationState::Ready {
                target,
                is_default_ref,
                ..
            } if publish_ref.is_none() => (target, *is_default_ref),
            VcsPublicationState::Ambiguous { candidates } => {
                let Some(target) = publish_ref.as_deref().and_then(|selected| {
                    candidates.iter().find(|target| target.ref_name == selected)
                }) else {
                    return;
                };
                (target, false)
            }
            _ => return,
        };
        let action = VcsAction::Push {
            expected_publication: target.revision.clone(),
            publish_ref,
            allow_default_ref: false,
        };
        self.vcs_menu_open = false;
        if is_default_ref {
            self.vcs_default_confirm = Some(PendingVcsAction {
                action,
                target: target.remote_ref.clone(),
            });
            cx.notify();
        } else {
            self.start_vcs_action(action, cx);
        }
    }

    fn confirm_vcs_default_push(&mut self, cx: &mut Context<Self>) {
        let Some(mut pending) = self.vcs_default_confirm.take() else {
            return;
        };
        match &mut pending.action {
            VcsAction::Push {
                allow_default_ref, ..
            }
            | VcsAction::CommitAndPush {
                allow_default_ref, ..
            } => *allow_default_ref = true,
            VcsAction::Commit { .. } => {}
        }
        self.start_vcs_action(pending.action, cx);
    }

    fn start_vcs_action(&mut self, action: VcsAction, cx: &mut Context<Self>) {
        if self.vcs_action_task.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(chat) = self.state.read(cx).selected_chat_row().cloned() else {
            return;
        };
        let target_device_id = (self.state.read(cx).local_device_id.as_deref()
            != Some(chat.device_id.as_str()))
        .then_some(chat.device_id);
        let action_id = uuid::Uuid::new_v4().to_string();
        let toast_id = format!("vcs-action-{action_id}");
        crate::toast::show(
            Toast::new(
                &toast_id,
                "Starting version-control action…",
                ToastKind::Info,
            )
            .persistent(),
            cx,
        );
        self.vcs_action_task = Some(cx.spawn(async move |this, cx| {
            let stream = subscribe_api(
                engine.client(),
                &RunVcsAction {
                    action_id,
                    chat_id: chat.id,
                    action,
                    target_device_id,
                },
            )
            .await;
            let Ok(mut receiver) = stream else {
                let error = stream.unwrap_err().to_string();
                this.update(cx, |shell, cx| {
                    shell.vcs_action_task = None;
                    crate::toast::show(
                        Toast::new(&toast_id, "Version-control action failed", ToastKind::Error)
                            .body(error),
                        cx,
                    );
                    shell.refresh_vcs_status(cx);
                })
                .ok();
                return;
            };
            let mut saw_finished = false;
            while let Some(value) = receiver.recv().await {
                let Ok(event) = serde_json::from_value::<VcsActionEvent>(value) else {
                    continue;
                };
                let finished = matches!(
                    event,
                    VcsActionEvent::Finished { .. } | VcsActionEvent::Failed { .. }
                );
                this.update(cx, |shell, cx| {
                    match event {
                        VcsActionEvent::Started { .. } => {}
                        VcsActionEvent::PhaseStarted { label, .. } => crate::toast::show(
                            Toast::new(&toast_id, label, ToastKind::Info).persistent(),
                            cx,
                        ),
                        VcsActionEvent::Finished { result, .. } => {
                            let (title, body) = vcs_result_copy(&result);
                            crate::toast::show(
                                Toast::new(&toast_id, title, ToastKind::Success).body(body),
                                cx,
                            );
                        }
                        VcsActionEvent::Failed {
                            completed_commit,
                            message,
                            ..
                        } => {
                            let title = if completed_commit.is_some() {
                                "Committed, but push failed"
                            } else {
                                "Version-control action failed"
                            };
                            crate::toast::show(
                                Toast::new(&toast_id, title, ToastKind::Error).body(message),
                                cx,
                            );
                        }
                    }
                    if finished {
                        shell.vcs_action_task = None;
                        shell.refresh_vcs_status(cx);
                    }
                })
                .ok();
                if finished {
                    saw_finished = true;
                    break;
                }
            }
            if !saw_finished {
                this.update(cx, |shell, cx| {
                    shell.vcs_action_task = None;
                    crate::toast::show(
                        Toast::new(
                            &toast_id,
                            "Version-control action interrupted",
                            ToastKind::Error,
                        )
                        .body("The host stopped reporting progress before the action finished"),
                        cx,
                    );
                    shell.refresh_vcs_status(cx);
                })
                .ok();
            }
        }));
        cx.notify();
    }

    fn activate_vcs_quick_action(&mut self, cx: &mut Context<Self>) {
        let Some(status) = self.vcs_status.ready() else {
            return;
        };
        match quick_action(status).0 {
            VcsQuickAction::Commit => self.open_vcs_commit_dialog(false, cx),
            VcsQuickAction::CommitAndPush => self.open_vcs_commit_dialog(true, cx),
            VcsQuickAction::Push => self.request_vcs_push(None, cx),
            VcsQuickAction::Disabled => {}
        }
    }

    pub(super) fn render_vcs_actions_control(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.ensure_vcs_status(cx);
        let theme = Theme::of(cx).clone();
        let (action, label, mut hint) = match &self.vcs_status {
            Loadable::Ready(status) => quick_action(status),
            Loadable::Error(error) => (VcsQuickAction::Disabled, "VCS", Some(error.clone())),
            Loadable::Idle | Loadable::Loading => (VcsQuickAction::Disabled, "Loading…", None),
        };
        let busy = self.vcs_action_task.is_some();
        let agent_active = self.vcs_status.ready().is_some_and(|status| {
            let state = self.state.read(cx);
            let selected_cwd = state
                .selected_chat_row()
                .and_then(|chat| chat.cwd.as_deref());
            state.chats.iter().any(|chat| {
                let same_checkout = chat.checkout_id.as_deref()
                    == Some(status.checkout_id.as_str())
                    || (selected_cwd.is_some() && chat.cwd.as_deref() == selected_cwd);
                same_checkout
                    && matches!(
                        state.indicator_for(&chat.id, Utc::now()),
                        Indicator::Working | Indicator::AwaitingInput
                    )
            })
        });
        if agent_active {
            hint = Some("Wait for active agent work in this checkout to finish".into());
        }
        let available = !busy && !agent_active;
        let enabled = action != VcsQuickAction::Disabled && available;
        let menu_open = self.vcs_menu_open;
        let can_commit = available
            && self
                .vcs_status
                .ready()
                .is_some_and(|status| !status.working_copy.files.is_empty());
        let can_push = available
            && self.vcs_status.ready().is_some_and(|status| {
                matches!(status.publication, VcsPublicationState::Ready { .. })
            });
        let mut control = div()
            .relative()
            .h(px(28.0))
            .flex_none()
            .flex()
            .items_center()
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border)
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .child(
                div()
                    .id("vcs-quick-action")
                    .h_full()
                    .px(px(9.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .text_size(px(11.0))
                    .text_color(if enabled {
                        theme.text
                    } else {
                        theme.text_faint
                    })
                    .when(enabled, |button| {
                        button
                            .cursor_pointer()
                            .hover(|style| style.bg(crate::theme::wash(0.08)))
                            .on_click(
                                cx.listener(|this, _, _, cx| this.activate_vcs_quick_action(cx)),
                            )
                    })
                    .child(icon(icons::GIT_BRANCH).size(px(14.0)))
                    .child(SharedString::from(if busy { "Working…" } else { label })),
            )
            .child(
                div()
                    .id("vcs-action-menu-trigger")
                    .h_full()
                    .w(px(25.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_l_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .hover(|style| style.bg(crate::theme::wash(0.08)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.vcs_menu_open = !this.vcs_menu_open;
                        cx.notify();
                    }))
                    .child(
                        icon(icons::CHEVRON_DOWN)
                            .size(px(13.0))
                            .text_color(theme.text_muted),
                    ),
            );
        if menu_open {
            let commit_color = if can_commit {
                theme.text
            } else {
                theme.text_faint
            };
            let push_color = if can_push {
                theme.text
            } else {
                theme.text_faint
            };
            let mut menu =
                popover::popover_card(&theme)
                    .w(px(230.0))
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.vcs_menu_open = false;
                        cx.notify();
                    }))
                    .child(
                        popover::menu_row(&theme, false, "vcs-menu-commit")
                            .id("vcs-menu-commit")
                            .text_color(commit_color)
                            .when(can_commit, |row| {
                                row.on_click(cx.listener(|this, _, _, cx| {
                                    this.open_vcs_commit_dialog(false, cx)
                                }))
                            })
                            .child(icon(icons::CHECK).size(px(15.0)).text_color(commit_color))
                            .child("Commit…"),
                    )
                    .child(
                        popover::menu_row(&theme, false, "vcs-menu-push")
                            .id("vcs-menu-push")
                            .text_color(push_color)
                            .when(can_push, |row| {
                                row.on_click(
                                    cx.listener(|this, _, _, cx| this.request_vcs_push(None, cx)),
                                )
                            })
                            .child(icon(icons::ARROW_UP).size(px(15.0)).text_color(push_color))
                            .child("Push"),
                    );
            let candidates = self
                .vcs_status
                .ready()
                .and_then(|status| match &status.publication {
                    VcsPublicationState::Ambiguous { candidates } => Some(candidates.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            if !candidates.is_empty() {
                menu = menu.child(popover::menu_separator());
            }
            for (index, candidate) in candidates.into_iter().enumerate() {
                let selected = candidate.ref_name.clone();
                let id = format!("vcs-push-candidate-{index}");
                let color = if available {
                    theme.text
                } else {
                    theme.text_faint
                };
                menu = menu.child(
                    popover::menu_row(&theme, false, id.clone())
                        .id(SharedString::from(id))
                        .text_color(color)
                        .when(available, |row| {
                            row.on_click(cx.listener(move |this, _, _, cx| {
                                this.request_vcs_push(Some(selected.clone()), cx)
                            }))
                        })
                        .child(icon(icons::ARROW_UP).size(px(15.0)).text_color(color))
                        .child(format!("Push {}", candidate.ref_name)),
                );
            }
            if let Some(hint) = hint {
                menu = menu.child(popover::menu_separator()).child(
                    div()
                        .px(px(9.0))
                        .py(px(7.0))
                        .text_size(px(10.0))
                        .text_color(theme.text_muted)
                        .child(hint),
                );
            }
            control = control.child(popover::anchored_menu_below(
                "vcs-actions-menu",
                menu.into_any_element(),
            ));
        }
        control.into_any_element()
    }

    pub(super) fn render_vcs_action_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays = Vec::new();
        if let Some(dialog) = &mut self.vcs_commit_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                window.focus(&dialog.input.focus_handle(cx), cx);
            }
            let input = dialog.input.clone();
            let files = dialog.status.working_copy.files.clone();
            let selected = dialog.selected.clone();
            let reference = dialog.status.reference.clone();
            let backend = dialog.status.backend.label();
            let selected_count = selected.len();
            let push_after = dialog.push_after;
            let mut file_list = div()
                .id("vcs-commit-file-list")
                .max_h(px(240.0))
                .overflow_y_scroll()
                .border_1()
                .border_color(theme.border)
                .rounded(px(8.0));
            for file in files {
                let file_id = file.id.clone();
                let included = selected.contains(&file.id);
                file_list = file_list.child(
                    div()
                        .id(SharedString::from(format!("commit-file-{}", file.id)))
                        .px(px(9.0))
                        .py(px(6.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .cursor_pointer()
                        .hover(|style| style.bg(crate::theme::wash(0.06)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(dialog) = &mut this.vcs_commit_dialog
                                && !dialog.selected.remove(&file_id)
                            {
                                dialog.selected.insert(file_id.clone());
                            }
                            cx.notify();
                        }))
                        .child(
                            icon(if included {
                                icons::CHECK
                            } else {
                                icons::SQUARE
                            })
                            .size(px(14.0))
                            .text_color(if included {
                                theme.accent
                            } else {
                                theme.text_faint
                            }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(11.0))
                                .child(file.path),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme.diff_add)
                                .child(format!("+{}", file.additions)),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme.diff_del)
                                .child(format!("−{}", file.deletions)),
                        ),
                );
            }
            let card = popover::dialog_card(&theme)
                .w(px(560.0))
                .child(popover::dialog_title(
                    &theme,
                    if push_after {
                        "Commit & push"
                    } else {
                        "Commit changes"
                    },
                ))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    format!("{backend} · {reference} · {selected_count} file(s) selected"),
                )))
                .child(div().mt(px(12.0)).child(file_list))
                .child(
                    div()
                        .mt(px(12.0))
                        .child(popover::dialog_body(&theme, "Commit message (optional)"))
                        .child(
                            div()
                                .mt(px(5.0))
                                .child(popover::dialog_field(input.into_any_element())),
                        ),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "vcs-commit-cancel")
                                .id("vcs-commit-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.vcs_commit_dialog = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(
                                &theme,
                                if push_after {
                                    "Commit & push"
                                } else {
                                    "Commit"
                                },
                            )
                            .id("vcs-commit-submit")
                            .when(selected_count > 0, |button| {
                                button.on_click(
                                    cx.listener(|this, _, _, cx| this.submit_vcs_commit(cx)),
                                )
                            }),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("vcs-commit-dialog", viewport, card));
        }
        if let Some(pending) = &self.vcs_default_confirm {
            let target = pending.target.clone();
            let card = popover::dialog_card(&theme)
                .w(px(440.0))
                .child(popover::dialog_title(&theme, "Push to the default ref?"))
                .child(div().mt(px(7.0)).child(popover::dialog_body(
                    &theme,
                    format!("This action will publish directly to {target}."),
                )))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "vcs-default-cancel")
                                .id("vcs-default-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.vcs_default_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Push anyway")
                                .id("vcs-default-confirm")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.confirm_vcs_default_push(cx)),
                                ),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("vcs-default-confirm", viewport, card));
        }
        overlays
    }
}

fn vcs_result_copy(result: &VcsActionResult) -> (&'static str, String) {
    match result {
        VcsActionResult::Commit { commit } => (
            "Committed",
            format!(
                "{} · {}",
                &commit.revision[..commit.revision.len().min(7)],
                commit.subject
            ),
        ),
        VcsActionResult::Push { push } => (
            "Pushed",
            format!(
                "{} to {}",
                &push.revision[..push.revision.len().min(7)],
                push.remote_ref
            ),
        ),
        VcsActionResult::CommitAndPush { commit, push } => (
            "Committed & pushed",
            format!(
                "{} · {} to {}",
                &commit.revision[..commit.revision.len().min(7)],
                commit.subject,
                push.remote_ref
            ),
        ),
    }
}
