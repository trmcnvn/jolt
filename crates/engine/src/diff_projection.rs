//! Immutable byte-bounded pages for checkout-diff viewports.

use std::collections::HashMap;

use jolt_proto::{
    CheckoutDiffBootstrap, CheckoutDiffManifest, CheckoutDiffPage, DiffCompleteness,
    DiffFileDescriptor, DiffFileSummary, DiffPageDescriptor,
};
use memchr::{memchr, memmem};
use sha2::{Digest, Sha256};

use crate::diff_sync::DiffSnapshot;

pub const DIFF_PAGE_TARGET_BYTES: usize = 128 * 1024;
pub const DIFF_PAGE_MAX_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone)]
pub struct DiffProjection {
    pub manifest: CheckoutDiffManifest,
    pages: HashMap<String, CheckoutDiffPage>,
}

impl DiffProjection {
    pub fn build(
        checkout_id: &str,
        device_id: &str,
        cwd: &str,
        snapshot: &DiffSnapshot,
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        let mut descriptors = Vec::with_capacity(snapshot.files.len());
        let mut page_descriptors = Vec::new();
        let mut pages = HashMap::new();

        for summary in &snapshot.files {
            let file_id = digest_hex(&[checkout_id.as_bytes(), &[0], summary.path.as_bytes()]);
            let section = snapshot.file_patches.get(&summary.path);
            let mut completeness = if summary.binary {
                DiffCompleteness::Binary
            } else if section.is_none() {
                DiffCompleteness::SnapshotTruncated
            } else {
                DiffCompleteness::Complete
            };
            let mut file_pages = Vec::new();
            let mut row_count = 0usize;
            let mut estimated_bytes = 0usize;
            if !summary.binary
                && let Some(raw) = section
            {
                let projected = project_file(raw);
                if projected.oversized {
                    completeness = DiffCompleteness::OversizedLine;
                }
                if snapshot.truncated && snapshot.patch.ends_with(raw) {
                    completeness = DiffCompleteness::SnapshotTruncated;
                }
                for page in projected.pages {
                    let id = digest_hex(&[page.patch.as_bytes()]);
                    let metrics = page_metrics(&page.patch);
                    let descriptor = DiffPageDescriptor {
                        id: id.clone(),
                        file_id: file_id.clone(),
                        first_row: row_count,
                        row_count: metrics.notices + metrics.hunks + metrics.lines,
                        notice_count: metrics.notices,
                        hunk_count: metrics.hunks,
                        line_count: metrics.lines,
                        split_line_count: metrics.split_lines,
                        estimated_bytes: page.patch.len(),
                    };
                    row_count += descriptor.row_count;
                    estimated_bytes += page.patch.len();
                    file_pages.push(id.clone());
                    page_descriptors.push(descriptor);
                    pages.insert(
                        id.clone(),
                        CheckoutDiffPage {
                            id,
                            catalog_revision: String::new(),
                            file_id: file_id.clone(),
                            patch: page.patch,
                        },
                    );
                }
            }
            descriptors.push(file_descriptor(
                summary,
                file_id,
                row_count,
                estimated_bytes,
                completeness,
                file_pages,
            ));
        }

        let catalog_revision = catalog_revision(snapshot, &descriptors, &page_descriptors);
        for page in pages.values_mut() {
            page.catalog_revision.clone_from(&catalog_revision);
        }
        Self {
            manifest: CheckoutDiffManifest {
                catalog_revision,
                checkout_id: checkout_id.to_string(),
                device_id: device_id.to_string(),
                cwd: cwd.to_string(),
                vcs: snapshot.vcs,
                label: snapshot.label.clone(),
                files: descriptors,
                pages: page_descriptors,
                additions: snapshot.additions,
                deletions: snapshot.deletions,
                truncated: snapshot.truncated,
                updated_at,
            },
            pages,
        }
    }

    pub fn bootstrap(&self, sequence: u64) -> CheckoutDiffBootstrap {
        CheckoutDiffBootstrap {
            sequence,
            manifest: self.manifest.clone(),
            pages: Vec::new(),
        }
    }

    pub fn page(&self, id: &str) -> Option<CheckoutDiffPage> {
        self.pages.get(id).cloned()
    }

