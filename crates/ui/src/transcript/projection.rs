//! Transcript row projection and pure presentation derivations.

use super::*;

// ---------------------------------------------------------------------------
// Row model (pure)
// ---------------------------------------------------------------------------

/// One tool invocation inside a group row.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolItem {
    pub call: ToolCall,
    pub is_error: bool,
    pub resolved: bool,
}

#[derive(Clone)]
pub enum RowKind {
    User {
        /// Parsed prompt Markdown (attachment-ref trailer already stripped).
        /// User messages stay one virtualized bubble row even when the tree
        /// contains multiple blocks.
        tree: Arc<BlockTree>,
        /// Image refs parsed out of the message text; thumbnails load from the
        /// owning device via ReadAttachmentChunk.
        attachments: Arc<Vec<crate::attachments::UserImageAttachment>>,
        /// Optimistic echo not yet confirmed by a doc frame.
        pending: bool,
    },
    /// One top-level markdown block of a completed message.
    Markdown {
        tree: Arc<BlockTree>,
        block_ix: usize,
    },
    /// One top-level block of a STREAMING message. Split per block like
    /// completed rows (only the tail blocks' versions change per commit, so
    /// the settled prefix is never respliced or re-rendered); rendered with
    /// the fade veil.
    LiveMarkdown {
        tree: Arc<BlockTree>,
        block_ix: usize,
    },
    ToolGroup {
        tools: Arc<Vec<ToolItem>>,
        /// This is the trailing tool group of a streaming reply. Collapsed
        /// active groups preview their latest tool instead of opening fully.
        active: bool,
    },
    /// Compact, immutable filesystem delta for the owning assistant entry.
    Changes {
        diff: Arc<TurnDiffManifest>,
    },
    InputChip {
        /// First question's header. The resolved chip shows it; unresolved
        /// shows "Awaiting your answer…", which
        /// stays TRUE even across a run death: the composer keeps the panel
        /// up until the user answers, and the engine delivers a dead run's
        /// answer as a resumed turn).
        header: SharedString,
        resolved: bool,
    },
    ErrorChip {
        message: SharedString,
    },
    /// Full-width transcript boundary between native harness conversations.
    HarnessSwitch {
        from: jolt_proto::HarnessId,
        to: jolt_proto::HarnessId,
    },
    /// Estimated-height stand-in for a cold transcript page. Rendering it
    /// starts the fetch; retaining its height makes the scrollbar represent
    /// the entire conversation before message bodies are decoded.
    HistoryPlaceholder {
        page_id: SharedString,
        estimated_height: f32,
        loading: bool,
        failed: bool,
    },
}

/// A transcript row: stable id + content version (diff key) + block payload.
#[derive(Clone)]
pub struct Row {
    pub id: SharedString,
    pub version: u64,
    /// First row of its message entry (gets the turn gap).
    pub turn_start: bool,
    pub kind: RowKind,
    /// The owning message entry; hovering any row reveals its timestamp strip.
    pub entry_id: SharedString,
    /// Epoch-ms for the 16px hover-timestamp strip UNDER this row: set on the
    /// Last row of a completed entry: user rows always, assistant rows only
    /// once streaming ends.
    pub timestamp: Option<i64>,
    /// Message text copied by the hover action beside the timestamp. Present
    /// only on the same settled last row as [`Self::timestamp`].
    pub copy_text: Option<SharedString>,
}

/// Absolute hover-timestamp label, e.g. "Jul 1, 3:45 PM": short month,
/// numeric day, hour, two-digit minutes, and no leading zero. Pure over an explicit
/// timezone so tests don't depend on the host's local time.
pub fn format_timestamp<Tz: chrono::TimeZone>(ms: i64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    match chrono::DateTime::from_timestamp_millis(ms) {
        Some(utc) => utc
            .with_timezone(tz)
            .format("%b %-d, %-I:%M %p")
            .to_string(),
        None => String::new(),
    }
}

/// Markdown source behind the visible message, excluding internal attachment
/// references. Non-text transcript parts keep their purpose-built controls.
pub(super) fn message_copy_text(entry: &SessionMessageEntry) -> Option<SharedString> {
    let text = entry
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let text = if entry.role == MessageRole::User {
        crate::attachments::parse_user_message_images(&text).text
    } else {
        text
    };
    (!text.trim().is_empty()).then(|| SharedString::from(text))
}

pub(super) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x1_0000_01b3);
    }
    hash
}

pub(super) fn is_file_mutation(call: &ToolCall) -> bool {
    matches!(
        call,
        ToolCall::WriteFile { .. } | ToolCall::EditFile { .. } | ToolCall::ApplyPatch { .. }
    )
}

