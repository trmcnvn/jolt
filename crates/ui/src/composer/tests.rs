//! Composer behavior tests.
use super::*;
use crate::settings::composer::ComposerDefaults;
use gpui::TestAppContext;
use jolt_proto::HarnessId;

/// Manual Metal-backed capture for composer spacing work. Unlike the ordinary
/// bounds tests, this paints the real font and SVG assets so the whitespace
/// visible between glyphs can be inspected directly.
#[cfg(target_os = "macos")]
#[test]
#[ignore]
fn capture_composer_control_rows() {
    use gpui::{
        AppContext as _, HeadlessAppContext, InputEvent as _, Modifiers, MouseButton,
        MouseDownEvent, MouseMoveEvent, MouseUpEvent, point, size,
    };
    use std::sync::Arc;
    use std::time::Duration;

    for compact in [false, true] {
        let platform = gpui_platform::current_platform(false);
        let mut cx = HeadlessAppContext::with_platform(
            platform.text_system(),
            Arc::new(crate::icons::Assets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(|cx| {
            cx.set_global(Theme::default());
            crate::register_fonts(cx);
            init(cx);
        });
        let data_dir = tempfile::tempdir().expect("temporary composer settings");
        let mut defaults = ComposerDefaults::default();
        defaults.harness = Some(HarnessId::Codex);
        defaults.reasoning = Some(jolt_proto::ReasoningLevel::High);
        defaults.remember_model(HarnessId::Codex, "gpt-5.6-sol".into(), "GPT-5.6-Sol".into());
        defaults
            .save(data_dir.path())
            .expect("seed composer defaults");
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.data_dir = Some(data_dir.path().to_path_buf());
            if compact {
                state.selected_chat = Some("visual-layout-test".into());
            }
            state
        });
        let window = cx
            .open_window(size(px(820.0), px(180.0)), |_, cx| {
                cx.new(|cx| Composer::new(state, cx))
            })
            .expect("open visual composer window");
        cx.run_until_parked();
        let window: gpui::AnyWindowHandle = window.into();
        cx.update_window(window, |_, window, _| window.refresh())
            .expect("refresh composer window");
        cx.run_until_parked();
        let screenshot = cx
            .capture_screenshot(window)
            .expect("capture composer framebuffer");
        let mode = if compact { "compact" } else { "expanded" };
        let path = format!("/tmp/jolt-composer-{mode}.png");
        screenshot.save(&path).expect("save composer screenshot");
        eprintln!("saved {path}");

        let hover_y = if compact { 24.0 } else { 97.0 };
        cx.update_window(window, |_, window, cx| {
            let position = point(px(540.0), px(hover_y));
            window.dispatch_event(
                MouseMoveEvent {
                    position,
                    modifiers: Modifiers::default(),
                    pressed_button: None,
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(
                MouseDownEvent {
                    position,
                    modifiers: Modifiers::default(),
                    button: MouseButton::Left,
                    click_count: 1,
                    first_mouse: false,
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(
                MouseUpEvent {
                    position,
                    modifiers: Modifiers::default(),
                    button: MouseButton::Left,
                    click_count: 1,
                }
                .to_platform_input(),
                cx,
            );
        })
        .expect("hover model action");
        cx.advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        cx.update_window(window, |_, window, _| window.refresh())
            .expect("refresh hovered composer window");
        cx.run_until_parked();
        let screenshot = cx
            .capture_screenshot(window)
            .expect("capture hovered composer framebuffer");
        let path = format!("/tmp/jolt-composer-{mode}-model-open.png");
        screenshot
            .save(&path)
            .expect("save hovered composer screenshot");
        eprintln!("saved {path}");
    }
}

fn assert_composer_controls_are_sequential(cx: &mut TestAppContext, compact: bool) {
    cx.update(|cx| {
        cx.set_global(Theme::default());
        init(cx);
    });
    let data_dir = tempfile::tempdir().expect("temporary composer settings");
    let mut defaults = ComposerDefaults::default();
    defaults.remember_model(
        HarnessId::ClaudeCode,
        "layout-test-model".into(),
        "Layout Test Model".into(),
    );
    defaults
        .save(data_dir.path())
        .expect("seed composer defaults");
    let state = cx.new(|_| {
        let mut state = AppState::new();
        state.data_dir = Some(data_dir.path().to_path_buf());
        state
    });
    if compact {
        state.update(cx, |state, _| {
            state.selected_chat = Some("layout-test".into());
        });
    }
    let (_, cx) = cx.add_window_view(|_, cx| Composer::new(state, cx));
    cx.run_until_parked();

    let expected_mode = if compact {
        "picker-controls-compact"
    } else {
        "picker-controls-expanded"
    };
    let unexpected_mode = if compact {
        "picker-controls-expanded"
    } else {
        "picker-controls-compact"
    };
    assert!(
        cx.debug_bounds(expected_mode).is_some(),
        "test rendered the wrong composer branch"
    );
    assert!(cx.debug_bounds(unexpected_mode).is_none());

    let model = cx
        .debug_bounds("picker-model-bounds")
        .expect("model control rendered");
    let model_label = cx
        .debug_bounds("picker-model-label-bounds")
        .expect("model label rendered");
    let model_icon = cx
        .debug_bounds("picker-model-icon-bounds")
        .expect("model icon rendered");
    let traits = cx
        .debug_bounds("picker-traits-bounds")
        .expect("traits control rendered");
    let traits_label = cx
        .debug_bounds("picker-traits-label-bounds")
        .expect("traits label rendered");
    let usage = cx
        .debug_bounds("composer-usage-bounds")
        .expect("context wheel rendered");
    let attach = cx
        .debug_bounds("composer-attach-bounds")
        .expect("attach control rendered");
    let send = cx
        .debug_bounds("composer-send-bounds")
        .expect("send control rendered");
    let pill = cx
        .debug_bounds("composer-pill-bounds")
        .expect("composer pill rendered");
    let actions = compact.then(|| {
        cx.debug_bounds("composer-compact-actions-bounds")
            .expect("compact action cluster rendered")
    });
    assert!(
        model.size.width > px(0.0),
        "model control collapsed to zero width: {model:?}"
    );
    assert!(
        model_label.size.width > px(0.0),
        "model label collapsed to zero width: {model_label:?}"
    );
    assert_eq!(
        model_icon.left() - model.left(),
        px(Theme::SPACE_SM),
        "model action left padding must match its right padding"
    );
    assert_eq!(
        model.right() - model_label.right(),
        px(Theme::SPACE_SM),
        "model action right padding must match its left padding"
    );
    assert!(
        traits.size.width > px(0.0),
        "traits control collapsed to zero width: {traits:?}"
    );
    assert!(
        traits_label.size.width > px(0.0),
        "traits label collapsed to zero width: {traits_label:?}"
    );
    assert_eq!(
        traits_label.left() - traits.left(),
        px(Theme::SPACE_SM),
        "traits action left padding must match its right padding"
    );
    assert_eq!(
        traits.right() - traits_label.right(),
        px(Theme::SPACE_SM),
        "traits action right padding must match its left padding"
    );
    assert_eq!(
        usage.size, attach.size,
        "context and attachment actions must use the same button footprint"
    );
    let gaps = [
        ("model→traits", traits.left() - model.right()),
        ("traits→context", usage.left() - traits.right()),
        ("context→attach", attach.left() - usage.right()),
        ("attach→send", send.left() - attach.right()),
    ];
    for (name, gap) in gaps {
        assert_eq!(
            gap,
            px(Theme::SPACE_XS),
            "{name} gap does not match the shared composer control gap; model={model:?}, traits={traits:?}, usage={usage:?}, attach={attach:?}, send={send:?}"
        );
    }
    assert_eq!(
        traits_label.left() - model_label.right(),
        px(Theme::SPACE_SM * 2.0 + Theme::SPACE_XS),
        "visible model→traits spacing should be two inner 8px insets plus the shared 4px gap"
    );
    assert!(
        send.right() <= pill.right(),
        "send control escaped the composer pill: model={model:?}, traits={traits:?}, usage={usage:?}, attach={attach:?}, send={send:?}, actions={actions:?}, pill={pill:?}"
    );
}

#[gpui::test]
fn compact_composer_controls_do_not_overlap(cx: &mut TestAppContext) {
    assert_composer_controls_are_sequential(cx, true);
}

#[gpui::test]
fn expanded_composer_controls_do_not_overlap(cx: &mut TestAppContext) {
    assert_composer_controls_are_sequential(cx, false);
}

fn tooltip_target(range: Range<usize>, path: &str) -> MentionTooltipTarget {
    MentionTooltipTarget {
        range,
        path: path.into(),
    }
}

#[test]
fn message_history_starts_only_from_an_empty_composer() {
    assert!(can_navigate_message_history(None, ""));
    assert!(!can_navigate_message_history(None, "unsent text"));
    assert!(
        can_navigate_message_history(Some(0), "recalled prompt"),
        "an untouched recalled prompt remains in history navigation"
    );
}

#[test]
fn message_history_walks_to_the_bottom_draft_slot() {
    let mut position = None;
    position = message_history_position(position, 3, MessageHistoryDirection::Older);
    assert_eq!(position, Some(0));
    position = message_history_position(position, 3, MessageHistoryDirection::Older);
    assert_eq!(position, Some(1));
    position = message_history_position(position, 3, MessageHistoryDirection::Older);
    assert_eq!(position, Some(2));
    assert_eq!(
        message_history_position(position, 3, MessageHistoryDirection::Older),
        Some(2),
        "the oldest prompt is the top boundary"
    );
    position = message_history_position(position, 3, MessageHistoryDirection::Newer);
    assert_eq!(position, Some(1));
    position = message_history_position(position, 3, MessageHistoryDirection::Newer);
    assert_eq!(position, Some(0));
    position = message_history_position(position, 3, MessageHistoryDirection::Newer);
    assert_eq!(position, None, "the bottom slot is the current draft");
    assert_eq!(
        message_history_position(None, 3, MessageHistoryDirection::Newer),
        None
    );
    assert_eq!(
        message_history_position(None, 0, MessageHistoryDirection::Older),
        None
    );
    assert_eq!(
        message_history_position(Some(9), 3, MessageHistoryDirection::Newer),
        Some(1),
        "a transcript shrink clamps a stale position"
    );
}

#[test]
fn message_history_restores_unsent_draft_at_the_bottom() {
    let history = vec!["first prompt".to_string(), "latest prompt".to_string()];
    assert_eq!(
        message_history_text(&history, Some(0), "unsent draft"),
        "latest prompt"
    );
    assert_eq!(
        message_history_text(&history, None, "unsent draft"),
        "unsent draft"
    );
}

#[test]
fn history_contains_only_recallable_user_text() {
    let entry = |id: &str, role: MessageRole, text: &str| SessionMessageEntry {
        id: id.into(),
        role,
        parts: vec![MessagePart::Text {
            id: "t0".into(),
            text: text.into(),
        }],
        created_at: 0,
        device_id: "d".into(),
        status: None,
        continuation_of: None,
    };
    let with_attachment = attachments::with_uploaded_attachments(
        "second prompt",
        &[attachments::UploadedAttachment {
            path: "/tmp/image.png".into(),
            sha256: "0123456789abcdef".repeat(4),
        }],
    );
    let transcript = vec![
        entry("u1", MessageRole::User, "first prompt"),
        entry("a1", MessageRole::Assistant, "response"),
        entry("u2", MessageRole::User, &with_attachment),
        entry(
            "u3",
            MessageRole::User,
            &attachments::with_uploaded_attachments(
                "",
                &[attachments::UploadedAttachment {
                    path: "/tmp/only.png".into(),
                    sha256: "0123456789abcdef".repeat(4),
                }],
            ),
        ),
    ];
    let echoes = vec![entry("u4", MessageRole::User, "latest prompt")];
    assert_eq!(
        user_message_history(&transcript, &echoes),
        ["first prompt", "second prompt", "latest prompt"]
    );
}

#[test]
fn copy_prefers_composer_selection_then_transcript_selection() {
    assert_eq!(
        selected_copy_text("draft text", &(0..5), Some("response".into())).as_deref(),
        Some("draft")
    );
    assert_eq!(
        selected_copy_text("draft text", &(0..0), Some("response".into())).as_deref(),
        Some("response")
    );
    assert_eq!(selected_copy_text("draft text", &(0..0), None), None);
}

#[test]
fn mention_tooltip_wait_survives_pointer_jitter_and_promotes_once() {
    let target = tooltip_target(3..20, "src/composer.rs");
    let waiting = MentionTooltipPhase::Waiting {
        target: target.clone(),
        generation: 1,
    };
    let restarted = mention_tooltip_reduce(waiting.clone(), Some(target.clone()), false, 2);
    assert_eq!(restarted, waiting);
    assert!(matches!(
        restarted,
        MentionTooltipPhase::Waiting { generation: 1, .. }
    ));
    assert_eq!(
        mention_tooltip_promote(restarted.clone(), 2, true),
        restarted,
        "a stale timer must not reveal the tooltip"
    );
    let visible = mention_tooltip_promote(restarted, 1, true);
    assert!(matches!(
        visible,
        MentionTooltipPhase::Visible { generation: 1, .. }
    ));
    assert_eq!(
        mention_tooltip_reduce(visible.clone(), Some(target), false, 3),
        visible,
        "one visible activation keeps its presentation generation stable"
    );
}

#[test]
fn mention_tooltip_changes_target_and_cancels_disappeared_target() {
    let first = tooltip_target(0..10, "src/a.rs");
    let second = tooltip_target(20..30, "src/a.rs");
    let visible = MentionTooltipPhase::Visible {
        target: first,
        generation: 4,
    };
    assert!(matches!(
        mention_tooltip_reduce(visible, Some(second), false, 5),
        MentionTooltipPhase::Waiting { generation: 5, .. }
    ));
    assert_eq!(
        mention_tooltip_promote(
            MentionTooltipPhase::Waiting {
                target: tooltip_target(20..30, "src/a.rs"),
                generation: 5,
            },
            5,
            false,
        ),
        MentionTooltipPhase::Hidden
    );
}

#[test]
fn mention_tooltip_stays_visible_over_chip_or_popup_only() {
    assert!(mention_tooltip_contains(true, false));
    assert!(mention_tooltip_contains(false, true));
    assert!(!mention_tooltip_contains(false, false));
}

#[test]
fn mention_wash_moves_wholly_to_the_next_visual_row_at_a_wrap() {
    assert_eq!(
        display_row_segments(12..24, [12, 40]),
        vec![(1, 12, 12..24)]
    );
    assert_eq!(
        display_row_segments(8..24, [12, 40]),
        vec![(0, 0, 8..12), (1, 12, 12..24)]
    );
}

#[test]
fn shell_commands_match_universal_prefix_semantics() {
    assert_eq!(shell_scope("!"), Some(ShellScope::AgentContext));
    assert_eq!(shell_scope("!!"), Some(ShellScope::LocalOnly));
    assert_eq!(shell_scope("!!!"), None);
    assert_eq!(shell_scope("!!!! echo normally"), None);
    assert_eq!(shell_scope("  ! pwd"), Some(ShellScope::AgentContext));
    assert_eq!(shell_scope("ordinary prompt"), None);
    assert_eq!(
        shell_command("! cargo test "),
        Some(ShellCommand {
            command: "cargo test".into(),
            exclude_from_context: false,
        })
    );
    assert_eq!(
        shell_command("!!pwd"),
        Some(ShellCommand {
            command: "pwd".into(),
            exclude_from_context: true,
        })
    );
    assert!(shell_command("!").is_none());
    assert!(shell_command("!!").is_none());
    assert!(shell_command("!!!echo nope").is_none());
    assert!(shell_command("ordinary prompt").is_none());
    let pending = bash_pending_transcript("printf '```'");
    assert!(pending.contains("$ printf '```'"));
    assert!(pending.ends_with("_Output pending…_"));
}

#[test]
fn slash_command_cache_key_ignores_model_option_insertion_order() {
    let mut left = serde_json::Map::new();
    left.insert("trust".into(), serde_json::json!("yes"));
    left.insert("tools".into(), serde_json::json!("read"));
    let mut right = serde_json::Map::new();
    right.insert("tools".into(), serde_json::json!("read"));
    right.insert("trust".into(), serde_json::json!("yes"));
    assert_eq!(
        command_model_options_key(&left),
        command_model_options_key(&right)
    );
}

#[test]
fn slash_command_cache_evicts_the_least_recently_used_project() {
    let now = Instant::now();
    let key = |index: usize| CommandCacheKey {
        harness: jolt_proto::HarnessId::Mock,
        target_device: "device".into(),
        cwd: format!("/project/{index}"),
        model_options: "{}".into(),
    };
    let mut cache = HashMap::new();
    for index in 0..=COMMAND_CACHE_CAPACITY {
        let mut entry = CommandCacheEntry::empty(now);
        entry.last_used = now - Duration::from_secs((COMMAND_CACHE_CAPACITY - index) as u64);
        cache.insert(key(index), entry);
    }
    prune_command_cache(&mut cache);
    assert_eq!(cache.len(), COMMAND_CACHE_CAPACITY);
    assert!(!cache.contains_key(&key(0)));
    assert!(cache.contains_key(&key(COMMAND_CACHE_CAPACITY)));
}

#[test]
fn slash_command_cache_refreshes_stale_entries_with_failure_backoff() {
    let now = Instant::now();
    assert!(command_cache_should_fetch(None, None, now));
    assert!(!command_cache_should_fetch(Some(now), None, now));

    let stale = now - COMMAND_CACHE_TTL;
    assert!(command_cache_should_fetch(Some(stale), None, now));
    assert!(!command_cache_should_fetch(Some(stale), Some(now), now));
    assert!(command_cache_should_fetch(
        Some(stale),
        Some(now - COMMAND_CACHE_FAILURE_RETRY),
        now,
    ));
}

#[test]
fn slash_commands_only_complete_the_leading_token() {
    assert_eq!(
        slash_command_token("/review", 7),
        Some(SlashCommandToken {
            range: 0..7,
            query: "review".into(),
        })
    );
    assert_eq!(
        slash_command_token("/review later", 4).map(|token| token.range),
        Some(0..7)
    );
    assert!(slash_command_token("/review later", 10).is_none());
    assert!(slash_command_token("try /review", 11).is_none());
    assert!(slash_command_token("/réview", 3).is_none());
}

#[test]
fn built_in_commands_filter_and_sort() {
    let command = |name: &str, description: Option<&str>| AgentCommand {
        name: name.into(),
        description: description.map(str::to_owned),
        argument_hint: None,
        source: AgentCommandSource::Jolt,
    };
    let catalog = vec![
        command("zebra", None),
        command("answer", Some("Answer questions")),
        AgentCommand {
            name: "discovered".into(),
            description: None,
            argument_hint: None,
            source: AgentCommandSource::Extension,
        },
    ];
    let commands = filtered_commands(&catalog, "");
    assert_eq!(
        commands
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        ["answer", "zebra"]
    );
    assert_eq!(filtered_commands(&catalog, "questions")[0].name, "answer");
}

#[test]
fn native_commands_require_exact_invocations() {
    assert!(is_answer_questions_command("/answer "));
    assert!(!is_answer_questions_command("/answer later"));
    assert!(is_bro_command("/bro"));
    assert!(!is_bro_command("/bro please"));
    assert_eq!(
        BRO_PROMPT,
        "Restate your last message. Stop using jargon and speak coherently. State it more simply and concisely, like one human talking to another."
    );
    assert!(is_goal_command("/goal"));
    assert!(!is_goal_command("/goal pause"));
    assert!(!is_goal_command(
        "/goal --tokens 12000 finish the migration"
    ));
    assert!(!is_goal_command("/goalkeeper"));
}

#[test]
fn mention_token_requires_a_token_boundary_and_tracks_full_token() {
    assert_eq!(
        mention_token("Fix @src/com", 12),
        Some(MentionToken {
            range: 4..12,
            query: "src/com".into(),
        })
    );
    assert!(mention_token("mail@example.com", 16).is_none());
    assert!(mention_token("word@file", 9).is_none());
    assert!(mention_token("path/@file", 10).is_none());
    assert_eq!(
        mention_token("See (@lib", 9).map(|token| token.range),
        Some(5..9)
    );
}

#[test]
fn dismissed_mentions_reject_stale_responses() {
    let mut state = FileMentionState {
        token: mention_token("@src", 4),
        request: 7,
        ..FileMentionState::default()
    };
    assert!(mention_response_is_current(&state, 7));
    state.request += 1;
    state.token = None;
    assert!(!mention_response_is_current(&state, 7));
    assert!(!mention_response_is_current(&state, 8));
}

#[test]
fn file_mentions_serialize_to_strict_local_markdown() {
    let raw = local_file_link("src/a file#[x].rs", false);
    assert_eq!(
        raw,
        "[a file#\\[x\\].rs](jolt-file:src/a%20file%23%5Bx%5D.rs)"
    );
    let links = file_mention_links(&raw);
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].path, "src/a file#[x].rs");
    assert_eq!(links[0].basename, "a file#[x].rs");
    assert!(!links[0].is_dir);

    let folder = local_file_link("src/components", true);
    assert_eq!(folder, "[components](jolt-file:src/components/)");
    let links = file_mention_links(&folder);
    assert_eq!(links[0].path, "src/components");
    assert!(links[0].is_dir);
}

#[test]
fn file_mentions_reject_external_or_noncanonical_markdown() {
    assert!(file_mention_links("[site](https://example.com/a)").is_empty());
    assert!(file_mention_links("[a.rs](../a.rs)").is_empty());
    assert!(file_mention_links("[a.rs](src/a file.rs)").is_empty());
    assert!(file_mention_links("[other](src/a.rs)").is_empty());
    assert!(file_mention_links("[a.rs](src/a.rs)").is_empty());
    assert!(file_mention_links("[a.rs](src%5Cfake%5Ca.rs)").is_empty());
    assert!(file_mention_links("[a.rs](src/a%0A.rs)").is_empty());
}

#[test]
fn duplicate_mention_basenames_use_unique_suffixes() {
    let raw = format!(
        "{} {}",
        local_file_link("src/one/mod.rs", false),
        local_file_link("src/two/mod.rs", false)
    );
    let projection = TextProjection::new(&raw);
    assert!(projection.display.contains("one/mod.rs"));
    assert!(projection.display.contains("two/mod.rs"));
}

#[test]
fn mention_suffixes_compare_path_components() {
    let links = vec![
        FileMentionLink {
            range: 0..0,
            basename: "mod.rs".into(),
            path: "foo/mod.rs".into(),
            is_dir: false,
        },
        FileMentionLink {
            range: 0..0,
            basename: "oomod.rs".into(),
            path: "bar/oomod.rs".into(),
            is_dir: false,
        },
    ];
    assert_eq!(
        mention_display_labels(&links),
        vec!["mod.rs".to_string(), "oomod.rs".to_string()]
    );
}

#[test]
fn projection_maps_and_expands_atomic_chip_ranges() {
    let raw = format!("open {} now", local_file_link("src/composer.rs", false));
    let projection = TextProjection::new(&raw);
    let (link, chip) = &projection.mentions[0];
    assert_eq!(
        &projection.display[chip.clone()],
        "\u{00A0}@composer.rs\u{00A0}"
    );
    assert_eq!(projection.display_to_raw(chip.start + 1), link.range.start);
    assert_eq!(projection.display_to_raw(chip.end - 1), link.range.end);
    assert_eq!(
        projection.previous_boundary(link.range.end),
        Some(link.range.start)
    );
    assert_eq!(
        projection.next_boundary(link.range.start),
        Some(link.range.end)
    );
    assert_eq!(
        projection.normalize_range(link.range.start + 2..link.range.end - 2),
        link.range
    );
}

#[test]
fn sent_mention_display_projects_chips_for_the_transcript() {
    let raw = format!(
        "check {} and {}",
        local_file_link("src/composer.rs", false),
        local_file_link("src/components", true)
    );
    let (display, spans) = sent_mention_display(&raw).expect("mentions project");
    assert!(!display.contains(FILE_MENTION_SCHEME));
    assert!(display.contains("composer.rs"));
    assert!(display.contains("components"));
    assert_eq!(spans.len(), 2);
    assert_eq!(
        &display[spans[0].range.clone()],
        "\u{00A0}@composer.rs\u{00A0}"
    );
    assert!(!spans[0].is_dir);
    assert_eq!(spans[0].path.as_ref(), "src/composer.rs");
    assert!(spans[1].is_dir);
    assert_eq!(spans[1].path.as_ref(), "src/components/");
}

/// Ordinary prompts must stay on the zero-cost path, including ones that
/// merely *talk about* the scheme without containing a valid mention.
#[test]
fn sent_mention_display_leaves_plain_prompts_untouched() {
    assert_eq!(sent_mention_display("fix the composer"), None);
    assert_eq!(
        sent_mention_display("what is a jolt-file: link?"),
        None,
        "scheme substring without a valid mention link"
    );
    assert_eq!(
        sent_mention_display("[a.rs](jolt-file:../a.rs)"),
        None,
        "a hostile path never becomes a chip in the transcript either"
    );
}

#[test]
fn extracted_answers_restore_pages_and_compile_message() {
    let mut wizard = ExtractedWizard::new(vec![
        ExtractedQuestion {
            question: "Which database?".into(),
            context: Some("MySQL and PostgreSQL are supported.".into()),
        },
        ExtractedQuestion {
            question: "Enable caching?".into(),
            context: None,
        },
    ]);
    wizard.save("PostgreSQL".into());
    assert!(!wizard.advance());
    wizard.save(String::new());
    assert!(wizard.back());
    assert_eq!(wizard.current_answer(), "PostgreSQL");
    assert!(!wizard.advance());
    assert!(wizard.advance());
    assert_eq!(
        wizard.compiled_message(),
        "I answered your questions in the following way:\n\nQ: Which database?\n> MySQL and PostgreSQL are supported.\nA: PostgreSQL\n\nQ: Enable caching?\nA: (no answer)"
    );
}

#[test]
fn latest_answerable_message_uses_completed_assistant_text() {
    let transcript = vec![
        SessionMessageEntry {
            id: "a1".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: "First question?".into(),
            }],
            created_at: 1,
            device_id: "d".into(),
            status: Some(jolt_session_doc::MessageStatus::Complete),
            continuation_of: None,
        },
        SessionMessageEntry {
            id: "u1".into(),
            role: MessageRole::User,
            parts: vec![],
            created_at: 2,
            device_id: "d".into(),
            status: Some(jolt_session_doc::MessageStatus::Complete),
            continuation_of: None,
        },
    ];
    assert_eq!(
        latest_answerable_message(&transcript),
        Some(("a1".into(), "First question?".into()))
    );
}

