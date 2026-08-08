//! Checkout, ref, and review picker behavior.

use super::*;

impl Pickers {
    pub(super) fn invalidate_checkout_review(&mut self) {
        self.checkout_review = None;
        self.review_lookup = None;
        self.review_loaded = false;
        self.review_task = None;
    }

    pub(super) fn selected_review_lookup(&self, cx: &App) -> Option<ReviewLookup> {
        let chat = self.state.read(cx).selected_chat_row()?;
        Some(ReviewLookup {
            chat_id: chat.id.clone(),
            cwd: chat.cwd.clone()?,
            branch: chat.branch.clone(),
            activity_at: chat.last_message_at.map(|at| at.timestamp_millis()),
            device_id: chat.device_id.clone(),
        })
    }

    /// Resolve the selected session checkout's open provider review on its host
    /// device. Failures are intentionally silent: unavailable/unsupported forge
    /// tooling is absence, not a broken composer state.
    pub(super) fn ensure_checkout_review(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(lookup) = self.selected_review_lookup(cx) else {
            self.invalidate_checkout_review();
            return;
        };
        if self.review_lookup.as_ref() != Some(&lookup) {
            self.invalidate_checkout_review();
            self.review_lookup = Some(lookup.clone());
        } else if !force && (self.review_loaded || self.review_task.is_some()) {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        let request = GetCheckoutReview {
            chat_id: lookup.chat_id.clone(),
            target_device_id: (local.as_deref() != Some(lookup.device_id.as_str()))
                .then(|| lookup.device_id.clone()),
        };
        self.review_loaded = false;
        self.review_task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(engine.client(), &request).await;
            this.update(cx, |pickers, cx| {
                if pickers.review_lookup.as_ref() != Some(&lookup) {
                    return;
                }
                pickers.checkout_review = result.ok().flatten();
                pickers.review_loaded = true;
                cx.notify();
            })
            .ok();
        }));
    }

    /// ListRefs for the selected SPACE's folder — targeted at the space's
    /// device (relay-forwarded when remote), keyed/invalidated by space id.
    /// Rows carry checkout state (`current`, `worktreePath`) so the picker can
    /// tag refs and the checkout-kind selector can offer worktree reuse.
    pub(super) fn ensure_refs(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(space) = self.state.read(cx).selected_space_row().cloned() else {
            return;
        };
        if !space.git_detected {
            return;
        }
        let fresh = self.refs_space.as_deref() == Some(space.id.as_str());
        if fresh && matches!(self.refs, Loadable::Loading) {
            return; // a load is already in flight
        }
        // Non-forced (the footer's eager kick, re-run every render) only loads
        // from Idle: an Error must WAIT for an explicit retry/reopen (force),
        // or re-render would flip Error back to Loading before the retry row
        // ever paints — an eternal skeleton plus an RPC storm (user report:
        // "the ref dropdown never loads anything").
        if !force && fresh && !matches!(self.refs, Loadable::Idle) {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        // Stale-while-revalidate: a forced refresh of an already-loaded space
        // keeps the current rows on screen while the reload runs — a send that
        // just minted a worktree (or a terminal-side branch) appears on the
        // popover's next open without the list ever flashing to a skeleton.
        if !(force && fresh && matches!(self.refs, Loadable::Ready(_))) {
            self.refs = Loadable::Loading;
        }
        self.refs_space = Some(space.id.clone());
        let generation = self.catalog_generation;
        self.refs_task = Some(cx.spawn(async move |this, cx| {
            let request = ListRefs {
                repo_path: space.path.clone(),
                target_device_id: (local.as_deref() != Some(space.device_id.as_str()))
                    .then(|| space.device_id.clone()),
            };
            let result = call_api(engine.client(), &request).await;
            this.update(cx, |pickers, cx| {
                if pickers.catalog_generation != generation {
                    return;
                }
                pickers.refs = match result {
                    Ok(refs) => Loadable::Ready(refs),
                    Err(err) => Loadable::Error(err.to_string()),
                };
                // Rows landed under an open, un-searched popover: re-home the
                // nav highlight to the selected row.
                if pickers.open == Some(PickerKind::Branch)
                    && pickers.search.read(cx).text().is_empty()
                {
                    pickers.active = pickers.selected_ref_index(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    // ---- selections ----

    pub(super) fn pick_ref(&mut self, row: RepoRef, cx: &mut Context<Self>) {
        // Existing session: the pick SWITCHES the session's checkout (the
        // t3code mid-session `switchRef`) instead of updating the draft.
        if self.state.read(cx).selected_chat_row().is_some() {
            self.switch_session_ref(row, cx);
            return;
        }
        if row.worktree_path.is_some() {
            // Reuse the ref's existing worktree ("Current worktree") — the
            // t3code `reuseExistingWorktree` path.
            self.config.branch = Some(row.name.clone());
            self.config.checkout = CheckoutKind::Local;
            self.config.revision = Some(row.revision.clone());
        } else if self.config.checkout == CheckoutKind::NewWorktree || row.current {
            // Base pick for a new worktree, or the already-current ref.
            self.config.branch = Some(row.name.clone());
            self.config.revision = Some(row.revision.clone());
        } else {
            // Local mode + a plain non-current ref: CHECK OUT the space
            // folder (full t3code `switchRef` — picking `main` means "put my
            // local checkout on main", it must never flip the mode).
            self.switch_draft_ref(row, cx);
            return;
        }
        self.open = None;
        cx.notify();
    }

    /// Draft-mode ref switch in the SPACE's folder
    /// (relay-forwarded for remote spaces). Success records the pick and
    /// refreshes tags; failure keeps the popover open with the VCS message.
    pub(super) fn switch_draft_ref(&mut self, row: RepoRef, cx: &mut Context<Self>) {
        if self.switching.is_some() {
            return; // one switch at a time
        }
        let Some(space) = self.state.read(cx).selected_space_row().cloned() else {
            return;
        };
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        self.switch_error = None;
        self.switching = Some(row.name.clone());
        let revision = row.revision.clone();
        let jujutsu = row.kind != jolt_proto::RepoRefKind::Branch;
        self.switch_task = Some(cx.spawn(async move |this, cx| {
            let request = SwitchRef {
                repo_path: space.path,
                ref_name: revision.clone(),
                target_device_id: (local.as_deref() != Some(space.device_id.as_str()))
                    .then_some(space.device_id),
            };
            let result = call_api(engine.client(), &request).await;
            this.update(cx, |pickers, cx| {
                pickers.switching = None;
                match result {
                    Ok(value) => {
                        pickers.config.branch = Some(value.branch);
                        pickers.config.revision = Some(if jujutsu { "@".into() } else { revision });
                        pickers.open = None;
                        pickers.ensure_refs(true, cx);
                    }
                    Err(err) => pickers.switch_error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Mid-session ref switch, two shapes (both t3code):
    ///
    /// - The picked ref already lives in ANOTHER worktree → RETARGET the
    ///   session onto that worktree (`reuseExistingWorktree`): a `setChatCwd`
    ///   + `setChatBranch` mutate, no VCS mutation. Resume is cwd-scoped, so the next
    ///     run there starts a fresh harness conversation — the transcript
    ///     itself carries on.
    /// - Otherwise → switch the ref in the SESSION's own cwd (`SwitchRef`,
    ///   relay-forwarded to the host device). The host's HEAD watcher
    ///   reconciles `chat.branch` to every device. Errors (dirty tree, ref
    ///   held by the MAIN checkout) keep the popover open with the VCS message.
    pub(super) fn switch_session_ref(&mut self, row: RepoRef, cx: &mut Context<Self>) {
        if self.switching.is_some() {
            return; // one switch at a time
        }
        let Some(chat) = self.state.read(cx).selected_chat_row().cloned() else {
            return;
        };
        let Some(cwd) = chat.cwd.clone() else {
            return;
        };
        let Some(engine) = self.engine(cx) else {
            return;
        };
        if row.worktree_path.as_deref() == Some(cwd.as_str()) {
            // Already this session's worktree — nothing to do.
            self.open = None;
            cx.notify();
            return;
        }
        let local = self.state.read(cx).local_device_id.clone();
        self.switch_error = None;
        self.switching = Some(row.name.clone());
        let ref_name = row.name.clone();
        let revision = row.revision.clone();
        let retarget = row.worktree_path.clone();
        self.switch_task = Some(cx.spawn(async move |this, cx| {
            let result = match retarget {
                // Reuse the ref's existing worktree: move the session there.
                Some(path) => {
                    let cwd_mutate = Mutate::SetChatCwd {
                        chat_id: chat.id.clone(),
                        cwd: path,
                    };
                    let branch_mutate = Mutate::SetChatBranch {
                        chat_id: chat.id.clone(),
                        branch: ref_name,
                    };
                    match call_api(engine.client(), &cwd_mutate).await {
                        Ok(_) => call_api(engine.client(), &branch_mutate).await.map(drop),
                        Err(err) => Err(err),
                    }
                }
                // Plain ref: checkout in place on the chat's HOST device.
                None => {
                    let request = SwitchRef {
                        repo_path: cwd,
                        ref_name: revision,
                        target_device_id: (local.as_deref() != Some(chat.device_id.as_str()))
                            .then_some(chat.device_id),
                    };
                    call_api(engine.client(), &request).await.map(drop)
                }
            };
            this.update(cx, |pickers, cx| {
                pickers.switching = None;
                match result {
                    Ok(_) => {
                        pickers.open = None;
                        // Checkout state changed — refresh tags/current and its
                        // provider review association.
                        pickers.ensure_refs(true, cx);
                        pickers.invalidate_checkout_review();
                        pickers.ensure_checkout_review(true, cx);
                    }
                    Err(err) => pickers.switch_error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub(super) fn pick_checkout(&mut self, kind: CheckoutKind, cx: &mut Context<Self>) {
        if kind == CheckoutKind::Local
            && self.config.checkout == CheckoutKind::NewWorktree
            && self.selected_ref_worktree().is_none()
            && self.selected_ref().is_some_and(|r| !r.current)
        {
            // Back to "Current checkout" with a non-current plain ref picked:
            // drop the pick (we don't checkout the main folder) — the current
            // branch takes over.
            self.config.branch = None;
            self.config.revision = None;
        }
        self.config.checkout = kind;
        self.open = None;
        cx.notify();
    }

    pub(super) fn filtered_ref_rows(&self, cx: &App) -> Vec<RepoRef> {
        let Some(refs) = self.refs.ready() else {
            return Vec::new();
        };
        let names: Vec<String> = refs.iter().map(|r| r.name.clone()).collect();
        let query = self.search.read(cx).text().to_string();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| refs[ix].clone())
            .collect()
    }

    // ---- checkout resolution (the t3code env-mode semantics) ----

    /// The ref that actually owns an existing chat's cwd. Live checkout state
    /// wins over the persisted branch label: JJ working-copy labels change as
    /// the change id advances, and a new chat can appear before its branch has
    /// been stamped by the host.
    pub(super) fn session_ref<'a>(
        &'a self,
        chat: &jolt_proto::Chat,
        space: &jolt_proto::Space,
    ) -> Option<&'a RepoRef> {
        let refs = self.refs.ready()?;
        let same_checkout = chat.cwd.as_deref() == Some(space.path.as_str())
            || chat
                .checkout_id
                .as_deref()
                .zip(space.checkout_id.as_deref())
                .is_some_and(|(chat, space)| chat == space);
        session_checkout_ref(
            refs,
            chat.branch.as_deref(),
            chat.cwd.as_deref(),
            same_checkout,
        )
    }

    /// Selected ref for either an existing chat or the new-chat draft. An
    /// unstamped chat defaults to the working-copy/current row rather than
    /// displaying an unselected "Select ref" state.
    pub(super) fn selected_ref_name(&self, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        if let Some(chat) = state.selected_chat_row() {
            return state
                .space_for_chat(chat)
                .and_then(|space| self.session_ref(chat, space))
                .map(|row| row.name.clone())
                .or_else(|| chat.branch.clone());
        }
        self.config
            .branch
            .clone()
            .or_else(|| self.selected_ref().map(|row| row.name.clone()))
    }

    /// Index of the highlighted-by-default row in the (filtered) ref list.
    /// Capped to the displayed window.
    pub(super) fn selected_ref_index(&self, cx: &App) -> usize {
        let rows = self.filtered_ref_rows(cx);
        let selected = self.selected_ref_name(cx);
        let index = match selected {
            Some(name) => rows.iter().position(|r| r.name == name).unwrap_or(0),
            None => rows.iter().position(|r| r.current).unwrap_or(0),
        };
        index.min(MAX_REF_ROWS.saturating_sub(1))
    }

    /// The picked ref's row, else the repo's current branch's row.
    pub(super) fn selected_ref(&self) -> Option<&RepoRef> {
        let refs = self.refs.ready()?;
        match self.config.branch.as_deref() {
            Some(name) => refs.iter().find(|r| r.name == name),
            None => refs.iter().find(|r| r.current),
        }
    }

    /// The picked (or current) ref's name.
    pub(super) fn effective_ref_name(&self) -> Option<String> {
        self.config
            .branch
            .clone()
            .or_else(|| self.selected_ref().map(|r| r.name.clone()))
    }

    pub(super) fn effective_ref_revision(&self) -> Option<String> {
        self.config
            .revision
            .clone()
            .or_else(|| self.selected_ref().map(|row| row.revision.clone()))
            .or_else(|| self.effective_ref_name())
    }

    /// The existing worktree the picked ref is materialized in, if any.
    pub(super) fn selected_ref_worktree(&self) -> Option<String> {
        self.selected_ref().and_then(|r| r.worktree_path.clone())
    }

    /// The resolved on-send checkout action for a new session.
    pub fn checkout_plan(&self) -> CheckoutPlan {
        match self.config.checkout {
            CheckoutKind::NewWorktree => CheckoutPlan::NewWorktree {
                base: self.effective_ref_revision(),
            },
            CheckoutKind::Local => match self.selected_ref_worktree() {
                Some(path) => CheckoutPlan::ReuseWorktree {
                    path,
                    branch: self.effective_ref_name().unwrap_or_default(),
                },
                None => CheckoutPlan::CurrentCheckout {
                    branch: self.effective_ref_name(),
                },
            },
        }
    }

    /// Label of the checkout-kind trigger (t3code `resolveEnvModeLabel` /
    /// `resolveCurrentWorkspaceLabel`).
    pub(super) fn checkout_label(&self) -> &'static str {
        let jujutsu = self.refs.ready().is_some_and(|refs| {
            refs.iter()
                .any(|row| row.kind != jolt_proto::RepoRefKind::Branch)
        });
        match self.config.checkout {
            CheckoutKind::NewWorktree if jujutsu => "New workspace",
            CheckoutKind::NewWorktree => "New worktree",
            CheckoutKind::Local if jujutsu => "Working copy",
            CheckoutKind::Local if self.selected_ref_worktree().is_some() => "Current worktree",
            CheckoutKind::Local => "Current checkout",
        }
    }

    /// Label of the ref trigger: `From <ref>` only when a NEW worktree will be
    /// created off it (t3code `getBranchTriggerLabel`); the bare name otherwise.
    pub(super) fn ref_label(&self) -> SharedString {
        match (self.config.checkout, self.effective_ref_name()) {
            (_, None) => SharedString::from("Select ref"),
            (CheckoutKind::NewWorktree, Some(name)) => SharedString::from(format!("From {name}")),
            (CheckoutKind::Local, Some(name)) => SharedString::from(name),
        }
    }

    pub(super) fn render_branch_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        if self.state.read(cx).selected_space_row().is_none() {
            return div()
                .p(px(Theme::SPACE_SM))
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from("No space selected"))
                .into_any_element();
        }
        let rows = self.filtered_ref_rows(cx);
        let total = rows.len();
        let shown = total.min(MAX_REF_ROWS);
        // Existing session: the highlighted row is the ref owning the
        // session's cwd; a new chat highlights the draft/current ref.
        let selected_ref = self.selected_ref_name(cx);
        let switching = self.switching.clone();
        let body: AnyElement =
            match &self.refs {
                Loadable::Loading | Loadable::Idle => {
                    popover::skeleton_rows("branch-skeleton", &theme, 4, cx.entity_id(), cx)
                }
                Loadable::Error(message) => {
                    let message = message.clone();
                    self.retry_row("branch-retry", &message, PickerKind::Branch, &theme, cx)
                }
                Loadable::Ready(_) if rows.is_empty() => div()
                    .p(px(Theme::SPACE_SM))
                    .text_size(px(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from("No refs found."))
                    .into_any_element(),
                Loadable::Ready(_) => {
                    let active = self.active;
                    let selected = selected_ref;
                    div()
                        .id("branch-list")
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .max_h(px(224.0))
                        .overflow_y_scroll()
                        .children(rows.into_iter().take(MAX_REF_ROWS).enumerate().map(
                            |(ix, row)| {
                                let label: SharedString = row.name.clone().into();
                                let is_selected = selected.as_deref() == Some(row.name.as_str());
                                // Right-aligned muted tag (t3code `text-[10px]
                                // text-muted-foreground/45`): current beats worktree.
                                let tag: Option<&'static str> = match row.kind {
                                    jolt_proto::RepoRefKind::WorkingCopy if row.current => {
                                        Some("working copy")
                                    }
                                    jolt_proto::RepoRefKind::WorkingCopy => Some("workspace"),
                                    jolt_proto::RepoRefKind::Bookmark => Some("bookmark"),
                                    jolt_proto::RepoRefKind::Branch if row.current => {
                                        Some("current")
                                    }
                                    jolt_proto::RepoRefKind::Branch
                                        if row.worktree_path.is_some() =>
                                    {
                                        Some("worktree")
                                    }
                                    jolt_proto::RepoRefKind::Branch => None,
                                };
                                let is_switching = switching.as_deref() == Some(row.name.as_str());
                                popover::menu_row_nav(
                                    &theme,
                                    is_selected,
                                    ix == active,
                                    format!("branch-row-{ix}"),
                                )
                                .id(("branch-row", ix))
                                .when(switching.is_some(), |el| el.opacity(0.55))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_ref(row.clone(), cx);
                                }))
                                .child(div().flex_1().min_w_0().truncate().child(label))
                                .when(is_switching, |el| {
                                    el.child(
                                        div()
                                            .flex_none()
                                            .text_size(px(10.0))
                                            .text_color(theme.text_muted.opacity(0.6))
                                            .child(SharedString::from("switching…")),
                                    )
                                })
                                .when_some(tag, |el, tag| {
                                    el.child(
                                        div()
                                            .flex_none()
                                            .text_size(px(10.0))
                                            .text_color(theme.text_muted.opacity(0.45))
                                            .child(SharedString::from(tag)),
                                    )
                                })
                            },
                        ))
                        .into_any_element()
                }
            };
        let mut popover = div()
            .flex()
            .flex_col()
            .child(self.search_box(&theme))
            .child(body);
        // Mid-session switch failure (dirty tree, ref checked out elsewhere):
        // the VCS's own message, under a hairline.
        if let Some(error) = &self.switch_error {
            popover = popover.child(
                popover::menu_section().child(
                    div()
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .text_size(px(11.0))
                        .text_color(theme.danger.opacity(0.9))
                        .child(SharedString::from(error.clone())),
                ),
            );
        }
        if total > shown {
            popover = popover.child(
                popover::menu_section().child(
                    div()
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!(
                            "Showing {shown} of {total} refs"
                        ))),
                ),
            );
        }
        popover.into_any_element()
    }

    /// The checkout-kind dropdown (t3code BranchToolbarEnvModeSelector): two
    /// rows — "Current checkout"/"Current worktree" (local) and "New worktree".
    pub(super) fn render_checkout_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let has_worktree = self.selected_ref_worktree().is_some();
        let jujutsu = self.refs.ready().is_some_and(|refs| {
            refs.iter()
                .any(|row| row.kind != jolt_proto::RepoRefKind::Branch)
        });
        let local_label: &'static str = if jujutsu {
            "Working copy"
        } else if has_worktree {
            "Current worktree"
        } else {
            "Current checkout"
        };
        let local_icon = if has_worktree {
            crate::icons::FOLDERS
        } else {
            crate::icons::FOLDER
        };
        let options: [(CheckoutKind, &'static str, &'static str); 2] = [
            (CheckoutKind::Local, local_label, local_icon),
            (
                CheckoutKind::NewWorktree,
                if jujutsu {
                    "New workspace"
                } else {
                    "New worktree"
                },
                crate::icons::FOLDERS,
            ),
        ];
        let active = self.active;
        let current = self.config.checkout;
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(
                options
                    .into_iter()
                    .enumerate()
                    .map(|(ix, (kind, label, icon_path))| {
                        let is_selected = current == kind;
                        popover::menu_row_nav(
                            &theme,
                            is_selected,
                            ix == active,
                            format!("checkout-row-{ix}"),
                        )
                        .id(("checkout-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pick_checkout(kind, cx);
                        }))
                        .child(
                            crate::icons::icon(icon_path)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(label)),
                        )
                    }),
            )
            .into_any_element()
    }
}
