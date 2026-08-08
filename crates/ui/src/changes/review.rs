//! Pending review draft lifecycle and rendering.

use super::*;

impl Changes {
    pub(super) fn diff_comments(&self) -> &[DiffReviewComment] {
        match self.review.as_ref().map(|review| &review.target) {
            Some(ReviewDraftTarget::Diff { comments, .. }) => comments,
            _ => &[],
        }
    }

    pub(super) fn diff_comments_mut(&mut self) -> Option<&mut Vec<DiffReviewComment>> {
        match self.review.as_mut().map(|review| &mut review.target) {
            Some(ReviewDraftTarget::Diff { comments, .. }) => Some(comments),
            _ => None,
        }
    }

    pub(super) fn set_review_source(&mut self, source: &DiffSource, cx: &mut Context<Self>) {
        let key = review_key_for_source(source);
        if self.review_key.as_deref() == Some(key.as_str()) {
            return;
        }
        self.persist_review_now(cx);
        self.review_key = Some(key.clone());
        self.review = None;
        self.newer_manifest = None;
        self.pinning_revision = None;
        self.editing_comment = None;
        self.comment_editor = None;
        self.comment_editor_subscription = None;
        self.review_save_task = None;
        self.review_pin_task = None;
        self.review_sending = false;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.review_load_task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(
                engine.client(),
                &GetReviewDraft {
                    review_key: key.clone(),
                },
            )
            .await;
            this.update(cx, |changes, cx| {
                if changes.review_key.as_deref() != Some(key.as_str()) {
                    return;
                }
                match result {
                    Ok(Some(draft)) if draft.review_key == key => {
                        changes.apply_loaded_review(draft, cx);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        changes.error =
                            Some(format!("Couldn’t load pending review feedback: {error}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn apply_loaded_review(&mut self, draft: ReviewDraft, cx: &mut Context<Self>) {
        let ReviewDraftTarget::Diff {
            subject,
            snapshot,
            comments,
        } = &draft.target
        else {
            return;
        };
        if draft.snapshot_id != snapshot.manifest.catalog_revision {
            self.error = Some("Pending review refers to an invalid snapshot".into());
            return;
        }
        let manifest = snapshot.manifest.clone();
        self.source = Some(match subject {
            DiffReviewSubject::WorkingCopy { chat_id } => DiffSource::Checkout {
                chat_id: chat_id.clone(),
                target: snapshot.target_device_id.clone(),
            },
            DiffReviewSubject::AssistantTurn {
                chat_id,
                assistant_message_id,
            } => DiffSource::Turn {
                chat_id: chat_id.clone(),
                assistant_message_id: assistant_message_id.clone(),
                target: snapshot.target_device_id.clone(),
            },
        });
        let commented_files: HashSet<_> = comments
            .iter()
            .map(|comment| comment.anchor.file_id.clone())
            .collect();
        self.review = Some(draft);
        self.manifest = Some(manifest.clone());
        self.pages.clear();
        self.page_order.clear();
        self.page_bytes = 0;
        self.loading.clear();
        self.page_tasks.clear();
        self.page_errors.clear();
        self.expanded.extend(commented_files.iter().cloned());
        self.rebuild_rows();
        let pages: Vec<_> = manifest
            .files
            .iter()
            .filter(|file| commented_files.contains(&file.id))
            .flat_map(|file| file.page_ids.iter().cloned())
            .collect();
        for page in pages {
            self.load_page(page, cx);
        }
    }

    pub(super) fn persist_review_now(&self, cx: &mut Context<Self>) {
        let Some(draft) = self.review.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        cx.spawn(async move |_, _| {
            let _ = call_api(engine.client(), &PutReviewDraft { draft }).await;
        })
        .detach();
    }

    pub(super) fn schedule_review_save(&mut self, cx: &mut Context<Self>) {
        let Some(review) = self.review.as_mut() else {
            return;
        };
        review.updated_at = chrono::Utc::now();
        let draft = review.clone();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Couldn’t save review feedback while offline".into());
            return;
        };
        self.review_save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(350))
                .await;
            let result = call_api(
                engine.client(),
                &PutReviewDraft {
                    draft: draft.clone(),
                },
            )
            .await;
            if let Err(error) = result {
                this.update(cx, |changes, cx| {
                    if changes
                        .review
                        .as_ref()
                        .is_some_and(|current| current.review_id == draft.review_id)
                    {
                        changes.error =
                            Some(format!("Couldn’t save review feedback: {error}").into());
                        cx.notify();
                    }
                })
                .ok();
            }
        }));
    }

    pub(super) fn line_value(
        &self,
        point: &ReviewLinePoint,
    ) -> Option<(DiffFileDescriptor, DiffLine)> {
        let file = self.manifest.as_ref()?.files.get(point.file)?.clone();
        let line = self
            .pages
            .get(&point.page_id)?
            .file
            .hunks
            .get(point.hunk)?
            .lines
            .get(point.line)?
            .clone();
        Some((file, line))
    }

    pub(super) fn loaded_line_points(&self, file: usize, side: ReviewSide) -> Vec<ReviewLinePoint> {
        let Some(descriptor) = self
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.files.get(file))
        else {
            return Vec::new();
        };
        let mut points = Vec::new();
        for page_id in &descriptor.page_ids {
            let Some(page) = self.pages.get(page_id) else {
                continue;
            };
            let mut flat_line = 0;
            for (hunk, value) in page.file.hunks.iter().enumerate() {
                for line in 0..value.lines.len() {
                    points.push(ReviewLinePoint {
                        file,
                        page_id: page_id.clone(),
                        hunk,
                        line,
                        flat_line,
                        side,
                    });
                    flat_line += 1;
                }
            }
        }
        points
    }