fn question(id: &str, options: &[&str], multi: bool) -> UserInputQuestion {
    UserInputQuestion {
        id: id.into(),
        header: "Header".into(),
        question: format!("Question {id}"),
        options: options.iter().map(|s| s.to_string()).collect(),
        multi_select: multi,
    }
}

#[test]
fn flip_decision() {
    // Fits in the pill → compact stays compact.
    assert!(!composer_flip(false, 150.0, 300.0, false, false));
    // Overflow → expand.
    assert!(composer_flip(false, 320.0, 300.0, false, false));
    // Newline always expands (either mode, even mid-resize).
    assert!(composer_flip(false, 10.0, 300.0, true, false));
    assert!(composer_flip(true, 10.0, 300.0, true, true));
    // Narrow column (< MIN_COMPACT_INPUT_WIDTH) always expands.
    assert!(composer_flip(false, 10.0, 199.0, false, false));
    assert!(!composer_flip(false, 10.0, 200.0, false, false));
}

#[test]
fn flip_hysteresis_band_prevents_oscillation() {
    let cap = 300.0;
    // Text just over capacity expands…
    assert!(composer_flip(false, cap + 1.0, cap, false, false));
    // …and the SAME width, now expanded, does NOT collapse back — the
    // collapse threshold sits COLLAPSE_HYSTERESIS below the expand one.
    assert!(composer_flip(true, cap + 1.0, cap, false, false));
    // Anywhere inside the band the two modes are both stable (no width in
    // (cap - 32, cap] flips in either direction).
    let in_band = cap - COLLAPSE_HYSTERESIS + 1.0;
    assert!(!composer_flip(false, in_band, cap, false, false));
    assert!(composer_flip(true, in_band, cap, false, false));
    // Comfortably under the band → collapses.
    assert!(!composer_flip(
        true,
        cap - COLLAPSE_HYSTERESIS - 1.0,
        cap,
        false,
        false
    ));
}