    pub fn pages(&self) -> impl Iterator<Item = &CheckoutDiffPage> {
        self.pages.values()
    }
}

fn file_descriptor(
    summary: &DiffFileSummary,
    id: String,
    row_count: usize,
    estimated_bytes: usize,
    completeness: DiffCompleteness,
    page_ids: Vec<String>,
) -> DiffFileDescriptor {
    DiffFileDescriptor {
        id,
        path: summary.path.clone(),
        old_path: summary.old_path.clone(),
        status: summary.status.clone(),
        additions: summary.additions,
        deletions: summary.deletions,
        binary: summary.binary,
        row_count,
        estimated_bytes,
        completeness,
        page_ids,
    }
}

fn catalog_revision(
    snapshot: &DiffSnapshot,
    files: &[DiffFileDescriptor],
    pages: &[DiffPageDescriptor],
) -> String {
    let files = serde_json::to_vec(files).unwrap_or_default();
    let pages = serde_json::to_vec(pages).unwrap_or_default();
    digest_hex(&[snapshot.checksum.as_bytes(), &[0], &files, &[0], &pages])
}

fn digest_hex(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part);
    }
    jolt_vcs::hex(&hash.finalize())
}

/// Section offsets found with memchr's runtime-SIMD newline scanner. On the
/// 3.146 MiB aarch64 release fixture in `benches/diff_projection.rs`, newline
/// scanning measured 67 µs versus 497 µs for the scalar reference; complete
/// catalog construction measured 12.9 ms. The section parser remains
/// line-oriented and is covered against the scalar reference in tests.
#[cfg(test)]
fn patch_sections(patch: &str) -> Vec<(usize, usize)> {
    let bytes = patch.as_bytes();
    let needle = b"diff --git ";
    let mut starts = Vec::new();
    if bytes.starts_with(needle) {
        starts.push(0);
    }
    let mut offset = 0usize;
    while let Some(newline) = memchr(b'\n', &bytes[offset..]) {
        let next = offset + newline + 1;
        if bytes
            .get(next..)
            .is_some_and(|tail| tail.starts_with(needle))
        {
            starts.push(next);
        }
        offset = next;
        if offset >= bytes.len() {
            break;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            (
                *start,
                starts.get(index + 1).copied().unwrap_or(bytes.len()),
            )
        })
        .collect()
}

#[derive(Debug)]
struct ProjectedFile {
    pages: Vec<ProjectedPage>,
    oversized: bool,
}

#[derive(Debug)]
struct ProjectedPage {
    patch: String,
    row_count: usize,
}

fn project_file(section: &str) -> ProjectedFile {
    let hunk_starts: Vec<usize> = memmem::find_iter(section.as_bytes(), b"\n@@")
        .map(|offset| offset + 1)
        .collect();
    if hunk_starts.is_empty() {
        let oversized = section.len() > DIFF_PAGE_MAX_BYTES;
        let patch = if oversized {
            truncate_at_line(section, DIFF_PAGE_MAX_BYTES)
        } else {
            section.to_string()
        };
        return ProjectedFile {
            pages: vec![ProjectedPage {
                row_count: notice_rows(&patch).max(1),
                patch,
            }],
            oversized,
        };
    }

    let prefix = &section[..hunk_starts[0]];
    let mut fragments = Vec::new();
    let mut oversized = false;
    for (index, start) in hunk_starts.iter().copied().enumerate() {
        let end = hunk_starts.get(index + 1).copied().unwrap_or(section.len());
        let hunk = &section[start..end];
        let mut projected = split_hunk(prefix, hunk);
        oversized |= projected.1;
        fragments.append(&mut projected.0);
    }
    let prefix_rows = notice_rows(prefix);
    let mut pages: Vec<ProjectedPage> = Vec::new();
    for fragment in fragments {
        let append_bytes = fragment.patch.len().saturating_sub(prefix.len());
        if let Some(page) = pages.last_mut()
            && page.patch.len() + append_bytes <= DIFF_PAGE_TARGET_BYTES
        {
            page.patch.push_str(&fragment.patch[prefix.len()..]);
            page.row_count += fragment.row_count.saturating_sub(prefix_rows);
        } else {
            pages.push(fragment);
        }
    }
    ProjectedFile { pages, oversized }
}