#[derive(Default)]
pub(super) struct ChangeTreeNode {
    directories: BTreeMap<String, ChangeTreeNode>,
    files: Vec<(String, usize)>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ChangeTreeRow {
    Directory {
        path: String,
        name: String,
        depth: usize,
        collapsed: bool,
    },
    File {
        file_index: usize,
        name: String,
        depth: usize,
    },
}

pub(super) fn change_tree_rows(
    files: &[jolt_proto::DiffFileDescriptor],
    collapsed_paths: Option<&HashSet<String>>,
) -> Vec<ChangeTreeRow> {
    let mut root = ChangeTreeNode::default();
    for (file_index, file) in files.iter().enumerate() {
        let mut components: Vec<_> = file
            .path
            .split('/')
            .filter(|component| !component.is_empty())
            .collect();
        let Some(name) = components.pop() else {
            continue;
        };
        let mut node = &mut root;
        for component in components {
            node = node.directories.entry(component.to_string()).or_default();
        }
        node.files.push((name.to_string(), file_index));
    }

    fn flatten(
        node: &mut ChangeTreeNode,
        parent: &str,
        depth: usize,
        collapsed_paths: Option<&HashSet<String>>,
        rows: &mut Vec<ChangeTreeRow>,
    ) {
        for (name, child) in &mut node.directories {
            let path = if parent.is_empty() {
                name.clone()
            } else {
                format!("{parent}/{name}")
            };
            let collapsed = collapsed_paths.is_some_and(|paths| paths.contains(&path));
            rows.push(ChangeTreeRow::Directory {
                path: path.clone(),
                name: name.clone(),
                depth,
                collapsed,
            });
            if !collapsed {
                flatten(child, &path, depth + 1, collapsed_paths, rows);
            }
        }
        node.files.sort_by(|(left, _), (right, _)| left.cmp(right));
        rows.extend(
            node.files
                .iter()
                .map(|(name, file_index)| ChangeTreeRow::File {
                    file_index: *file_index,
                    name: name.clone(),
                    depth,
                }),
        );
    }

    let mut rows = Vec::new();
    flatten(&mut root, "", 0, collapsed_paths, &mut rows);
    rows
}

pub(super) fn tool_fingerprint(tools: &[ToolItem], active: bool) -> u64 {
    let mut acc = Vec::with_capacity(tools.len() * 8 + 1);
    for t in tools {
        let (label, detail) = tool_chip_content(&t.call);
        acc.extend_from_slice(label.as_bytes());
        acc.extend_from_slice(&(detail.len() as u32).to_le_bytes());
        acc.push(t.is_error as u8 | (t.resolved as u8) << 1);
    }
    acc.push(active as u8);
    fnv1a(&acc)
}

/// Build the block rows of one (already continuation-joined) entry.
///
/// `parse` maps `(part_key, text)` to a block tree — the entity supplies
/// incremental parsers for live parts and a cache for complete ones; tests pass
/// a plain `parse_full`.
pub fn rows_for_entry(
    entry: &SessionMessageEntry,
    pending: bool,
    parse: &mut dyn FnMut(&str, &str) -> Arc<BlockTree>,
) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let streaming = entry.status == Some(MessageStatus::Streaming);
    let entry_id: SharedString = entry.id.clone().into();