#[test]
fn flip_frozen_during_interactive_resize() {
    // While resizing, both modes hold even across their thresholds…
    assert!(!composer_flip(false, 500.0, 300.0, false, true));
    assert!(composer_flip(true, 0.0, 300.0, false, true));
    // …including the narrow-column force-expand.
    assert!(!composer_flip(false, 10.0, 150.0, false, true));
    // Once settled, the same inputs flip.
    assert!(composer_flip(false, 500.0, 300.0, false, false));
    assert!(!composer_flip(true, 0.0, 300.0, false, false));
    assert!(composer_flip(false, 10.0, 150.0, false, false));
}

#[test]
fn caret_blink_phase() {
    // Solid through the first half-period (typing burst never blinks).
    assert!(caret_visible(0));
    assert!(caret_visible(CARET_BLINK_MS - 1));
    // Off for the second half-period, back on for the third.
    assert!(!caret_visible(CARET_BLINK_MS));
    assert!(!caret_visible(2 * CARET_BLINK_MS - 1));
    assert!(caret_visible(2 * CARET_BLINK_MS));
}

#[test]
fn auto_grow_math() {
    // Textarea clamp + actions row + hairlines: 76+46+2 empty through
    // 260+46+2 capped.
    assert_eq!(COMPOSER_MIN_HEIGHT, 124.0);
    assert_eq!(COMPOSER_MAX_HEIGHT, 308.0);
    // One line sits at the floor: the textarea BOX (content + `pt-4 pb-1`)
    // clamps UP to 76 exactly like `Math.max(scrollHeight, 76)` — this is
    // what makes the always-expanded new-chat composer 124px tall.
    assert_eq!(
        composer_total_height(input_content_height(1)),
        COMPOSER_MIN_HEIGHT
    );
    // Growth is linear once the textarea box exceeds its 76px floor.
    let h4 = composer_total_height(input_content_height(4));
    assert_eq!(
        h4,
        4.0 * INPUT_LINE_HEIGHT + TEXTAREA_PAD_V + ACTIONS_ROW_HEIGHT + PILL_BORDER_V
    );
    // Caps at a 260px textarea box (jolt max-h-[260px] / the JS clamp).
    assert_eq!(
        composer_total_height(input_content_height(100)),
        COMPOSER_MAX_HEIGHT
    );
    // Zero lines still measures one.
    assert_eq!(input_content_height(0), INPUT_LINE_HEIGHT);
}