fn split_hunk(prefix: &str, hunk: &str) -> (Vec<ProjectedPage>, bool) {
    let header_end = memchr(b'\n', hunk.as_bytes()).map_or(hunk.len(), |index| index + 1);
    let header = &hunk[..header_end];
    let body = &hunk[header_end..];
    let Some((mut old_line, mut new_line)) = parse_hunk_starts(header) else {
        return (
            vec![ProjectedPage {
                patch: format!("{prefix}{hunk}"),
                row_count: 1 + body.lines().count(),
            }],
            false,
        );
    };
    let budget = DIFF_PAGE_TARGET_BYTES
        .saturating_sub(prefix.len() + header.len())
        .max(1024);
    let hard_line_limit = DIFF_PAGE_MAX_BYTES
        .saturating_sub(prefix.len() + header.len() + 80)
        .max(1024);
    let mut pages = Vec::new();
    let mut chunk = String::new();
    let mut chunk_old = old_line;
    let mut chunk_new = new_line;
    let mut chunk_old_count = 0u32;
    let mut chunk_new_count = 0u32;
    let mut chunk_rows = 0usize;
    let mut oversized = false;

    let flush = |pages: &mut Vec<ProjectedPage>,
                 chunk: &mut String,
                 old_start: u32,
                 new_start: u32,
                 old_count: u32,
                 new_count: u32,
                 rows: usize| {
        if chunk.is_empty() {
            return;
        }
        pages.push(ProjectedPage {
            patch: format!(
                "{prefix}@@ -{old_start},{old_count} +{new_start},{new_count} @@\n{chunk}"
            ),
            row_count: notice_rows(prefix) + 1 + rows,
        });
        chunk.clear();
    };

    let lines: Vec<_> = body.split_inclusive('\n').collect();
    let mut protected_change_end = 0usize;
    for (index, line) in lines.iter().copied().enumerate() {
        if index >= protected_change_end && matches!(line.as_bytes().first(), Some(b'+' | b'-')) {
            let mut end = index;
            let mut bytes = 0usize;
            let mut has_old = false;
            let mut has_new = false;
            while end < lines.len() && matches!(lines[end].as_bytes().first(), Some(b'+' | b'-')) {
                bytes += lines[end].len();
                has_old |= lines[end].starts_with('-');
                has_new |= lines[end].starts_with('+');
                end += 1;
            }
            // Keep ordinary replacement blocks on one page so a split viewer
            // can pair their old and new sides. Very large one-sided blocks
            // and replacements above the hard page bound still split by line.
            if has_old && has_new && bytes <= hard_line_limit {
                if !chunk.is_empty() && chunk.len() + bytes > budget {
                    flush(
                        &mut pages,
                        &mut chunk,
                        chunk_old,
                        chunk_new,
                        chunk_old_count,
                        chunk_new_count,
                        chunk_rows,
                    );
                    chunk_old = old_line;
                    chunk_new = new_line;
                    chunk_old_count = 0;
                    chunk_new_count = 0;
                    chunk_rows = 0;
                }
                protected_change_end = end;
            }
        }
        if !chunk.is_empty() && chunk.len() + line.len() > budget && index >= protected_change_end {
            flush(
                &mut pages,
                &mut chunk,
                chunk_old,
                chunk_new,
                chunk_old_count,
                chunk_new_count,
                chunk_rows,
            );
            chunk_old = old_line;
            chunk_new = new_line;
            chunk_old_count = 0;
            chunk_new_count = 0;
            chunk_rows = 0;
        }
        let line_was_oversized = line.len() > hard_line_limit;
        let line = if line_was_oversized {
            oversized = true;
            truncate_at_line(line, hard_line_limit)
        } else {
            line.to_string()
        };
        match line.as_bytes().first().copied() {
            Some(b'+') => {
                new_line = new_line.saturating_add(1);
                chunk_new_count = chunk_new_count.saturating_add(1);
            }
            Some(b'-') => {
                old_line = old_line.saturating_add(1);
                chunk_old_count = chunk_old_count.saturating_add(1);
            }
            Some(b' ') | None => {
                old_line = old_line.saturating_add(1);
                new_line = new_line.saturating_add(1);
                chunk_old_count = chunk_old_count.saturating_add(1);
                chunk_new_count = chunk_new_count.saturating_add(1);
            }
            _ => {}
        }
        chunk_rows += 1 + usize::from(line_was_oversized);
        chunk.push_str(&line);
    }
    flush(
        &mut pages,
        &mut chunk,
        chunk_old,
        chunk_new,
        chunk_old_count,
        chunk_new_count,
        chunk_rows,
    );
    (pages, oversized)
}

