//! Module behavior tests.
use super::*;
use jolt_proto::{FolderEntry, Model, ModelOption, ModelOptionChoice};

fn repo_ref(
    name: &str,
    kind: jolt_proto::RepoRefKind,
    current: bool,
    worktree_path: Option<&str>,
) -> RepoRef {
    RepoRef {
        name: name.into(),
        revision: name.into(),
        kind,
        current,
        worktree_path: worktree_path.map(str::to_string),
    }
}

#[test]
fn empty_search_keeps_the_selected_row_active() {
    assert_eq!(search_active_index("", 2), 2);
    assert_eq!(search_active_index("   ", 2), 2);
    assert_eq!(search_active_index("jo", 2), 0);
}

#[test]
fn context_pressure_changes_at_seventy_and_ninety_percent() {
    let usage = |tokens| UsageSummary {
        context_tokens: Some(tokens),
        context_window: Some(100),
        ..UsageSummary::default()
    };
    assert_eq!(context_pressure(Some(&usage(69))), ContextPressure::Normal);
    assert_eq!(context_pressure(Some(&usage(70))), ContextPressure::Warning);
    assert_eq!(context_pressure(Some(&usage(89))), ContextPressure::Warning);
    assert_eq!(context_pressure(Some(&usage(90))), ContextPressure::Danger);
    assert_eq!(context_pressure(None), ContextPressure::Normal);
}

#[test]
fn usage_values_have_clean_compact_labels() {
    assert_eq!(format_tokens(999), "999");
    assert_eq!(format_tokens(17_000), "17k");
    assert_eq!(format_tokens(17_500), "17.5k");
    assert_eq!(format_tokens(2_000_000), "2m");

    let usage = UsageSummary {
        context_tokens: Some(17_000),
        context_window: Some(258_000),
        ..UsageSummary::default()
    };
    assert_eq!(format_context(&usage), "6.6% · 17k/258k");
}

#[test]
fn session_checkout_ref_defaults_to_the_jj_working_copy() {
    let refs = [
        repo_ref("abcdef12", jolt_proto::RepoRefKind::WorkingCopy, true, None),
        repo_ref(
            "12345678",
            jolt_proto::RepoRefKind::WorkingCopy,
            false,
            Some("/repo/other"),
        ),
        repo_ref("main", jolt_proto::RepoRefKind::Bookmark, false, None),
    ];

    assert_eq!(
        session_checkout_ref(&refs, Some("main"), Some("/repo"), true).map(|row| &*row.name),
        Some("abcdef12")
    );
    assert_eq!(
        session_checkout_ref(&refs, None, Some("/repo/other"), false).map(|row| &*row.name),
        Some("12345678")
    );
}

#[test]
fn pi_uses_its_brand_mark() {
    assert_eq!(harness_brand_icon(HarnessId::Pi).0, crate::icons::PI_MARK);
}

#[test]
fn traits_summary_formats_non_defaults() {
    let model = Model {
        id: "opus".into(),
        label: "Opus".into(),
        description: None,
        reasoning_levels: vec![ReasoningLevel::Medium, ReasoningLevel::High],
        options: vec![
            ModelOption {
                id: "context".into(),
                label: "Context window".into(),
                choices: vec![
                    ModelOptionChoice {
                        id: "standard".into(),
                        label: "Standard".into(),
                    },
                    ModelOptionChoice {
                        id: "1m".into(),
                        label: "1M".into(),
                    },
                ],
                default_choice: "standard".into(),
            },
            ModelOption {
                id: "speed".into(),
                label: "Speed".into(),
                choices: vec![
                    ModelOptionChoice {
                        id: "normal".into(),
                        label: "Normal".into(),
                    },
                    ModelOptionChoice {
                        id: "fast".into(),
                        label: "Fast".into(),
                    },
                ],
                default_choice: "normal".into(),
            },
        ],
    };
    let mut selections = serde_json::Map::new();
    selections.insert("context".into(), serde_json::Value::String("1m".into()));
    selections.insert("speed".into(), serde_json::Value::String("fast".into()));
    assert_eq!(
        traits_summary(Some(&model), Some(ReasoningLevel::High), &selections),
        Some("High · 1M · Fast".to_string())
    );
    // All defaults → no summary.
    assert_eq!(
        traits_summary(Some(&model), None, &serde_json::Map::new()),
        None
    );
    // Default-choice selections don't count as non-default.
    let mut defaults = serde_json::Map::new();
    defaults.insert("speed".into(), serde_json::Value::String("normal".into()));
    assert_eq!(traits_summary(Some(&model), None, &defaults), None);
    // Reasoning shows without a model too.
    assert_eq!(
        traits_summary(
            None,
            Some(ReasoningLevel::Ultrathink),
            &serde_json::Map::new()
        ),
        Some("Ultrathink".to_string())
    );
}