#[test]
fn input_wheel_scroll_uses_gpui_direction_and_clamps() {
    // Positive wheel delta moves toward the start; negative moves down.
    assert_eq!(input_scroll_offset(40.0, 20.0, 200.0, 100.0), 20.0);
    assert_eq!(input_scroll_offset(40.0, -30.0, 200.0, 100.0), 70.0);
    // Neither edge can be overscrolled.
    assert_eq!(input_scroll_offset(10.0, 50.0, 200.0, 100.0), 0.0);
    assert_eq!(input_scroll_offset(90.0, -50.0, 200.0, 100.0), 100.0);
    // Short content has no internal scroll range.
    assert_eq!(input_scroll_offset(20.0, -50.0, 80.0, 100.0), 0.0);
}

#[test]
fn input_scroll_reveals_only_when_caret_leaves_viewport() {
    // A visible caret preserves the user's viewport.
    assert_eq!(
        input_scroll_offset_for_cursor(40.0, 60.0, 20.0, 300.0, 100.0),
        40.0
    );
    // Moving above or below reveals the row with the smallest adjustment.
    assert_eq!(
        input_scroll_offset_for_cursor(80.0, 30.0, 20.0, 300.0, 100.0),
        30.0
    );
    assert_eq!(
        input_scroll_offset_for_cursor(20.0, 130.0, 20.0, 300.0, 100.0),
        50.0
    );
    // Revealing the final row clamps exactly to the content end.
    assert_eq!(
        input_scroll_offset_for_cursor(0.0, 290.0, 20.0, 300.0, 100.0),
        200.0
    );
}