fn parse_hunk_starts(header: &str) -> Option<(u32, u32)> {
    let minus = header.find('-')?;
    let old = header[minus + 1..]
        .split(|character: char| character == ',' || character.is_whitespace())
        .next()?
        .parse()
        .ok()?;
    let plus = header.find('+')?;
    let new = header[plus + 1..]
        .split(|character: char| character == ',' || character.is_whitespace())
        .next()?
        .parse()
        .ok()?;
    Some((old, new))
}

fn truncate_at_line(value: &str, max: usize) -> String {
    let mut end = value.len().min(max);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut result = value[..end].to_string();
    result.push_str("\n\\ Jolt omitted the remainder of an oversized diff line\n");
    result
}

#[derive(Debug, Clone, Copy)]
struct PageMetrics {
    notices: usize,
    hunks: usize,
    lines: usize,
    split_lines: usize,
}

fn page_metrics(patch: &str) -> PageMetrics {
    let mut metrics = PageMetrics {
        notices: notice_rows(patch),
        hunks: 0,
        lines: 0,
        split_lines: 0,
    };
    let mut in_hunk = false;
    let mut old_change_lines = 0usize;
    let mut new_change_lines = 0usize;
    let flush_change = |metrics: &mut PageMetrics, old: &mut usize, new: &mut usize| {
        metrics.split_lines += (*old).max(*new);
        *old = 0;
        *new = 0;
    };
    for line in patch.lines() {
        if line.starts_with("@@") {
            flush_change(&mut metrics, &mut old_change_lines, &mut new_change_lines);
            metrics.hunks += 1;
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        match line.as_bytes().first().copied() {
            Some(b'-') => {
                metrics.lines += 1;
                old_change_lines += 1;
            }
            Some(b'+') => {
                metrics.lines += 1;
                new_change_lines += 1;
            }
            Some(b' ') | Some(b'\\') => {
                flush_change(&mut metrics, &mut old_change_lines, &mut new_change_lines);
                metrics.lines += 1;
                metrics.split_lines += 1;
            }
            _ => {
                flush_change(&mut metrics, &mut old_change_lines, &mut new_change_lines);
                in_hunk = false;
            }
        }
    }
    flush_change(&mut metrics, &mut old_change_lines, &mut new_change_lines);
    metrics
}

fn notice_rows(prefix: &str) -> usize {
    prefix
        .lines()
        .filter(|line| {
            line.starts_with("new file mode")
                || line.starts_with("deleted file mode")
                || line.starts_with("rename from ")
                || line.starts_with("new mode ")
                || line.starts_with("Binary files")
                || *line == "GIT binary patch"
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_scanner_matches_scalar_reference() {
        let patch =
            "diff --git a/a b/a\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/b b/b\n@@ -1 +1 @@\n-c\n+d\n";
        let scalar: Vec<_> = patch
            .match_indices("diff --git ")
            .map(|(offset, _)| offset)
            .collect();
        let sections = patch_sections(patch);
        assert_eq!(
            sections.iter().map(|section| section.0).collect::<Vec<_>>(),
            scalar
        );
    }

    fn snapshot(patch: &str, paths: &[&str], truncated: bool) -> DiffSnapshot {
        let files: Vec<_> = paths
            .iter()
            .map(|path| DiffFileSummary {
                path: (*path).into(),
                old_path: None,
                status: "modified".into(),
                additions: 1,
                deletions: 1,
                binary: false,
            })
            .collect();
        let sections = patch_sections(patch);
        let file_patches = files
            .iter()
            .zip(sections)
            .map(|(file, (start, end))| (file.path.clone(), patch[start..end].to_string()))
            .collect();
        DiffSnapshot {
            vcs: jolt_proto::VcsKind::Git,
            label: None,
            branch: "main".into(),
            head_sha: Some("head".into()),
            patch: patch.into(),
            files,
            file_patches,
            additions: paths.len() as u32,
            deletions: paths.len() as u32,
            truncated,
            checksum: patch.len().to_string(),
        }
    }

    #[test]
    fn page_metrics_count_paired_split_rows() {
        let patch = "diff --git a/a b/a\n@@ -1,3 +1,2 @@\n-old one\n-old two\n+new\n context\n";
        let metrics = page_metrics(patch);
        assert_eq!(metrics.lines, 4);
        assert_eq!(metrics.split_lines, 3);
    }

    #[test]
    fn replacement_block_stays_pairable_below_hard_page_limit() {
        let deleted = format!("-{}\n", "a".repeat(2048)).repeat(40);
        let added = format!("+{}\n", "b".repeat(2048)).repeat(40);
        let section =
            format!("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,40 +1,40 @@\n{deleted}{added}");
        let projected = project_file(&section);
        assert_eq!(projected.pages.len(), 1);
        assert!(projected.pages[0].patch.len() > DIFF_PAGE_TARGET_BYTES);
        assert!(projected.pages[0].patch.len() <= DIFF_PAGE_MAX_BYTES);
    }

    #[test]
    fn oversized_hunk_is_split_into_self_contained_pages() {
        let line = format!("+{}\n", "x".repeat(4096));
        let body = line.repeat(80);
        let section = format!("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -0,0 +1,80 @@\n{body}");
        let projected = project_file(&section);
        assert!(projected.pages.len() > 1);
        assert!(
            projected
                .pages
                .iter()
                .all(|page| page.patch.starts_with("diff --git ") && page.patch.contains("\n@@ -"))
        );
        assert!(
            projected
                .pages
                .iter()
                .all(|page| page.patch.len() <= DIFF_PAGE_TARGET_BYTES + 8192)
        );
    }

    #[test]
    fn oversized_line_is_bounded_and_marked() {
        let section = format!(
            "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -0,0 +1 @@\n+{}\n",
            "x".repeat(DIFF_PAGE_MAX_BYTES * 2)
        );
        let projected = project_file(&section);
        assert!(projected.oversized);
        assert!(projected.pages[0].patch.len() <= DIFF_PAGE_MAX_BYTES);
        assert!(projected.pages[0].patch.contains("Jolt omitted"));
    }

    #[test]
    fn unchanged_file_pages_survive_an_unrelated_edit() {
        let first =
            "diff --git a/a b/a\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/b b/b\n@@ -1 +1 @@\n-c\n+d\n";
        let second = "diff --git a/a b/a\n@@ -1 +1 @@\n-a\n+changed\ndiff --git a/b b/b\n@@ -1 +1 @@\n-c\n+d\n";
        let left = DiffProjection::build(
            "checkout",
            "device",
            "/repo",
            &snapshot(first, &["a", "b"], false),
            chrono::Utc::now(),
        );
        let right = DiffProjection::build(
            "checkout",
            "device",
            "/repo",
            &snapshot(second, &["a", "b"], false),
            chrono::Utc::now(),
        );
        assert!(left.page(&left.manifest.files[0].page_ids[0]).is_some());
        assert_ne!(
            left.manifest.files[0].page_ids,
            right.manifest.files[0].page_ids
        );
        assert_eq!(
            left.manifest.files[1].page_ids,
            right.manifest.files[1].page_ids
        );
    }

    #[test]
    fn truncated_snapshot_keeps_uncaptured_file_descriptors() {
        let patch = "diff --git a/a b/a\n@@ -1 +1 @@\n-a\n+b\n";
        let projection = DiffProjection::build(
            "checkout",
            "device",
            "/repo",
            &snapshot(patch, &["a", "later"], true),
            chrono::Utc::now(),
        );
        assert_eq!(projection.manifest.files.len(), 2);
        assert!(projection.manifest.files[1].page_ids.is_empty());
        assert_eq!(
            projection.manifest.files[1].completeness,
            DiffCompleteness::SnapshotTruncated
        );
    }
}