#[test]
fn folder_paths_and_breadcrumbs() {
    assert_eq!(parent_path("/home/w/dev"), Some("/home/w".to_string()));
    assert_eq!(parent_path("/home"), Some("/".to_string()));
    assert_eq!(parent_path("/home/"), Some("/".to_string()));
    assert_eq!(parent_path("/"), None);
    assert_eq!(parent_path(""), None);
    assert_eq!(child_path("/home", "w"), "/home/w");
    assert_eq!(child_path("/", "home"), "/home");
    let crumbs = breadcrumbs("/home/w/dev");
    let labels: Vec<&str> = crumbs.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(labels, ["/", "home", "w", "dev"]);
    assert_eq!(crumbs[2].1, "/home/w");
    assert_eq!(breadcrumbs("/").len(), 1);
}

#[test]
fn browser_navigation_reducer() {
    let listing = FolderListing {
        path: "/home/w".into(),
        entries: vec![
            FolderEntry {
                name: "notes.txt".into(),
                is_dir: false,
                is_repo: false,
            },
            FolderEntry {
                name: "dev".into(),
                is_dir: true,
                is_repo: false,
            },
            FolderEntry {
                name: "jolt".into(),
                is_dir: true,
                is_repo: true,
            },
        ],
        truncated: false,
    };
    // Files never show as rows.
    assert_eq!(browser_rows(&listing).len(), 2);
    assert_eq!(browser_rows(&listing)[1].name, "jolt");
}

#[test]
fn resolved_chat_config_requires_harness() {
    let mut resolved = ResolvedRunConfig::default();
    assert!(resolved.chat_config().is_none());
    resolved.harness = Some(HarnessId::ClaudeCode);
    resolved.model = Some("opus".into());
    resolved.reasoning = Some(ReasoningLevel::High);
    let config = resolved.chat_config().expect("harness set");
    assert_eq!(config.harness, HarnessId::ClaudeCode);
    assert_eq!(config.model.as_deref(), Some("opus"));
    assert_eq!(config.sandbox, SandboxLevel::WorkspaceWrite);
}

#[test]
fn default_model_is_first_catalog_row() {
    let models = vec![
        Model {
            id: "flagship".into(),
            label: "Flagship".into(),
            description: None,
            reasoning_levels: vec![],
            options: vec![],
        },
        Model {
            id: "fast".into(),
            label: "Fast".into(),
            description: None,
            reasoning_levels: vec![],
            options: vec![],
        },
    ];
    assert_eq!(default_model(&models).map(|m| &*m.id), Some("flagship"));
    assert!(default_model(&[]).is_none());
}

#[test]
fn default_reasoning_prefers_high_then_medium() {
    use ReasoningLevel::*;
    // Recommended default is High (user-corrected), even on full ladders.
    assert_eq!(
        default_reasoning(&[Low, Medium, High, XHigh, Max, Ultracode, Ultrathink]),
        Some(High)
    );
    assert_eq!(default_reasoning(&[Low, Medium, High, Max]), Some(High));
    // No High: Medium.
    assert_eq!(default_reasoning(&[Minimal, Low, Medium]), Some(Medium));
    // Neither offered: first entry.
    assert_eq!(default_reasoning(&[Minimal, Low]), Some(Minimal));
    // Ladder-less model (Haiku): no reasoning at all.
    assert_eq!(default_reasoning(&[]), None);
}

#[test]
fn clamp_reasoning_keeps_offered_levels_and_heals_foreign_ones() {
    use ReasoningLevel::*;
    let ladder = [Low, Medium, High, Max];
    // A pick the ladder offers survives.
    assert_eq!(clamp_reasoning(Some(Max), &ladder), Some(Max));
    // A remembered level the new model doesn't offer heals to its default.
    assert_eq!(clamp_reasoning(Some(XHigh), &ladder), Some(High));
    // No pick at all resolves to the concrete default too.
    assert_eq!(clamp_reasoning(None, &ladder), Some(High));
    assert_eq!(clamp_reasoning(Some(High), &[]), None);
}

#[test]
fn mock_harness_hidden_unless_alone() {
    let descriptor = |id: HarnessId, name: &str| HarnessDescriptor {
        id,
        name: name.into(),
        supports_steering: true,
        steering_mode: jolt_proto::SteeringMode::StepBoundary,
        reasoning_levels: vec![],
    };
    let mixed = vec![
        descriptor(HarnessId::Mock, "Mock"),
        descriptor(HarnessId::ClaudeCode, "Claude Code"),
    ];
    // Env-independent core: mock hidden in production…
    let visible = visible_harnesses_impl(&mixed, false);
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].id, HarnessId::ClaudeCode);
    let only_mock = vec![descriptor(HarnessId::Mock, "Mock")];
    assert_eq!(visible_harnesses_impl(&only_mock, false).len(), 1);
    // …and opted back in by JOLT_HARNESS=mock (the e2e rig).
    assert_eq!(visible_harnesses_impl(&mixed, true).len(), 2);
    assert_eq!(visible_harnesses_impl(&mixed, true)[0].id, HarnessId::Mock);
}