#[test]
fn input_drag_autoscroll_is_edge_proportional_and_capped() {
    let top = 100.0;
    let bottom = 300.0;
    let line = INPUT_LINE_HEIGHT;
    assert_eq!(input_drag_scroll_delta(200.0, top, bottom, line), 0.0);
    assert_eq!(input_drag_scroll_delta(90.0, top, bottom, line), -2.0);
    assert_eq!(input_drag_scroll_delta(315.0, top, bottom, line), 3.0);
    assert_eq!(input_drag_scroll_delta(-100.0, top, bottom, line), -line);
    assert_eq!(input_drag_scroll_delta(500.0, top, bottom, line), line);
}

/// One frame short of the full morph timeline (never rounds up to done).
const ALMOST: f32 = 179.0;

#[test]
fn flip_morph_starts_once_per_committed_flip() {
    // No committed flip → no morph.
    assert_eq!(flip_morph_step(None, false, 49.0, 0.0, false, false), None);
    // A committed flip starts one, from the last rendered height…
    let m = flip_morph_step(None, true, 49.0, 100.0, false, false).unwrap();
    assert_eq!(m.from, 49.0);
    assert_eq!(m.start_ms, 100.0);
    // …and same-mode renders keep it UNCHANGED (no restart at the
    // boundary, whatever the heights are doing).
    assert_eq!(
        flip_morph_step(Some(m), false, 80.0, 150.0, false, false),
        Some(m)
    );
    // A finished morph clears on the next same-mode render.
    assert_eq!(
        flip_morph_step(Some(m), false, 124.0, 100.0 + ALMOST, false, false),
        Some(m)
    );
    assert_eq!(
        flip_morph_step(Some(m), false, 124.0, 300.0, false, false),
        None
    );
}

