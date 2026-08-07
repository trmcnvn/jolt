//! Target-neutral review feedback export.

use std::fmt::Write as _;

use jolt_proto::{
    DiffReviewAnchor, DiffReviewComment, DiffReviewLineKind, InclusiveLineRange, ReviewDraft,
    ReviewDraftTarget,
};

pub const REVIEW_FEEDBACK_PREFIX: &str = "Please address the following review feedback.";

pub fn feedback_message(draft: &ReviewDraft) -> Option<String> {
    let ReviewDraftTarget::Diff {
        snapshot, comments, ..
    } = &draft.target
    else {
        return message_feedback(draft);
    };
    let comments: Vec<_> = comments
        .iter()
        .filter(|comment| !comment.body.trim().is_empty())
        .collect();
    if comments.is_empty() {
        return None;
    }

    let mut output = String::from(REVIEW_FEEDBACK_PREFIX);
    let mut files: Vec<&str> = snapshot
        .manifest
        .files
        .iter()
        .filter_map(|file| {
            comments
                .iter()
                .any(|comment| comment.anchor.file_id == file.id)
                .then_some(file.id.as_str())
        })
        .collect();
    for comment in &comments {
        if !files.contains(&comment.anchor.file_id.as_str()) {
            files.push(&comment.anchor.file_id);
        }
    }
    for file_id in files {
        let file_comments: Vec<_> = comments
            .iter()
            .filter(|comment| comment.anchor.file_id == file_id)
            .collect();
        let Some(first) = file_comments.first() else {
            continue;
        };
        let _ = write!(output, "\n\n### {}", inline_code(&first.anchor.path));
        for comment in file_comments {
            write_diff_comment(&mut output, comment);
        }
    }
    Some(output)
}

fn message_feedback(draft: &ReviewDraft) -> Option<String> {
    let ReviewDraftTarget::AssistantMessage { comments, .. } = &draft.target else {
        return None;
    };
    let comments: Vec<_> = comments
        .iter()
        .filter(|comment| !comment.body.trim().is_empty())
        .collect();
    if comments.is_empty() {
        return None;
    }
    let mut output = format!("{REVIEW_FEEDBACK_PREFIX}\n\n### Previous response");
    for comment in comments {
        let _ = write!(
            output,
            "\n\n> {}\n\n{}",
            comment.anchor.exact.replace('\n', "\n> "),
            comment.body.trim()
        );
    }
    Some(output)
}

fn write_diff_comment(output: &mut String, comment: &DiffReviewComment) {
    let _ = write!(
        output,
        "\n\n#### {}\n\n{}",
        anchor_label(&comment.anchor),
        comment.body.trim()
    );
    if comment.anchor.excerpt.is_empty() {
        return;
    }
    let fence_len = comment
        .anchor
        .excerpt
        .iter()
        .map(|line| longest_backtick_run(&line.text))
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(3);
    let fence = "`".repeat(fence_len);
    let _ = write!(output, "\n\n{fence}diff\n");
    const EXCERPT_LIMIT: usize = 40;
    for line in comment.anchor.excerpt.iter().take(EXCERPT_LIMIT) {
        let marker = match line.kind {
            DiffReviewLineKind::Addition => '+',
            DiffReviewLineKind::Deletion => '-',
            DiffReviewLineKind::Context => ' ',
        };
        let _ = writeln!(output, "{marker}{}", line.text);
    }
    if comment.anchor.excerpt.len() > EXCERPT_LIMIT {
        let omitted = comment.anchor.excerpt.len() - EXCERPT_LIMIT;
        let _ = writeln!(output, " … {omitted} selected lines omitted …");
    }
    output.push_str(&fence);
}

fn anchor_label(anchor: &DiffReviewAnchor) -> String {
    match (anchor.old_lines, anchor.new_lines) {
        (None, Some(lines)) => format!("New {}", range_label(lines)),
        (Some(lines), None) => format!("Old {}", range_label(lines)),
        (Some(old), Some(new)) if old == new => range_label(new),
        (Some(old), Some(new)) => {
            format!("Old {} · New {}", range_label(old), range_label(new))
        }
        (None, None) => "Selected lines".into(),
    }
}

fn range_label(range: InclusiveLineRange) -> String {
    if range.start == range.end {
        format!("line {}", range.start)
    } else {
        format!("lines {}–{}", range.start, range.end)
    }
}