    if entry.role == MessageRole::User {
        let raw: String = entry
            .parts
            .iter()
            .filter_map(|p| match p {
                MessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        // Attachment refs ride the plain text; split them back out for the
        // thumbnail strip.
        let parsed = crate::attachments::parse_user_message_images(&raw);
        // File mentions keep their compact inline-code treatment while the
        // rest of the prompt is parsed as Markdown (including inline and
        // fenced code). User entries are already row-cached, so a full parse
        // happens only when the entry changes.
        let markdown = match crate::composer::sent_mention_display(&parsed.text) {
            Some((display, spans)) => user_markdown_source(&display, &spans),
            None => parsed.text,
        };
        return vec![Row {
            id: entry.id.clone().into(),
            version: (raw.len() as u64) << 1 | pending as u64,
            turn_start: true,
            kind: RowKind::User {
                tree: Arc::new(parse_full(&markdown)),
                attachments: Arc::new(parsed.attachments),
                pending,
            },
            entry_id,
            // User rows always carry the strip when `createdAt` exists,
            // including the optimistic echo.
            timestamp: Some(entry.created_at),
            copy_text: message_copy_text(entry),
        }];
    }

    // Assistant/system: split parts into block rows, folding consecutive tools.
    let has_successful_file_mutation = entry.parts.iter().any(|part| {
        matches!(
            part,
            MessagePart::Tool {
                call,
                is_error: false,
                resolved: true,
                ..
            } if is_file_mutation(call)
        )
    });
    let show_changes = has_successful_file_mutation
        && entry
            .parts
            .iter()
            .any(|part| matches!(part, MessagePart::Changes { .. }));
    let last_reveal_ix = entry
        .parts
        .iter()
        .rposition(|part| matches!(part, MessagePart::TextReveal { .. }));
    let last_content_ix = entry
        .parts
        .iter()
        .rposition(|part| !matches!(part, MessagePart::TextReveal { .. }))
        .unwrap_or_default();
    let mut group_ix = 0usize;
    let mut pending_group: Vec<ToolItem> = Vec::new();
    let mut group_last_part_ix = 0usize;

    let flush_group =
        |rows: &mut Vec<Row>, group: &mut Vec<ToolItem>, group_ix: &mut usize, last_ix: usize| {
            if group.is_empty() {
                return;
            }
            let tools = std::mem::take(group);
            let active = streaming && last_ix == last_content_ix;
            rows.push(Row {
                id: format!("{}#g{}", entry.id, group_ix).into(),
                version: tool_fingerprint(&tools, active),
                turn_start: false,
                kind: RowKind::ToolGroup {
                    tools: Arc::new(tools),
                    active,
                },
                entry_id: entry.id.clone().into(),
                timestamp: None,
                copy_text: None,
            });
            *group_ix += 1;
        };

    for (part_ix, part) in entry.parts.iter().enumerate() {
        match part {
            MessagePart::Tool {
                call,
                is_error,
                resolved,
                ..
            } => {
                pending_group.push(ToolItem {
                    call: call.clone(),
                    is_error: *is_error,
                    resolved: *resolved,
                });
                group_last_part_ix = part_ix;
            }
            other => {
                flush_group(
                    &mut rows,
                    &mut pending_group,
                    &mut group_ix,
                    group_last_part_ix,
                );
                match other {
                    MessagePart::Text { id: part_id, text } => {
                        let revealed = !streaming
                            || last_reveal_ix.is_some_and(|reveal_ix| part_ix < reveal_ix);
                        if !revealed || text.trim().is_empty() {
                            continue;
                        }
                        let key = format!("{}#{}", entry.id, part_id);
                        let tree = parse(&key, text);
                        // Boundary-completed prose appears as one stable chunk:
                        // no partial Markdown, live-tail veil, or per-token row
                        // churn. The entry may still be streaming tools.
                        for block_ix in 0..tree.blocks.len() {
                            let range = &tree.blocks[block_ix].range;
                            let end = range.end.min(text.len());
                            let bytes = text
                                .as_bytes()
                                .get(range.start.min(end)..end)
                                .unwrap_or_default();
                            rows.push(Row {
                                id: format!("{key}.{block_ix}").into(),
                                version: fnv1a(bytes) << 1,
                                turn_start: false,
                                entry_id: entry_id.clone(),
                                timestamp: None,
                                copy_text: None,
                                kind: RowKind::Markdown {
                                    tree: tree.clone(),
                                    block_ix,
                                },
                            });
                        }
                    }
                    MessagePart::TextReveal { .. } => {}
                    MessagePart::Input {
                        id: part_id,
                        questions,
                        resolved,
                        ..
                    } => {
                        // Model-generated header onto the one-line chip.
                        let header: SharedString = single_line(
                            &questions
                                .first()
                                .map(|q| q.header.clone())
                                .unwrap_or_else(|| "Question".to_string()),
                        )
                        .into();
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id).into(),
                            version: fnv1a(header.as_bytes()) << 1 | *resolved as u64,
                            turn_start: false,
                            kind: RowKind::InputChip {
                                header,
                                resolved: *resolved,
                            },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                    MessagePart::Error {
                        id: part_id,
                        message,
                    } => {
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id).into(),
                            version: message.len() as u64,
                            turn_start: false,
                            kind: RowKind::ErrorChip {
                                // Harness-generated; the chip is one line.
                                message: single_line(message).into(),
                            },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                    MessagePart::HarnessSwitch {
                        id: part_id,
                        from,
                        to,
                    } => {
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id).into(),
                            version: fnv1a(format!("{from:?}:{to:?}").as_bytes()),
                            turn_start: false,
                            kind: RowKind::HarnessSwitch {
                                from: *from,
                                to: *to,
                            },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                    MessagePart::Changes { id: part_id, diff } => {
                        if !show_changes {
                            continue;
                        }
                        rows.push(Row {
                            id: format!("{}#{}", entry.id, part_id).into(),
                            version: fnv1a(diff.catalog_revision.as_bytes()),
                            turn_start: false,
                            kind: RowKind::Changes {
                                diff: Arc::new(diff.clone()),
                            },
                            entry_id: entry_id.clone(),
                            timestamp: None,
                            copy_text: None,
                        });
                    }
                    // Tools are grouped by the outer arm; nothing reaches here.
                    MessagePart::Tool { .. } => {}
                }
            }
        }
    }
    flush_group(
        &mut rows,
        &mut pending_group,
        &mut group_ix,
        group_last_part_ix,
    );

    if let Some(first) = rows.first_mut() {
        first.turn_start = true;
    }
    // Timestamp strip under the entry's last row once the turn has settled;
    // there is no timestamp hover mid-stream. The version bit keeps
    // the diff key honest for last-row kinds whose own version wouldn't
    // change when streaming flips off (chips).
    if !streaming && let Some(last) = rows.last_mut() {
        last.timestamp = Some(entry.created_at);
        last.copy_text = message_copy_text(entry);
        last.version ^= 1 << 62;
    }
    rows
}

/// `JOLT_FRAME_STATS=1` logs live-row render-cost percentiles (p50/p95 µs
/// over rolling windows of [`FRAME_STATS_WINDOW`] samples) at `warn` level —
/// the smoothness measurement knob. Off by default; zero cost when off.
pub(super) fn frame_stats_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var("JOLT_FRAME_STATS").is_ok_and(|v| !v.is_empty() && v != "0"))
}