#[test]
fn flip_morph_height_ramps_monotonically_to_target() {
    let m = FlipMorph {
        from: 49.0,
        start_ms: 0.0,
    };
    // Starts exactly at the committed height…
    let mut prev = m.height(124.0, 0.0);
    assert_eq!(prev, 49.0);
    // …ramps without ever moving backwards…
    for step in 1..=18 {
        let h = m.height(124.0, step as f32 * 10.0);
        assert!(h >= prev, "height regressed at {step}: {h} < {prev}");
        prev = h;
    }
    // …and lands exactly on the target when done (and stays there).
    assert_eq!(m.height(124.0, 180.0), 124.0);
    assert!(m.done(180.0));
    assert_eq!(m.height(124.0, 500.0), 124.0);
    // Collapse runs the same ramp downward.
    assert!(m.height(124.0, 90.0) > 49.0);
    let down = FlipMorph {
        from: 124.0,
        start_ms: 0.0,
    };
    assert!(down.height(49.0, 90.0) < 124.0);
    assert!(down.height(49.0, 90.0) > 49.0);
}

#[test]
fn flip_morph_reverse_hands_off_from_current_height() {
    let m = FlipMorph {
        from: 49.0,
        start_ms: 0.0,
    };
    let mid = m.height(124.0, 90.0);
    assert!(mid > 49.0 && mid < 124.0);
    // A reverse flip mid-flight commits a new morph FROM the animated
    // height — continuous at the handoff, no pop to an endpoint.
    let rev = flip_morph_step(Some(m), true, mid, 90.0, false, false).unwrap();
    assert_eq!(rev.from, mid);
    assert_eq!(rev.height(49.0, 90.0), mid);
}

