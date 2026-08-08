//! Module behavior tests.
use super::*;

#[test]
fn display_columns_expands_tabs_to_four_column_stops() {
    assert_eq!(display_columns("a\tb"), 5);
    assert_eq!(display_columns("abcd\te"), 9);
}

#[test]
fn parses_basic_patch() {
    let files =
        parse_patch("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].additions, 1);
    assert_eq!(files[0].deletions, 1);
}

#[test]
fn split_rows_pair_change_blocks_and_duplicate_context() {
    let line = |kind| DiffLine {
        kind,
        old_no: None,
        new_no: None,
        text: String::new(),
    };
    let lines = [
        line(LineKind::Del),
        line(LineKind::Del),
        line(LineKind::Add),
        line(LineKind::Context),
        line(LineKind::Meta),
    ];
    assert_eq!(
        split_line_slots(&lines),
        vec![
            SplitLineSlot::Pair {
                old: Some(0),
                new: Some(2),
            },
            SplitLineSlot::Pair {
                old: Some(1),
                new: None,
            },
            SplitLineSlot::Pair {
                old: Some(3),
                new: Some(3),
            },
            SplitLineSlot::Full(4),
        ]
    );
}

#[test]
fn review_side_uses_the_selected_coordinate_set() {
    let anchor = |old_lines, new_lines| DiffReviewAnchor {
        file_id: "file".into(),
        path: "a.rs".into(),
        old_path: None,
        old_lines,
        new_lines,
        excerpt: Vec::new(),
    };
    let line = Some(InclusiveLineRange { start: 4, end: 4 });
    assert_eq!(review_side(&anchor(line, None)), ReviewSide::Old);
    assert_eq!(review_side(&anchor(None, line)), ReviewSide::New);
    assert_eq!(review_side(&anchor(line, line)), ReviewSide::Both);
}

#[test]
fn row_splice_preserves_unchanged_prefix_and_suffix() {
    let row = |id: &str| ChangeRow {
        id: id.into(),
        version: 1,
        kind: ChangeRowKind::FileHeader { file: 0 },
    };
    let old = [row("a"), row("b"), row("c")];
    let new = [row("a"), row("x"), row("c")];
    assert_eq!(row_splice(&old, &new), Some((1..2, 1)));
}

#[test]
fn sticky_header_tracks_the_file_above_the_viewport() {
    let headers = [0, 5, 9];
    assert_eq!(sticky_header_row(&headers, 0, false), None);
    assert_eq!(sticky_header_row(&headers, 0, true), Some(0));
    assert_eq!(sticky_header_row(&headers, 4, false), Some(0));
    assert_eq!(sticky_header_row(&headers, 5, false), None);
    assert_eq!(sticky_header_row(&headers, 8, false), Some(5));
}

#[test]
fn next_file_pushes_the_sticky_header_away() {
    assert_eq!(sticky_header_push_offset(None), 0.0);
    assert_eq!(sticky_header_push_offset(Some(50.0)), 0.0);
    assert_eq!(sticky_header_push_offset(Some(FILE_HEADER_HEIGHT)), 0.0);
    assert_eq!(sticky_header_push_offset(Some(20.0)), -16.0);
    assert_eq!(sticky_header_push_offset(Some(0.0)), -FILE_HEADER_HEIGHT);
}