pub(super) const FRAME_STATS_WINDOW: usize = 240;

/// `JOLT_NO_RENDER_CACHE=1` bypasses the cross-frame flatten cache — the
/// A/B knob for the frame-cost measurement above.
pub(super) fn render_cache_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| {
        std::env::var("JOLT_NO_RENDER_CACHE").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

pub(super) fn record_live_frame_us(us: u64) {
    thread_local! {
        static SAMPLES: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    }
    SAMPLES.with(|s| {
        let mut s = s.borrow_mut();
        s.push(us);
        if s.len() >= FRAME_STATS_WINDOW {
            s.sort_unstable();
            let p50 = s[s.len() / 2];
            let p95 = s[s.len() * 95 / 100];
            let max = *s.last().unwrap();
            tracing::warn!(
                n = s.len(),
                p50_us = p50,
                p95_us = p95,
                max_us = max,
                "live-row render cost"
            );
            s.clear();
        }
    });
}

/// How [`parse_for_row`] produced its tree — carries the incremental parser's
/// work counters so callers (and tests) can see that per-append parse work is
/// bounded by the reparsed tail, never the whole accumulated reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseOutcome {
    /// Streaming row: the live [`IncrementalParser`] advanced by one commit.
    Incremental {
        /// Bytes fed through `parse_full` for this commit (the reparse tail).
        parsed_bytes: usize,
        /// Leading top-level blocks left untouched (render caches stay valid).
        stable_prefix_blocks: usize,
    },
    /// Completed row served from the settled tree cache (no parse at all).
    Cached,
    /// Live→complete handoff: the live parser's exact tree was adopted.
    Handoff,
    /// Completed row parsed from scratch.
    Full,
}

/// The transcript's markdown parse wiring, extracted for testability: one call
/// per text part per sync. Streaming parts keep one [`IncrementalParser`] per
/// row key and advance it with the full accumulated text (`set_text` takes the
/// O(tail) append path for the prefix-extensions the doc watch delivers);
/// completed parts hit the settled cache, adopt the live parser's tree on the
/// live→complete flip (flicker-free handoff), or do one full parse.
pub fn parse_for_row(
    streaming: bool,
    key: &str,
    text: &str,
    live_parsers: &mut HashMap<String, IncrementalParser>,
    tree_cache: &mut HashMap<String, (usize, Arc<BlockTree>)>,
) -> (Arc<BlockTree>, ParseOutcome) {
    if streaming {
        let parser = live_parsers.entry(key.to_string()).or_default();
        parser.set_text(text);
        (
            // Display tree: hanging inline markers mended so closers arriving
            // later never reflow painted text (markdown/mend.rs). Completed
            // rows below use the canonical tree — the honest settle.
            Arc::new(parser.display_tree()),
            ParseOutcome::Incremental {
                parsed_bytes: parser.last_parse_bytes(),
                stable_prefix_blocks: parser.stable_prefix_blocks(),
            },
        )
    } else {
        if let Some((len, tree)) = tree_cache.get(key)
            && *len == text.len()
        {
            return (tree.clone(), ParseOutcome::Cached);
        }
        // On the live→complete flip reuse the live parser's tree when
        // the sources match — the split rows then share the exact tree
        // the unsplit row painted, guaranteeing a flicker-free handoff.
        let (tree, outcome) = match live_parsers.remove(key) {
            Some(parser) if parser.source() == text => {
                (Arc::new(parser.tree().clone()), ParseOutcome::Handoff)
            }
            _ => (Arc::new(parse_full(text)), ParseOutcome::Full),
        };
        tree_cache.insert(key.to_string(), (text.len(), tree.clone()));
        (tree, outcome)
    }
}