#[test]
fn flip_morph_snaps_for_reduced_motion_and_first_paint() {
    // Reduced motion never creates a morph (the flip just snaps)…
    assert_eq!(flip_morph_step(None, true, 49.0, 0.0, true, false), None);
    // …and neither does a flip before anything was ever rendered.
    assert_eq!(flip_morph_step(None, true, 0.0, 0.0, false, false), None);
}

#[test]
fn route_change_never_arms_the_morph() {
    // A flip committed inside the route-snap window must NOT animate —
    // switching sessions (chat↔chat or chat↔new-session) snaps the
    // composer straight to the target mode, like the header (round 6).
    assert_eq!(flip_morph_step(None, true, 49.0, 0.0, false, true), None);
    // The route change also kills anything already in flight…
    let m = FlipMorph {
        from: 49.0,
        start_ms: 0.0,
    };
    assert_eq!(
        flip_morph_step(Some(m), false, 80.0, 50.0, false, true),
        None
    );
    assert_eq!(
        flip_morph_step(Some(m), true, 80.0, 50.0, false, true),
        None
    );
    // …while outside the window the same flip animates as usual.
    let armed = flip_morph_step(None, true, 49.0, 300.0, false, false).unwrap();
    assert_eq!(armed.from, 49.0);
}

#[test]
fn morph_anchoring_holds_controls_and_glides_text() {
    // Steady state (progress 1): no offsets, everything at rest.
    assert_eq!(morph_cluster_dy(1.0), 0.0);
    assert_eq!(morph_text_pad(1.0), 16.0);
    assert_eq!(collapse_text_glide(124.0, 1.0), 0.0);
    // At the commit instant the pieces start from the OLD mode's resting
    // geometry: text pad at the compact 12px inset, cluster displaced by
    // exactly the 2.5px centering delta.
    assert_eq!(morph_text_pad(0.0), 12.0);
    assert_eq!(morph_cluster_dy(0.0), CLUSTER_Y_DELTA);
    // Collapse glide: starts where the expanded text sat (17px below the
    // committed pill top → `from − 53` above the compact resting spot)…
    assert_eq!(collapse_text_glide(124.0, 0.0), 71.0);
    // …decays monotonically to zero…
    let mut prev = collapse_text_glide(124.0, 0.0);
    for step in 1..=10 {
        let g = collapse_text_glide(124.0, step as f32 / 10.0);
        assert!(g <= prev, "glide regressed at {step}");
        prev = g;
    }
    // …and can't go negative on shallow mid-flight reversals.
    assert_eq!(collapse_text_glide(50.0, 0.0), 0.0);
}

#[test]
fn flip_morph_tracks_live_target_and_drives_fade() {
    let m = FlipMorph {
        from: 49.0,
        start_ms: 0.0,
    };
    // Auto-grow can move the target mid-morph: evaluation tracks the
    // live value instead of finishing on a stale height.
    assert!(m.height(159.0, 90.0) > m.height(124.0, 90.0));
    // The eased progress is the actions-row fade: 0 at commit, 1 at rest.
    assert_eq!(m.progress(0.0), 0.0);
    assert_eq!(m.progress(180.0), 1.0);
    let mid = m.progress(90.0);
    assert!(mid > 0.0 && mid < 1.0);
}

#[test]
fn busy_composer_invites_a_follow_up() {
    assert_eq!(composer_placeholder(false), DEFAULT_PLACEHOLDER);
    assert_eq!(composer_placeholder(true), BUSY_PLACEHOLDER);
    assert!(BUSY_PLACEHOLDER.contains("queues next"));
}

#[test]
fn send_button_morph() {
    assert_eq!(send_button_mode(false, false), SendButtonMode::Send);
    assert_eq!(send_button_mode(false, true), SendButtonMode::Send);
    assert_eq!(send_button_mode(true, true), SendButtonMode::Steer);
    assert_eq!(send_button_mode(true, false), SendButtonMode::Stop);
}

#[test]
fn generated_reviews_do_not_consume_editor_state() {
    assert!(SubmissionOrigin::Editor.uses_editor_state());
    assert!(
        !SubmissionOrigin::GeneratedReview {
            review_id: "review".into()
        }
        .uses_editor_state()
    );
}