    pub(super) fn point_for_excerpt(
        &self,
        file_id: &str,
        excerpt: &DiffReviewExcerptLine,
        side: ReviewSide,
    ) -> Option<ReviewLinePoint> {
        let file = self
            .manifest
            .as_ref()?
            .files
            .iter()
            .position(|file| file.id == file_id)?;
        self.loaded_line_points(file, side)
            .into_iter()
            .find(|point| {
                self.line_value(point)
                    .is_some_and(|(_, line)| excerpt_matches_line(excerpt, &line))
            })
    }

    pub(super) fn anchor_between(
        &self,
        left: &ReviewLinePoint,
        right: &ReviewLinePoint,
    ) -> Option<DiffReviewAnchor> {
        if left.file != right.file || left.side != right.side {
            return None;
        }
        let points = self.loaded_line_points(left.file, left.side);
        let left_index = points.iter().position(|point| point.same_line(left))?;
        let right_index = points.iter().position(|point| point.same_line(right))?;
        let (start, end) = if left_index <= right_index {
            (left_index, right_index)
        } else {
            (right_index, left_index)
        };
        let file = self.manifest.as_ref()?.files.get(left.file)?.clone();
        let mut excerpt = Vec::new();
        for point in &points[start..=end] {
            let (_, value) = self.line_value(point)?;
            if !side_contains_line(left.side, &value) {
                continue;
            }
            let Some(kind) = review_line_kind(value.kind) else {
                continue;
            };
            excerpt.push(DiffReviewExcerptLine {
                kind,
                old_number: value.old_no,
                new_number: value.new_no,
                text: value.text,
            });
        }
        if excerpt.is_empty() {
            return None;
        }
        let old_lines = (left.side != ReviewSide::New)
            .then(|| {
                InclusiveLineRange::containing(excerpt.iter().filter_map(|line| line.old_number))
            })
            .flatten();
        let new_lines = (left.side != ReviewSide::Old)
            .then(|| {
                InclusiveLineRange::containing(excerpt.iter().filter_map(|line| line.new_number))
            })
            .flatten();
        Some(DiffReviewAnchor {
            file_id: file.id,
            path: file.path,
            old_path: file.old_path,
            old_lines,
            new_lines,
            excerpt,
        })
    }

