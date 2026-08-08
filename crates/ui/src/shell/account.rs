//! Shell account behavior.

use super::*;

impl Shell {
    // ---- sidebar mutations ----

    /// Fire a Mutate op; failures surface through the app-wide toast center.
    pub(super) fn mutate(&mut self, request: Mutate, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            crate::toast::show(
                Toast::new("mutation-error", "Action failed", ToastKind::Error)
                    .body("The Jolt engine is not connected."),
                cx,
            );
            return;
        };
        self.mutate_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = call_api(engine.client(), &request).await {
                this.update(cx, |_, cx| {
                    crate::toast::show(
                        Toast::new("mutation-error", "Action failed", ToastKind::Error)
                            .body(err.to_string()),
                        cx,
                    );
                })
                .ok();
            }
        }));
    }

    pub(super) fn open_rename_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.chat_menu = None;
        let current = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .and_then(|c| c.title.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Thread title", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename_chat(cx);
            }
        });
        self.rename_dialog = Some(RenameChatDialog {
            chat_id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    pub(super) fn submit_rename_chat(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_dialog.take() else {
            return;
        };
        let title = dialog.input.read(cx).text().trim().to_string();
        if !title.is_empty() {
            self.mutate(
                Mutate::RenameChat {
                    chat_id: dialog.chat_id,
                    title,
                },
                cx,
            );
        }
        cx.notify();
    }

    pub(super) fn regenerate_chat_title(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.chat_menu = None;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            crate::toast::show(
                Toast::new(
                    "regenerate-title-error",
                    "Regenerate name failed",
                    ToastKind::Error,
                )
                .body("The Jolt engine is not connected."),
                cx,
            );
            return;
        };
        let Some(target_device_id) = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .map(|chat| chat.device_id.clone())
        else {
            crate::toast::show(
                Toast::new(
                    "regenerate-title-error",
                    "Regenerate name failed",
                    ToastKind::Error,
                )
                .body("The thread no longer exists."),
                cx,
            );
            return;
        };
        self.regenerate_title_task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(
                engine.client(),
                &RegenerateChatTitle {
                    chat_id,
                    target_device_id: Some(target_device_id),
                },
            )
            .await;
            this.update(cx, |shell, cx| {
                shell.regenerate_title_task = None;
                if let Err(error) = result {
                    crate::toast::show(
                        Toast::new(
                            "regenerate-title-error",
                            "Regenerate name failed",
                            ToastKind::Error,
                        )
                        .body(error.to_string()),
                        cx,
                    );
                }
            })
            .ok();
        }));
        cx.notify();
    }

    pub(super) fn set_chat_pinned(
        &mut self,
        chat_id: String,
        pinned: bool,
        cx: &mut Context<Self>,
    ) {
        self.chat_menu = None;
        self.mutate(Mutate::SetChatPinned { chat_id, pinned }, cx);
        cx.notify();
    }

    pub(super) fn archive_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.chat_menu = None;
        let run_active = {
            let state = self.state.read(cx);
            state
                .chats
                .iter()
                .find(|chat| chat.id == chat_id)
                .is_some_and(|chat| {
                    matches!(
                        state.display_status_for(chat, Utc::now()),
                        jolt_proto::ChatIndicator::Working
                            | jolt_proto::ChatIndicator::AwaitingInput
                    )
                })
        };
        if run_active {
            crate::toast::show(
                Toast::new(
                    "close-thread-active",
                    "Thread is active",
                    ToastKind::Warning,
                )
                .body("Stop the current run before closing this thread."),
                cx,
            );
            cx.notify();
            return;
        }
        if self.state.read(cx).selected_chat.as_deref() == Some(chat_id.as_str()) {
            self.state
                .update(cx, |state, cx| state.select_chat(None, cx));
        }
        self.mutate(
            Mutate::SetChatArchived {
                chat_id,
                archived: true,
            },
            cx,
        );
        cx.notify();
    }

    pub(super) fn unarchive_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.mutate(
            Mutate::SetChatArchived {
                chat_id,
                archived: false,
            },
            cx,
        );
        cx.notify();
    }

    pub(super) fn confirm_delete_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.chat_menu = None;
        self.delete_confirm = Some(chat_id);
        cx.notify();
    }

    pub(super) fn delete_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.delete_confirm = None;
        if self.state.read(cx).selected_chat.as_deref() == Some(chat_id.as_str()) {
            self.state.update(cx, |s, cx| s.select_chat(None, cx));
        }
        self.composer
            .update(cx, |composer, _| composer.purge_chat(&chat_id));
        self.mutate(Mutate::DeleteChat { chat_id }, cx);
        cx.notify();
    }

    pub(super) fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.user_menu_open = false;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = call_api(engine.client(), &SignOut::default()).await {
                this.update(cx, |_, cx| {
                    crate::toast::show(
                        Toast::new("sign-out-error", "Sign out failed", ToastKind::Error)
                            .body(err.to_string()),
                        cx,
                    );
                })
                .ok();
            }
        }));
        cx.notify();
    }

    pub(super) fn switch_scope(&mut self, scope: ScopeKind, cx: &mut Context<Self>) {
        self.user_menu_open = false;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(err) =
                jolt_api::call(engine.client(), &jolt_api::SwitchScope { scope }).await
            {
                tracing::warn!(error = %err, "scope switch failed");
            }
        })
        .detach();
        cx.notify();
    }

    pub(super) fn resolve_account_link(&mut self, merge: bool, cx: &mut Context<Self>) {
        if merge {
            let navigation = ScopeNavigation {
                last_space_id: self.settings.last_space_id.clone(),
                space_filter: self.settings.space_filter.clone(),
            };
            self.settings
                .scope_navigation
                .insert("account".into(), navigation);
            self.settings
                .scope_navigation
                .insert("local".into(), ScopeNavigation::default());
            self.schedule_save(cx);
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) =
                jolt_api::call(engine.client(), &jolt_api::ResolveAccountLink { merge }).await
            {
                this.update(cx, |_, cx| {
                    crate::toast::show(
                        Toast::new(
                            "local-account-link-error",
                            "Couldn’t open the account",
                            ToastKind::Error,
                        )
                        .body(err.to_string()),
                        cx,
                    );
                })
                .ok();
            }
        }));
    }

    pub(super) fn start_sign_in(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(engine.client(), &SignIn::default()).await;
            this.update(cx, |_, cx| match result {
                Ok(value) => cx.open_url(&value.url),
                Err(err) => {
                    crate::toast::show(
                        Toast::new("sign-in-error", "Sign in failed", ToastKind::Error)
                            .body(err.to_string()),
                        cx,
                    );
                }
            })
            .ok();
        }));
    }

    // ---- automatic organization setup ----

    pub(super) fn ensure_org_ui(&mut self, cx: &mut Context<Self>) {
        if self.org.is_some() {
            return;
        }
        self.org = Some(OrgGateUi {
            submitting: false,
            error: None,
            task: None,
        });
        self.provision_personal_org(cx);
    }

    pub(super) fn provision_personal_org(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(org) = self.org.as_mut() else { return };
        if org.submitting {
            return;
        }
        org.submitting = true;
        org.error = None;
        org.task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(engine.client(), &EnsurePersonalOrg::default()).await;
            this.update(cx, |shell, cx| {
                if let Some(org) = shell.org.as_mut() {
                    org.submitting = false;
                    if let Err(err) = result {
                        org.error = Some(err.to_string().into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}