#[test]
fn wizard_single_select_auto_advances_and_completes() {
    let mut w = Wizard::new(
        "req".into(),
        vec![
            question("q1", &["a", "b"], false),
            question("q2", &["x"], false),
        ],
    );
    assert_eq!(w.counter(), "1/2");
    assert_eq!(w.select(1), WizardStep::AutoAdvance);
    assert!(w.is_picked(1));
    assert_eq!(w.advance(), WizardStep::Stay);
    assert_eq!(w.counter(), "2/2");
    assert_eq!(w.select(0), WizardStep::Stay);
    let WizardStep::Done(answers) = w.advance() else {
        panic!("expected Done")
    };
    assert_eq!(answers.len(), 2);
    assert_eq!(answers[0].labels, vec!["b"]);
    assert_eq!(answers[1].labels, vec!["x"]);
}

#[test]
fn wizard_multi_select_toggles_and_stays() {
    let mut w = Wizard::new("req".into(), vec![question("q", &["a", "b", "c"], true)]);
    assert_eq!(w.select(0), WizardStep::Stay);
    assert_eq!(w.select(2), WizardStep::Stay);
    assert!(w.is_picked(0) && w.is_picked(2));
    // Toggle off.
    assert_eq!(w.select(0), WizardStep::Stay);
    assert!(!w.is_picked(0));
    let WizardStep::Done(answers) = w.advance() else {
        panic!()
    };
    assert_eq!(answers[0].labels, vec!["c"]);
}

#[test]
fn wizard_number_keys_and_bounds() {
    let mut w = Wizard::new("req".into(), vec![question("q", &["a", "b"], false)]);
    assert_eq!(w.press_number(9), WizardStep::Stay, "out of range ignored");
    assert_eq!(w.press_number(0), WizardStep::Stay);
    assert_eq!(w.press_number(2), WizardStep::Stay);
    assert!(w.is_picked(1));
    assert_eq!(w.select(5), WizardStep::Stay, "bad option ix ignored");
}

#[test]
fn wizard_typed_answer_overrides_and_back_pages() {
    let mut w = Wizard::new(
        "req".into(),
        vec![
            question("q1", &["a"], false),
            question("q2", &["x", "y"], false),
        ],
    );
    w.select(0);
    w.advance();
    assert_eq!(w.page, 1);
    assert!(w.back());
    assert_eq!(w.page, 0);
    assert!(!w.back(), "already at first page");
    w.advance();
    w.set_typed("  custom answer  ".into());
    assert_eq!(w.current_typed(), "  custom answer  ");
    let WizardStep::Done(answers) = w.advance() else {
        panic!()
    };
    assert_eq!(answers[0].labels, vec!["a"]);
    assert_eq!(
        answers[1].labels,
        vec!["custom answer"],
        "typed overrides picked, trimmed"
    );
}

#[test]
fn pending_input_detection() {
    use jolt_session_doc::MessageStatus;
    let input_part = MessagePart::Input {
        id: "in-r1".into(),
        request_id: "r1".into(),
        questions: vec![question("q", &["a"], false)],
        resolved: false,
    };
    let entry = |status: Option<MessageStatus>, parts: Vec<MessagePart>| SessionMessageEntry {
        id: "m".into(),
        role: MessageRole::Assistant,
        parts,
        created_at: 0,
        device_id: "d".into(),
        status,
        continuation_of: None,
    };
    // Streaming entry with unresolved input → panel.
    let t = vec![entry(
        Some(MessageStatus::Streaming),
        vec![input_part.clone()],
    )];
    assert_eq!(
        pending_input_request(&t).map(|(id, _)| id),
        Some("r1".into())
    );
    // DEAD entry with an unresolved input STILL gets the panel: the
    // question stays answerable until answered (the engine delivers the
    // answer as a resumed turn), so a run reaped under its question —
    // engine restart — must not orphan it (user report).
    let t = vec![entry(
        Some(MessageStatus::Aborted),
        vec![input_part.clone()],
    )];
    assert_eq!(
        pending_input_request(&t).map(|(id, _)| id),
        Some("r1".into())
    );
    // A NEWER assistant entry supersedes an unanswered question.
    let t = vec![
        entry(Some(MessageStatus::Aborted), vec![input_part.clone()]),
        SessionMessageEntry {
            id: "m2".into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                id: "t2".into(),
                text: "moved on".into(),
            }],
            created_at: 2,
            device_id: "d".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        },
    ];
    assert!(pending_input_request(&t).is_none());
    // Resolved part → no panel.
    let resolved = MessagePart::Input {
        id: "in-r1".into(),
        request_id: "r1".into(),
        questions: vec![],
        resolved: true,
    };
    let t = vec![entry(
        Some(MessageStatus::Streaming),
        vec![resolved.clone()],
    )];
    assert!(pending_input_request(&t).is_none());
    assert!(pending_input_request(&[]).is_none());

    // Regression (user forensics): a steer prompt appends a USER entry
    // AFTER the streaming assistant entry — the question must still be
    // found (a last-entry-only read vanished the panel exactly when the
    // user typed, bricking the answer flow).
    let user_echo = SessionMessageEntry {
        id: "u2".into(),
        role: MessageRole::User,
        parts: vec![MessagePart::Text {
            id: "t".into(),
            text: "I answered".into(),
        }],
        created_at: 1,
        device_id: "d".into(),
        status: Some(MessageStatus::Complete),
        continuation_of: None,
    };
    let t = vec![
        entry(Some(MessageStatus::Streaming), vec![input_part.clone()]),
        user_echo,
    ];
    assert_eq!(
        pending_input_request(&t).map(|(id, _)| id),
        Some("r1".into()),
        "question survives entries appended behind the streaming entry"
    );

    // Latch release: only an explicitly resolved matching part releases.
    assert!(!input_request_resolved(&t, "r1"));
    let t = vec![entry(Some(MessageStatus::Streaming), vec![resolved])];
    assert!(input_request_resolved(&t, "r1"));
    assert!(!input_request_resolved(&t, "other"));
}