    pub(super) fn select_review_line(
        &mut self,
        point: ReviewLinePoint,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pinning_revision.is_some() || self.review_sending {
            return;
        }
        if extend
            && let Some(comment_id) = self.editing_comment.clone()
            && let Some(comment) = self
                .diff_comments()
                .iter()
                .find(|comment| comment.id == comment_id)
        {
            let side = review_side(&comment.anchor);
            if side != point.side {
                return;
            }
            if let Some(first) = comment.anchor.excerpt.first()
                && let Some(first_point) =
                    self.point_for_excerpt(&comment.anchor.file_id, first, side)
                && let Some(anchor) = self.anchor_between(&first_point, &point)
            {
                if let Some(comment) = self.diff_comments_mut().and_then(|comments| {
                    comments.iter_mut().find(|comment| comment.id == comment_id)
                }) {
                    comment.anchor = anchor;
                    comment.updated_at = chrono::Utc::now();
                }
                self.schedule_review_save(cx);
                self.rebuild_rows();
                cx.notify();
            }
            return;
        }
        let Some(anchor) = self.anchor_between(&point, &point) else {
            return;
        };
        if self.review.is_some() {
            let replace = self
                .editing_comment
                .as_ref()
                .and_then(|id| {
                    self.diff_comments()
                        .iter()
                        .find(|comment| &comment.id == id)
                })
                .is_some_and(|comment| comment.body.trim().is_empty());
            if replace {
                let id = self.editing_comment.clone().unwrap_or_default();
                if let Some(comment) = self
                    .diff_comments_mut()
                    .and_then(|comments| comments.iter_mut().find(|comment| comment.id == id))
                {
                    comment.anchor = anchor;
                    comment.updated_at = chrono::Utc::now();
                }
                self.schedule_review_save(cx);
                self.rebuild_rows();
                cx.notify();
                return;
            }
            let now = chrono::Utc::now();
            let comment = DiffReviewComment {
                id: uuid::Uuid::new_v4().to_string(),
                anchor,
                body: String::new(),
                created_at: now,
                updated_at: now,
            };
            let id = comment.id.clone();
            if let Some(comments) = self.diff_comments_mut() {
                comments.push(comment);
            }
            self.open_comment_editor(id, window, cx);
            self.schedule_review_save(cx);
            self.rebuild_rows();
            cx.notify();
            return;
        }

        let Some(source) = self.source.clone() else {
            return;
        };
        let Some(manifest) = self.manifest.clone() else {
            return;
        };
        let Some(review_key) = self.review_key.clone() else {
            return;
        };
        let now = chrono::Utc::now();
        let review_id = uuid::Uuid::new_v4().to_string();
        let comment = DiffReviewComment {
            id: uuid::Uuid::new_v4().to_string(),
            anchor,
            body: String::new(),
            created_at: now,
            updated_at: now,
        };
        if matches!(source, DiffSource::Turn { .. }) {
            self.install_new_review(review_id, review_key, source, manifest, comment, window, cx);
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let revision = manifest.catalog_revision.clone();
        self.pinning_revision = Some(revision.clone());
        let request = PinDiffDocument {
            chat_id: match &source {
                DiffSource::Checkout { chat_id, .. } => chat_id.clone(),
                DiffSource::Turn { chat_id, .. } => chat_id.clone(),
            },
            catalog_revision: revision.clone(),
            review_id: review_id.clone(),
            target_device_id: match &source {
                DiffSource::Checkout { target, .. } => target.clone(),
                DiffSource::Turn { .. } => None,
            },
        };
        self.review_pin_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = call_api(engine.client(), &request).await;
            this.update_in(cx, |changes, window, cx| {
                if changes.pinning_revision.as_deref() != Some(revision.as_str()) {
                    return;
                }
                changes.pinning_revision = None;
                match result {
                    Ok(_) => changes.install_new_review(
                        review_id, review_key, source, manifest, comment, window, cx,
                    ),
                    Err(error) => {
                        changes.error =
                            Some(format!("Couldn’t preserve this diff revision: {error}").into());
                        changes.show_newer_manifest();
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn install_new_review(
        &mut self,
        review_id: String,
        review_key: String,
        source: DiffSource,
        manifest: CheckoutDiffManifest,
        comment: DiffReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let chat_id = match &source {
            DiffSource::Checkout { chat_id, .. } | DiffSource::Turn { chat_id, .. } => {
                chat_id.clone()
            }
        };
        let target_device_id = match &source {
            DiffSource::Checkout { target, .. } | DiffSource::Turn { target, .. } => target.clone(),
        };
        let now = chrono::Utc::now();
        let comment_id = comment.id.clone();
        self.review = Some(ReviewDraft {
            schema_version: REVIEW_DRAFT_SCHEMA_VERSION,
            review_id,
            review_key,
            destination_chat_id: chat_id,
            snapshot_id: manifest.catalog_revision.clone(),
            target: ReviewDraftTarget::Diff {
                subject: review_subject(&source),
                snapshot: DiffReviewSnapshot {
                    manifest,
                    target_device_id,
                },
                comments: vec![comment],
            },
            created_at: now,
            updated_at: now,
        });
        self.open_comment_editor(comment_id, window, cx);
        self.schedule_review_save(cx);
        self.rebuild_rows();
        cx.notify();
    }

    pub(super) fn open_comment_editor(
        &mut self,
        comment_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let body = self
            .diff_comments()
            .iter()
            .find(|comment| comment.id == comment_id)
            .map(|comment| comment.body.clone())
            .unwrap_or_default();
        let editor = cx.new(|cx| ComposerInput::new("Add feedback…", cx));
        editor.update(cx, |editor, cx| editor.set_text(body, cx));
        self.comment_editor_subscription = Some(cx.subscribe(
            &editor,
            |this: &mut Changes, editor, event: &ComposerInputEvent, cx| match event {
                ComposerInputEvent::Edited => {
                    let body = editor.read(cx).text().to_string();
                    this.update_editing_comment(body, cx);
                }
                ComposerInputEvent::Submitted => this.finish_comment(cx),
                _ => {}
            },
        ));
        self.editing_comment = Some(comment_id);
        self.comment_editor = Some(editor.clone());
        window.focus(&editor.read(cx).focus_handle(cx), cx);
    }

    pub(super) fn update_editing_comment(&mut self, body: String, cx: &mut Context<Self>) {
        let Some(comment_id) = self.editing_comment.clone() else {
            return;
        };
        let Some(comment) = self
            .diff_comments_mut()
            .and_then(|comments| comments.iter_mut().find(|comment| comment.id == comment_id))
        else {
            return;
        };
        if comment.body == body {
            return;
        }
        comment.body = body;
        comment.updated_at = chrono::Utc::now();
        self.schedule_review_save(cx);
        cx.notify();
    }

    pub(super) fn finish_comment(&mut self, cx: &mut Context<Self>) {
        let Some(comment_id) = self.editing_comment.clone() else {
            return;
        };
        let empty = self
            .diff_comments()
            .iter()
            .find(|comment| comment.id == comment_id)
            .is_none_or(|comment| comment.body.trim().is_empty());
        if empty {
            self.delete_comment(&comment_id, cx);
            return;
        }
        self.editing_comment = None;
        self.comment_editor = None;
        self.comment_editor_subscription = None;
        self.schedule_review_save(cx);
        self.rebuild_rows();
        cx.notify();
    }

    pub(super) fn edit_comment(
        &mut self,
        comment_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_comment_editor(comment_id, window, cx);
        self.rebuild_rows();
        cx.notify();
    }

    pub(super) fn delete_comment(&mut self, comment_id: &str, cx: &mut Context<Self>) {
        if let Some(comments) = self.diff_comments_mut() {
            comments.retain(|comment| comment.id != comment_id);
        }
        if self.editing_comment.as_deref() == Some(comment_id) {
            self.editing_comment = None;
            self.comment_editor = None;
            self.comment_editor_subscription = None;
        }
        if self.diff_comments().is_empty() {
            self.discard_review(cx);
        } else {
            self.schedule_review_save(cx);
            self.rebuild_rows();
            cx.notify();
        }
    }

    pub(super) fn discard_review(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.review.take() else {
            return;
        };
        self.review_save_task = None;
        self.editing_comment = None;
        self.comment_editor = None;
        self.comment_editor_subscription = None;
        self.review_sending = false;
        self.delete_persisted_review(draft, cx);
        self.show_newer_manifest();
        self.rebuild_rows();
        cx.notify();
    }

    pub(super) fn delete_persisted_review(&self, draft: ReviewDraft, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        cx.spawn(async move |_, _| {
            let _ = call_api(
                engine.client(),
                &DeleteReviewDraft {
                    review_key: draft.review_key.clone(),
                },
            )
            .await;
            if let ReviewDraftTarget::Diff {
                subject: DiffReviewSubject::WorkingCopy { .. },
                snapshot,
                ..
            } = draft.target
            {
                let _ = call_api(
                    engine.client(),
                    &ReleaseDiffDocument {
                        catalog_revision: snapshot.manifest.catalog_revision,
                        review_id: draft.review_id,
                        target_device_id: snapshot.target_device_id,
                    },
                )
                .await;
            }
        })
        .detach();
    }

    pub(super) fn send_review(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.review.as_ref() else {
            return;
        };
        let Some(message) = feedback_message(draft) else {
            self.error = Some("Add feedback before sending the review".into());
            cx.notify();
            return;
        };
        if message.len() > REVIEW_MESSAGE_MAX_BYTES {
            self.error = Some("Review feedback is too large to send as one message".into());
            cx.notify();
            return;
        }
        self.review_sending = true;
        self.inflight_reviews
            .insert(draft.review_id.clone(), draft.clone());
        self.error = None;
        cx.emit(ChangesEvent::SubmitReview {
            review_id: draft.review_id.clone(),
            chat_id: draft.destination_chat_id.clone(),
            message,
        });
        cx.notify();
    }

    pub fn review_submission_finished(
        &mut self,
        review_id: &str,
        error: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let inflight = self.inflight_reviews.remove(review_id);
        let current_matches = self
            .review
            .as_ref()
            .is_some_and(|draft| draft.review_id == review_id);
        if !current_matches {
            if error.is_none()
                && let Some(draft) = inflight
            {
                self.delete_persisted_review(draft, cx);
            }
            return;
        }
        self.review_sending = false;
        if let Some(error) = error {
            self.error = Some(format!("Couldn’t send review feedback: {error}").into());
            cx.notify();
            return;
        }
        let draft = self.review.take().unwrap();
        self.review_save_task = None;
        self.delete_persisted_review(draft, cx);
        self.editing_comment = None;
        self.comment_editor = None;
        self.comment_editor_subscription = None;
        self.show_newer_manifest();
        self.rebuild_rows();
        cx.notify();
    }

    pub(super) fn show_newer_manifest(&mut self) {
        let Some(manifest) = self.newer_manifest.take() else {
            return;
        };
        self.pages.clear();
        self.page_order.clear();
        self.page_bytes = 0;
        self.loading.clear();
        self.page_tasks.clear();
        self.page_errors.clear();
        self.manifest = Some(manifest);
    }

    pub fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.expanded.clear();
        self.rebuild_rows();
        cx.notify();
    }

    pub(super) fn render_review_lane(&self, side: ReviewSide, content: AnyElement) -> AnyElement {
        if self.effective_layout() != DiffLayout::Split || side == ReviewSide::Both {
            return content;
        }
        let border = crate::theme::hairline(0.05);
        let empty = || div().w_1_2().min_w_0();
        match side {
            ReviewSide::Old => div()
                .w_full()
                .flex()
                .items_stretch()
                .child(
                    div()
                        .w_1_2()
                        .min_w_0()
                        .border_r_1()
                        .border_color(border)
                        .child(content),
                )
                .child(empty())
                .into_any_element(),
            ReviewSide::New => div()
                .w_full()
                .flex()
                .items_stretch()
                .child(empty().border_r_1().border_color(border))
                .child(div().w_1_2().min_w_0().child(content))
                .into_any_element(),
            ReviewSide::Both => content,
        }
    }

    pub(super) fn render_review_editor(
        &self,
        comment_id: &str,
        side: ReviewSide,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(editor) = self.comment_editor.clone() else {
            return gpui::Empty.into_any_element();
        };
        let delete_id = comment_id.to_string();
        let content = div()
            .w_full()
            .px(px(Theme::SPACE_MD))
            .py(px(Theme::SPACE_SM))
            .child(
                div()
                    .id(SharedString::from(format!("review-editor:{comment_id}")))
                    .w_full()
                    .p(px(Theme::SPACE_MD))
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_SM))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border_strong)
                    .bg(theme.surface_card)
                    .child(div().min_h(px(54.0)).w_full().child(editor))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(Theme::SPACE_SM))
                            .child(
                                div()
                                    .id(SharedString::from(format!("delete-review:{comment_id}")))
                                    .px(px(10.0))
                                    .py(px(5.0))
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_muted)
                                    .hover(|style| style.bg(crate::theme::wash(0.08)))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.delete_comment(&delete_id, cx);
                                    }))
                                    .child("Delete"),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("finish-review:{comment_id}")))
                                    .px(px(10.0))
                                    .py(px(5.0))
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .bg(theme.solid)
                                    .text_size(px(11.0))
                                    .text_color(theme.on_solid)
                                    .on_click(cx.listener(|this, _, _, cx| this.finish_comment(cx)))
                                    .child("Done"),
                            ),
                    ),
            )
            .into_any_element();
        self.render_review_lane(side, content)
    }

    pub(super) fn render_review_comment(
        &self,
        comment_id: &str,
        side: ReviewSide,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(comment) = self
            .diff_comments()
            .iter()
            .find(|comment| comment.id == comment_id)
        else {
            return gpui::Empty.into_any_element();
        };
        let edit_id = comment_id.to_string();
        let delete_id = comment_id.to_string();
        let content = div()
            .w_full()
            .px(px(Theme::SPACE_MD))
            .py(px(Theme::SPACE_SM))
            .child(
                div()
                    .id(SharedString::from(format!("review-comment:{comment_id}")))
                    .w_full()
                    .p(px(Theme::SPACE_MD))
                    .flex()
                    .items_start()
                    .gap(px(Theme::SPACE_MD))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.surface_card)
                    .child(
                        crate::icons::icon(crate::icons::MESSAGE_CIRCLE)
                            .size(px(15.0))
                            .text_color(theme.accent),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.0))
                            .line_height(px(18.0))
                            .text_color(theme.text)
                            .child(SharedString::from(comment.body.clone())),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("edit-review:{comment_id}")))
                            .size(px(24.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .hover(|style| style.bg(crate::theme::wash(0.08)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit_comment(edit_id.clone(), window, cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::PENCIL)
                                    .size(px(13.0))
                                    .text_color(theme.text_muted),
                            ),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("remove-review:{comment_id}")))
                            .size(px(24.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .hover(|style| style.bg(crate::theme::wash(0.08)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.delete_comment(&delete_id, cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::TRASH)
                                    .size(px(13.0))
                                    .text_color(theme.text_muted),
                            ),
                    ),
            )
            .into_any_element();
        self.render_review_lane(side, content)
    }
}