/// Markdown row ids are `{entry}#{part}.{blockIx}` — the part prefix is
/// everything before the block index.
pub(super) fn part_prefix(id: &str) -> &str {
    id.rsplit_once('.').map(|(p, _)| p).unwrap_or(id)
}

/// Vertical gap opening `row` given its predecessor: turn gap at turn starts;
/// the markdown block gap between sibling block rows split from the same text
/// part — matching the live row's internal spacing exactly, so the
/// live→split handoff cannot shift a pixel; the block gap otherwise.
pub fn top_gap_for(prev: Option<&Row>, row: &Row) -> f32 {
    if row.turn_start {
        return GAP_TURN;
    }
    let is_md = |k: &RowKind| matches!(k, RowKind::Markdown { .. } | RowKind::LiveMarkdown { .. });
    let same_part_markdown = prev.is_some_and(|p| {
        is_md(&p.kind) && is_md(&row.kind) && part_prefix(&p.id) == part_prefix(&row.id)
    });
    if same_part_markdown {
        render::MD_BLOCK_GAP
    } else {
        GAP_BLOCK
    }
}

/// Minimal splice for a row-set change: `Some((old_range, new_count))`, or
/// `None` when the sets are identical by (id, version).
pub fn diff_rows(old: &[Row], new: &[Row]) -> Option<(Range<usize>, usize)> {
    let eq = |a: &Row, b: &Row| a.id == b.id && a.version == b.version;
    let mut prefix = 0usize;
    let max_prefix = old.len().min(new.len());
    while prefix < max_prefix && eq(&old[prefix], &new[prefix]) {
        prefix += 1;
    }
    if prefix == old.len() && prefix == new.len() {
        return None;
    }
    let mut suffix = 0usize;
    let max_suffix = (old.len() - prefix).min(new.len() - prefix);
    while suffix < max_suffix && eq(&old[old.len() - 1 - suffix], &new[new.len() - 1 - suffix]) {
        suffix += 1;
    }
    Some((prefix..old.len() - suffix, new.len() - suffix - prefix))
}

/// Whether a diff only updates existing rows without changing their identity.
/// These rows can be remeasured in place, preserving the viewport's pixel
/// offset inside a tall row while its content grows.
pub(super) fn rows_changed_in_place(
    old: &[Row],
    new: &[Row],
    old_range: &Range<usize>,
    new_count: usize,
) -> bool {
    old_range.len() == new_count
        && old[old_range.clone()]
            .iter()
            .zip(&new[old_range.start..old_range.start + new_count])
            .all(|(old, new)| old.id == new.id)
}

// ---------------------------------------------------------------------------
// Tool summaries / chips (pure)
// ---------------------------------------------------------------------------

/// The ToolGroup summary line — "Ran 3 commands · edited 2 files".
///
/// The rule lives in `jolt_proto::view` so the terminal viewport reports the
/// same summary; this only adapts the row model's [`ToolItem`] to it.
pub fn tool_group_summary(tools: &[ToolItem]) -> String {
    let pairs: Vec<(ToolCall, bool)> = tools.iter().map(|t| (t.call.clone(), t.is_error)).collect();
    jolt_proto::view::tool_group_summary(&pairs)
}

// `single_line` and the per-kind chip label/detail are shared with the terminal
// viewport (`jolt_proto::view`): a tool must be named identically on every
// surface, and the one-line collapse is needed for the same reason in both (a
// literal newline breaks gpui's ellipsis logic and would be a cursor move in a
// cell grid).
pub use jolt_proto::view::{single_line, tool_chip_content};