fn inline_code(value: &str) -> String {
    let fence = "`".repeat(longest_backtick_run(value).saturating_add(1).max(1));
    if value.starts_with('`') || value.ends_with('`') {
        format!("{fence} {value} {fence}")
    } else {
        format!("{fence}{value}{fence}")
    }
}

fn longest_backtick_run(value: &str) -> usize {
    value
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use jolt_proto::{
        AssistantMessageReviewSnapshot, AssistantMessageReviewSubject, CheckoutDiffManifest,
        DiffReviewExcerptLine, DiffReviewSnapshot, DiffReviewSubject, MessageReviewAnchor,
        MessageReviewComment, REVIEW_DRAFT_SCHEMA_VERSION, ReviewDraftTarget,
    };

    use super::*;

    #[test]
    fn diff_feedback_is_grouped_and_uses_semantic_coordinates() {
        let now = Utc::now();
        let draft = ReviewDraft {
            schema_version: REVIEW_DRAFT_SCHEMA_VERSION,
            review_id: "review".into(),
            review_key: "key".into(),
            destination_chat_id: "chat".into(),
            snapshot_id: "revision".into(),
            target: ReviewDraftTarget::Diff {
                subject: DiffReviewSubject::WorkingCopy {
                    chat_id: "chat".into(),
                },
                snapshot: DiffReviewSnapshot {
                    manifest: CheckoutDiffManifest {
                        catalog_revision: "revision".into(),
                        checkout_id: "checkout".into(),
                        device_id: "device".into(),
                        cwd: "/repo".into(),
                        vcs: jolt_proto::VcsKind::Git,
                        label: None,
                        files: Vec::new(),
                        pages: Vec::new(),
                        additions: 1,
                        deletions: 0,
                        truncated: false,
                        updated_at: now,
                    },
                    target_device_id: None,
                },
                comments: vec![DiffReviewComment {
                    id: "comment".into(),
                    anchor: DiffReviewAnchor {
                        file_id: "file".into(),
                        path: "src/a.rs".into(),
                        old_path: None,
                        old_lines: None,
                        new_lines: Some(InclusiveLineRange { start: 4, end: 5 }),
                        excerpt: vec![DiffReviewExcerptLine {
                            kind: DiffReviewLineKind::Addition,
                            old_number: None,
                            new_number: Some(4),
                            text: "let value = 1;".into(),
                        }],
                    },
                    body: "Explain this value.".into(),
                    created_at: now,
                    updated_at: now,
                }],
            },
            created_at: now,
            updated_at: now,
        };
        let message = feedback_message(&draft).unwrap();
        assert!(message.contains("### `src/a.rs`"));
        assert!(message.contains("#### New lines 4–5"));
        assert!(message.contains("+let value = 1;"));
        assert!(message.contains("Explain this value."));
    }

    #[test]
    fn assistant_message_feedback_uses_the_same_review_envelope() {
        let now = Utc::now();
        let draft = ReviewDraft {
            schema_version: REVIEW_DRAFT_SCHEMA_VERSION,
            review_id: "review".into(),
            review_key: "key".into(),
            destination_chat_id: "chat".into(),
            snapshot_id: "revision".into(),
            target: ReviewDraftTarget::AssistantMessage {
                subject: AssistantMessageReviewSubject {
                    chat_id: "chat".into(),
                    root_message_id: "message".into(),
                },
                snapshot: AssistantMessageReviewSnapshot {
                    revision: "revision".into(),
                    text_parts: Vec::new(),
                },
                comments: vec![MessageReviewComment {
                    id: "comment".into(),
                    anchor: MessageReviewAnchor {
                        part_id: "t0".into(),
                        start_byte: 0,
                        end_byte: 18,
                        exact: "The cache is safe.".into(),
                        prefix: String::new(),
                        suffix: String::new(),
                    },
                    body: "Account for cancellation.".into(),
                    created_at: now,
                    updated_at: now,
                }],
            },
            created_at: now,
            updated_at: now,
        };
        let message = feedback_message(&draft).unwrap();
        assert!(message.starts_with(REVIEW_FEEDBACK_PREFIX));
        assert!(message.contains("> The cache is safe."));
        assert!(message.contains("Account for cancellation."));
    }
}