/// Analytic expanded-chips height — no measurement needed for the fold tween.
pub fn chips_height(count: usize) -> f32 {
    if count == 0 {
        return 0.0;
    }
    CHIPS_TOP_PAD + count as f32 * CHIP_HEIGHT + (count as f32 - 1.0) * CHIP_GAP
}

/// Tools visible for a group's current fold state. Collapsed active groups
/// retain only the latest chip; inactive groups retain none.
pub(super) fn visible_tool_range(count: usize, open: bool, active: bool) -> Range<usize> {
    if open {
        0..count
    } else if active && count > 0 {
        count - 1..count
    } else {
        count..count
    }
}

// ---------------------------------------------------------------------------
// Working indicator flavour (pure; rendered by the shell strip)
// ---------------------------------------------------------------------------

/// Rotating working-message vocabulary, changing every seven seconds and
/// seeded per chat. Trailing ellipses are omitted because the shell adds one.
pub const FLAVOUR_WORDS: [&str; 453] = [
    "Schlepping",
    "Combobulating",
    "Doing",
    "Channelling",
    "Vibing",
    "Concocting",
    "Spelunking",
    "Transmuting",
    "Imagining",
    "Pontificating",
    "Whirring",
    "Cogitating",
    "Honking",
    "Flibbertigibbeting",
    "Noodling",
    "Percolating",
    "Ruminating",
    "Simmering",
    "Marinating",
    "Fermenting",
    "Gestating",
    "Hatching",
    "Brewing",
    "Steeping",
    "Contemplating",
    "Musing",
    "Pondering",
    "Mulling",
    "Daydreaming",
    "Woolgathering",
    "Dithering",
    "Faffing",
    "Puttering",
    "Tinkering",
    "Fiddling",
    "Noodging",
    "Finagling",
    "Wrangling",
    "Jiggling",
    "Wiggling",
    "Shimmying",
    "Galumphing",
    "Perambulating",
    "Meandering",
    "Traipsing",
    "Moseying",
    "Sauntering",
    "Ambling",
    "Pottering",
    "Bumbling",
    "Futzing",
    "Schmalzing",
    "Kerfuffling",
    "Bamboozling",
    "Discombobulating",
    "Recombobulating",
    "Unbefuddling",
    "Defenestrating",
    "Confabulating",
    "Persnicketing",
    "Flummoxing",
    "Befuddling",
    "Snorkeling",
    "Yodeling",
    "Zigzagging",
    "Ricocheting",
    "Somersaulting",
    "Pirouetting",
    "Canoodling",
    "Schmoozing",
    "Kibbitzing",
    "Skedaddling",
    "Scampering",
    "Skittering",
    "Sashaying",
    "Swashbuckling",
    "Oscillating",
    "Undulating",
    "Pulsating",
    "Effervescing",
    "Fizzing",
    "Bubbling",
    "Perplexing",
    "Mystifying",
    "Enchanting",
    "Bewitching",
    "Beguiling",
    "Mesmerizing",
    "Bedazzling",
    "Sparkling",
    "Glittering",
    "Scintillating",
    "Coruscating",
    "Phosphorescing",
    "Luminescing",
    "Sublimating",
    "Synthesizing",
    "Amalgamating",
    "Procrastinating",
    "Dillydallying",
    "Lollygagging",
    "Dawdling",
    "Malingering",
    "Skulking",
    "Lurking",
    "Sleuthing",
    "Rummaging",
    "Fossicking",
    "Foraging",
    "Scavenging",
    "Absquatulating",
    "Vamoosing",
    "Absconding",
    "Grooving",
    "Jamming",
    "Improvising",
    "Extemporizing",
    "Freestyling",
    "Frolicking",
    "Gamboling",
    "Blorping",
    "Flonking",
    "Snurfling",
    "Whomping",
    "Zorping",
    "Biffing",
    "Splunging",
    "Thwacking",
    "Gonkulating",
    "Splorfing",
    "Wibbling",
    "Wobbling",
    "Squonking",
    "Plonking",
    "Bonking",
    "Zonking",
    "Flumping",
    "Clomping",
    "Squelching",
    "Schlurping",
    "Glurping",
    "Burbling",
    "Gurgling",
    "Splooshing",
    "Whooshing",
    "Swooshing",
    "Kerplunking",
    "Thunking",
    "Clunking",
    "Clanking",
    "Rattling",
    "Jostling",
    "Rustling",
    "Bustling",
    "Hustling",
    "Miffing",
    "Boffing",
    "Snazzifying",
    "Pizzazzing",
    "Razzmatazzing",
    "Bedoodling",
    "Doodling",
    "Scribbling",
    "Squiggling",
    "Wriggling",
    "Niggling",
    "Higgling",
    "Piggling",
    "Figgling",
    "Gibbering",
    "Jabbering",
    "Blathering",
    "Blithering",
    "Withering",
    "Slithering",
    "Tethering",
    "Feathering",
    "Weathering",
    "Leathering",
    "Heathering",
    "Smoldering",
    "Moldering",
    "Shouldering",
    "Bouldering",
    "Tottering",
    "Teetering",
    "Tittering",
    "Flittering",
    "Jittering",
    "Frittering",
    "Twittering",
    "Nattering",
    "Chattering",
    "Clattering",
    "Splattering",
    "Battering",
    "Scattering",
    "Shattering",
    "Flattering",
    "Pattering",
    "Tattering",
    "Mattering",
    "Yammering",
    "Hammering",
    "Stammering",
    "Clamoring",
    "Glamoring",
    "Enamoring",
    "Shimmering",
    "Glimmering",
    "Brimming",
    "Skimming",
    "Trimming",
    "Primming",
    "Whimming",
    "Humming",
    "Strumming",
    "Thrumming",
    "Drumming",
    "Plumbing",
    "Thumbing",
    "Numbing",
    "Fumbling",
    "Grumbling",
    "Mumbling",
    "Rumbling",
    "Stumbling",
    "Tumbling",
    "Crumbling",
    "Jumbling",
    "Humbling",
    "Bungling",
    "Jungling",
    "Mangling",
    "Wangling",
    "Dangling",
    "Tangling",
    "Jangling",
    "Angling",
    "Struggling",
    "Mingling",
    "Tingling",
    "Jingling",
    "Singling",
    "Ringling",
    "Kingling",
    "Consulting the void",
    "Asking the electrons",
    "Bribing the compiler",
    "Negotiating with entropy",
    "Whispering to the bits",
    "Tickling the stack",
    "Massaging the heap",
    "Appeasing the garbage collector",
    "Summoning semicolons",
    "Herding pointers",
    "Untangling spaghetti",
    "Polishing the algorithms",
    "Waxing philosophical",
    "Consulting ancient scrolls",
    "Reading tea leaves",
    "Shaking the magic 8-ball",
    "Sacrificing to the demo gods",
    "Warming up the hamsters",
    "Spinning up the squirrels",
    "Caffeinating",
    "Existentially questioning",
    "Having a little think",
    "Stroking chin thoughtfully",
    "Squinting at the problem",
    "Staring into the abyss",
    "Abyss staring back",
    "Achieving enlightenment",
    "Transcending mere computation",
    "Ascending to a higher plane",
    "Communing with the machine spirit",
    "Performing arcane rituals",
    "Invoking elder functions",
    "Consulting the oracle",
    "Divining the answer",
    "Scrying the codebase",
    "Dowsing for bugs",
    "Rearranging deck chairs",
    "Shuffling bits around",
    "Aligning the chakras",
    "Reticulating splines",
    "Reversing the polarity",
    "Calibrating the flux capacitor",
    "Charging the crystals",
    "Tuning the vibrations",
    "Adjusting the cosmic frequency",
    "Waiting for a sign",
    "Hoping for the best",
    "Manifesting solutions",
    "Willing it into existence",
    "Believing really hard",
    "Politely asking the CPU",
    "Bribing the runtime",
    "Flirting with the database",
    "Sweet-talking the API",
    "Negotiating with deadlines",
    "Having words with the cache",
    "Reasoning with the memory",
    "Pleading with the logs",
    "Bargaining with fate",
    "Making offerings to the CI",
    "Praying to the uptime gods",
    "Consulting the rubber duck",
    "Interrogating the stack trace",
    "Cross-examining the debugger",
    "Petitioning the kernel",
    "Lobbying the scheduler",
    "Schmoozing the network",
    "Buttering up the firewall",
    "Wining and dining the servers",
    "Taking the bytes out for lunch",
    "Giving the code a pep talk",
    "Reading the room",
    "Checking under the hood",
    "Kicking the tires",
    "Shaking loose the cobwebs",
    "Dusting off the neurons",
    "Greasing the gears",
    "Oiling the cogs",
    "Winding up the clockwork",
    "Stoking the furnace",
    "Feeding the machine",
    "Watering the logic tree",
    "Pruning the decision branches",
    "Harvesting the outputs",
    "Planting computational seeds",
    "Nurturing the algorithm",
    "Raising the exceptions",
    "Taming wild pointers",
    "Herding cats in memory",
    "Teaching old code new tricks",
    "Whispering sweet nothings to the compiler",
    "Serenading the syntax",
    "Dancing with dependencies",
    "Waltzing through the codebase",
    "Tangoing with type errors",
    "Doing the deployment dance",
    "Having a moment of clarity",
    "Experiencing a flash of insight",
    "Channeling the ancient developers",
    "Receiving transmissions from the cloud",
    "Asking the hamsters to run faster",
    "Convincing the pixels to cooperate",
    "Teaching electrons new tricks",
    "Bribing the byte fairies",
    "Whispering passwords to the void",
    "Negotiating with cosmic rays",
    "Flattering the floating points",
    "Seducing the semicolons",
    "Wooing the while loops",
    "Charming the curly braces",
    "Hypnotizing the hash tables",
    "Mesmerizing the memory banks",
    "Enchanting the error handlers",
    "Bewitching the boolean logic",
    "Spellbinding the stack frames",
    "Hexing the hexadecimals",
    "Jinxing the JSON parsers",
    "Cursing the cache misses",
    "Blessing the build process",
    "Anointing the algorithms",
    "Consecrating the callbacks",
    "Sanctifying the source code",
    "Exorcising the exceptions",
    "Purifying the parameters",
    "Cleansing the closures",
    "Baptizing the binary",
    "Absolving the abstractions",
    "Redeeming the recursion",
    "Forgiving the for loops",
    "Pardoning the pointers",
    "Liberating the lambdas",
    "Emancipating the enums",
    "Freeing the functions",
    "Releasing the references",
    "Unbinding the variables",
    "Untying the type knots",
    "Unraveling the regex",
    "Decoding the mysteries",
    "Cracking the conundrums",
    "Solving the riddles of RAM",
    "Unlocking the secrets of silicon",
    "Discovering hidden semicolons",
    "Unearthing buried bugs",
    "Excavating ancient APIs",
    "Archeologically analyzing the architecture",
    "Fossil hunting in the functions",
    "Spelunking through the stack",
    "Scuba diving in the data",
    "Snorkeling through the streams",
    "Parasailing past the parameters",
    "Hang gliding through the heap",
    "Bungee jumping into the backend",
    "Skydiving through the source",
    "Surfing the syntax waves",
    "Skateboarding down the stack trace",
    "Snowboarding through the schemas",
    "Mountain climbing the modules",
    "Hiking through the headers",
    "Trekking through the trees",
    "Backpacking through the binaries",
    "Camping in the codebase",
    "Glamping in the globals",
    "Picnicking with the processes",
    "Barbecuing the bugs",
    "Roasting the race conditions",
    "Grilling the glitches",
    "Sautéing the syntax errors",
    "Flambéing the failures",
    "Caramelizing the callbacks",
    "Braising the breakpoints",
    "Poaching the pointers",
    "Blanching the branches",
    "Searing the segments",
    "Smoking the subroutines",
    "Curing the code smells",
    "Pickling the packages",
    "Preserving the protocols",
    "Canning the constants",
    "Bottling the buffers",
    "Jarring the JavaScript",
    "Decanting the data structures",
    "Aerating the arrays",
    "Letting the logic breathe",
    "Aging the algorithms gracefully",
    "Maturing the methods",
    "Ripening the results",
    "Seasoning the solutions",
    "Spicing up the specs",
    "Garnishing the getters",
    "Plating the output nicely",
    "Presenting with pizzazz",
    "Adding a dash of elegance",
    "Sprinkling some magic dust",
    "Drizzling debug sauce",
    "Folding in the features",
    "Whisking the widgets",
    "Kneading the namespaces",
    "Rolling out the runtime",
    "Proofing the promises",
    "Letting the dough rise",
    "Baking at 350 kilobytes",
    "Frosting the functions",
    "Decorating the deployment",
    "Icing the interfaces",
    "Glazing the graphics",
    "Topping with tests",
    "Cherry-picking the commits",
];
pub const FLAVOUR_ROTATE_SECS: i64 = 7;

/// The whimsical working message for a seed at an elapsed time.
pub fn flavour_word(seed: u64, elapsed_secs: i64) -> &'static str {
    let step = (elapsed_secs.max(0) / FLAVOUR_ROTATE_SECS) as u64;
    FLAVOUR_WORDS[((seed.wrapping_add(step)) % FLAVOUR_WORDS.len() as u64) as usize]
}

/// A stable per-chat seed.
pub fn flavour_seed(chat_id: &str) -> u64 {
    fnv1a(chat_id.as_bytes())
}

/// "1m 32s"-style elapsed formatting.
pub fn format_elapsed(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}
