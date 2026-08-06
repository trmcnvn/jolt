//! Paged, checkout-specific right-sidebar diff viewer.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, ListAlignment, ListScrollEvent, ListState,
    Render, ScrollHandle, SharedString, Subscription, Task, Window, div, font, list, prelude::*,
    px,
};
use jolt_proto::{
    CheckoutDiffManifest, CheckoutDiffPage, CheckoutDiffWatchFrame, DiffCompleteness,
    DiffFileDescriptor, TurnDiffManifest,
};
use jolt_rpc::methods;

use crate::markdown::highlight::{Lang, LineCarry, Token, lang_for_tag, tokenize_line};
use crate::markdown::render;
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

pub const FILE_HEADER_HEIGHT: f32 = 36.0;
pub const HUNK_HEADER_HEIGHT: f32 = 28.0;
pub const DIFF_LINE_HEIGHT: f32 = 22.0;
pub const NOTICE_HEIGHT: f32 = 24.0;
pub const GUTTER_WIDTH: f32 = 36.0;
pub const MARKER_WIDTH: f32 = 28.0;
pub const ACCENT_BAR_WIDTH: f32 = 3.0;
const MONOSPACE_GLYPH_WIDTH_RATIO: f32 = 0.62;
const PAGE_CACHE_BYTES: usize = 16 * 1024 * 1024;
const HIGHLIGHT_CACHE_BYTES: usize = 16 * 1024 * 1024;

pub enum ChangesEvent {
    ToggleExpanded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Add,
    Del,
    Meta,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    pub notices: Vec<String>,
    pub hunks: Vec<Hunk>,
    pub additions: u32,
    pub deletions: u32,
}

impl FileDiff {
    fn new(path: String, old_path: Option<String>) -> Self {
        Self {
            path,
            old_path,
            status: FileStatus::Modified,
            binary: false,
            notices: Vec::new(),
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
        }
    }
}

fn strip_git_prefix(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

fn parse_git_paths(rest: &str) -> (String, String) {
    fn unquote(value: &str) -> String {
        let value = value.trim();
        if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
            value[1..value.len() - 1]
                .replace("\\\"", "\"")
                .replace("\\\\", "\\")
        } else {
            value.to_string()
        }
    }
    if let Some(position) = rest.rfind(" b/").or_else(|| rest.rfind(" \"b/")) {
        let old = unquote(&rest[..position]);
        let new = unquote(&rest[position + 1..]);
        (
            strip_git_prefix(&old).to_string(),
            strip_git_prefix(&new).to_string(),
        )
    } else {
        let path = strip_git_prefix(&unquote(rest)).to_string();
        (path.clone(), path)
    }
}

fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let minus = line.find('-')?;
    let old = line[minus + 1..]
        .split(|character: char| character == ',' || character.is_whitespace())
        .next()?
        .parse()
        .ok()?;
    let plus = line.find('+')?;
    let new = line[plus + 1..]
        .split(|character: char| character == ',' || character.is_whitespace())
        .next()?
        .parse()
        .ok()?;
    Some((old, new))
}

pub fn parse_patch(patch: &str) -> Vec<FileDiff> {
    let mut files = Vec::new();
    let mut in_hunk = false;
    let mut old_no = 0u32;
    let mut new_no = 0u32;
    for raw in patch.lines() {
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            let (old, new) = parse_git_paths(rest);
            let old_path = (old != new).then_some(old);
            files.push(FileDiff::new(new, old_path));
            in_hunk = false;
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if raw.starts_with("@@") {
            if let Some((old, new)) = parse_hunk_header(raw) {
                old_no = old;
                new_no = new;
                file.hunks.push(Hunk {
                    header: raw.to_string(),
                    lines: Vec::new(),
                });
                in_hunk = true;
            }
            continue;
        }
        if in_hunk {
            let marker = raw.as_bytes().first().copied();
            let body = raw.get(1..).unwrap_or_default().to_string();
            let line = match marker {
                Some(b'+') => {
                    file.additions += 1;
                    let line = DiffLine {
                        kind: LineKind::Add,
                        old_no: None,
                        new_no: Some(new_no),
                        text: body,
                    };
                    new_no += 1;
                    Some(line)
                }
                Some(b'-') => {
                    file.deletions += 1;
                    let line = DiffLine {
                        kind: LineKind::Del,
                        old_no: Some(old_no),
                        new_no: None,
                        text: body,
                    };
                    old_no += 1;
                    Some(line)
                }
                Some(b' ') | None => {
                    let line = DiffLine {
                        kind: LineKind::Context,
                        old_no: Some(old_no),
                        new_no: Some(new_no),
                        text: body,
                    };
                    old_no += 1;
                    new_no += 1;
                    Some(line)
                }
                Some(b'\\') => Some(DiffLine {
                    kind: LineKind::Meta,
                    old_no: None,
                    new_no: None,
                    text: raw.trim_start_matches('\\').trim().to_string(),
                }),
                _ => {
                    in_hunk = false;
                    None
                }
            };
            if let Some(line) = line
                && let Some(hunk) = file.hunks.last_mut()
            {
                hunk.lines.push(line);
                continue;
            }
        }
        if raw.starts_with("new file mode") {
            file.status = FileStatus::Added;
        } else if raw.starts_with("deleted file mode") {
            file.status = FileStatus::Deleted;
        } else if let Some(from) = raw.strip_prefix("rename from ") {
            file.status = FileStatus::Renamed;
            file.old_path = Some(from.trim().to_string());
        } else if let Some(to) = raw.strip_prefix("rename to ") {
            file.status = FileStatus::Renamed;
            file.path = to.trim().to_string();
        } else if raw.starts_with("Binary files") || raw == "GIT binary patch" {
            file.binary = true;
        } else if let Some(mode) = raw.strip_prefix("new mode ") {
            file.notices
                .push(format!("Mode changed to {}", mode.trim()));
        } else if let Some(new) = raw.strip_prefix("+++ ") {
            if new.trim() == "/dev/null" {
                file.status = FileStatus::Deleted;
            }
        } else if let Some(old) = raw.strip_prefix("--- ")
            && old.trim() == "/dev/null"
        {
            file.status = FileStatus::Added;
        }
    }
    files
}

pub fn file_notices(file: &FileDiff) -> Vec<String> {
    let mut notices = Vec::new();
    match file.status {
        FileStatus::Added => notices.push("New file".to_string()),
        FileStatus::Deleted => notices.push("Deleted file".to_string()),
        FileStatus::Renamed => notices.push(format!(
            "Renamed from {}",
            file.old_path.as_deref().unwrap_or("?")
        )),
        FileStatus::Modified => {}
    }
    if file.binary {
        notices.push("Binary file — contents not shown".to_string());
    }
    notices.extend(file.notices.iter().cloned());
    notices
}

pub fn lang_for_path(path: &str) -> Option<Lang> {
    lang_for_tag(path.rsplit('/').next()?.rsplit('.').next()?)
}

fn display_columns(text: &str) -> usize {
    text.chars().fold(0, |columns, character| {
        if character == '\t' {
            columns + (4 - columns % 4)
        } else {
            columns + 1
        }
    })
}

fn file_display_columns(file: &FileDiff) -> usize {
    let mut max_columns = file_notices(file)
        .iter()
        .map(|notice| display_columns(notice))
        .max()
        .unwrap_or(0);
    for hunk in &file.hunks {
        max_columns = max_columns.max(display_columns(&hunk.header));
        for line in &hunk.lines {
            max_columns = max_columns.max(display_columns(&line.text));
        }
    }
    max_columns
}

fn hash64(parts: &[&str]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    for part in parts {
        part.hash(&mut hasher);
    }
    hasher.finish()
}

#[derive(Clone)]
struct CachedPage {
    file: Arc<FileDiff>,
    bytes: usize,
    access: u64,
}

struct HighlightSlot {
    lines: Option<Arc<Vec<Vec<Token>>>>,
    bytes: usize,
    _task: Option<Task<()>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ChangeRowKind {
    FileHeader {
        file: usize,
    },
    PagePlaceholder {
        file: usize,
        page_id: String,
    },
    Unavailable {
        file: usize,
    },
    Notice {
        file: usize,
        page_id: String,
        notice: usize,
    },
    HunkHeader {
        file: usize,
        page_id: String,
        hunk: usize,
    },
    Line {
        file: usize,
        page_id: String,
        hunk: usize,
        line: usize,
        flat_line: usize,
    },
}

#[derive(Clone, Debug)]
struct ChangeRow {
    id: String,
    version: u64,
    kind: ChangeRowKind,
}

fn row_splice(old: &[ChangeRow], new: &[ChangeRow]) -> Option<(Range<usize>, usize)> {
    let same =
        |left: &ChangeRow, right: &ChangeRow| left.id == right.id && left.version == right.version;
    let mut prefix = 0;
    while prefix < old.len().min(new.len()) && same(&old[prefix], &new[prefix]) {
        prefix += 1;
    }
    if prefix == old.len() && prefix == new.len() {
        return None;
    }
    let mut suffix = 0;
    while suffix < (old.len() - prefix).min(new.len() - prefix)
        && same(&old[old.len() - 1 - suffix], &new[new.len() - 1 - suffix])
    {
        suffix += 1;
    }
    Some((prefix..old.len() - suffix, new.len() - prefix - suffix))
}

#[derive(Clone)]
enum DiffSource {
    Checkout {
        chat_id: String,
        target: Option<String>,
    },
    Turn {
        chat_id: String,
        assistant_message_id: String,
        target: Option<String>,
    },
}

async fn fetch_page(
    engine: &EngineHandle,
    chat_id: &str,
    target: Option<&str>,
    catalog_revision: &str,
    page_id: &str,
) -> Result<CheckoutDiffPage, jolt_rpc::RpcError> {
    let mut params = serde_json::Map::from_iter([
        ("chatId".into(), chat_id.into()),
        ("catalogRevision".into(), catalog_revision.into()),
        ("pageId".into(), page_id.into()),
    ]);
    if let Some(target) = target {
        params.insert("targetDeviceId".into(), target.into());
    }
    engine
        .client()
        .call_as::<CheckoutDiffPage>(
            methods::GET_CHECKOUT_DIFF_PAGE,
            serde_json::Value::Object(params),
        )
        .await
}

async fn fetch_turn_page(
    engine: &EngineHandle,
    chat_id: &str,
    assistant_message_id: &str,
    target: Option<&str>,
    catalog_revision: &str,
    page_id: &str,
) -> Result<CheckoutDiffPage, jolt_rpc::RpcError> {
    let mut params = serde_json::Map::from_iter([
        ("chatId".into(), chat_id.into()),
        ("assistantMessageId".into(), assistant_message_id.into()),
        ("catalogRevision".into(), catalog_revision.into()),
        ("pageId".into(), page_id.into()),
    ]);
    if let Some(target) = target {
        params.insert("targetDeviceId".into(), target.into());
    }
    engine
        .client()
        .call_as::<CheckoutDiffPage>(
            methods::GET_TURN_DIFF_PAGE,
            serde_json::Value::Object(params),
        )
        .await
}

async fn fetch_source_page(
    engine: &EngineHandle,
    source: &DiffSource,
    catalog_revision: &str,
    page_id: &str,
) -> Result<CheckoutDiffPage, jolt_rpc::RpcError> {
    match source {
        DiffSource::Checkout { chat_id, target } => {
            fetch_page(
                engine,
                chat_id,
                target.as_deref(),
                catalog_revision,
                page_id,
            )
            .await
        }
        DiffSource::Turn {
            chat_id,
            assistant_message_id,
            target,
        } => {
            fetch_turn_page(
                engine,
                chat_id,
                assistant_message_id,
                target.as_deref(),
                catalog_revision,
                page_id,
            )
            .await
        }
    }
}

async fn yield_now() {
    let mut yielded = false;
    futures::future::poll_fn(move |cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await
}

pub struct Changes {
    state: Entity<AppState>,
    enabled: bool,
    watch_key: Option<(String, Option<String>)>,
    watch_task: Option<Task<()>>,
    source: Option<DiffSource>,
    expanded_view: bool,
    error: Option<SharedString>,
    manifest: Option<CheckoutDiffManifest>,
    sequence: u64,
    expanded: HashSet<String>,
    pages: HashMap<String, CachedPage>,
    page_order: VecDeque<String>,
    page_bytes: usize,
    access_clock: u64,
    loading: HashSet<String>,
    page_tasks: HashMap<String, Task<()>>,
    page_errors: HashSet<String>,
    highlights: HashMap<String, HighlightSlot>,
    highlight_order: VecDeque<String>,
    highlight_bytes: usize,
    horizontal_scrolls: HashMap<String, ScrollHandle>,
    file_columns: HashMap<String, usize>,
    rows: Vec<ChangeRow>,
    list: ListState,
    _observe: Subscription,
}

impl Changes {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.sync_watch(cx));
        let list = ListState::new(0, ListAlignment::Top, px(320.0));
        let weak = cx.weak_entity();
        list.set_scroll_handler(move |event: &ListScrollEvent, _window, cx| {
            weak.update(cx, |this: &mut Changes, cx| this.handle_scroll(event, cx))
                .ok();
        });
        Self {
            state,
            enabled: false,
            watch_key: None,
            watch_task: None,
            source: None,
            expanded_view: false,
            error: None,
            manifest: None,
            sequence: 0,
            expanded: HashSet::new(),
            pages: HashMap::new(),
            page_order: VecDeque::new(),
            page_bytes: 0,
            access_clock: 0,
            loading: HashSet::new(),
            page_tasks: HashMap::new(),
            page_errors: HashSet::new(),
            highlights: HashMap::new(),
            highlight_order: VecDeque::new(),
            highlight_bytes: 0,
            horizontal_scrolls: HashMap::new(),
            file_columns: HashMap::new(),
            rows: Vec::new(),
            list,
            _observe: observe,
        }
    }

    pub fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.expanded.clear();
        self.rebuild_rows();
        cx.notify();
    }

    pub fn set_expanded_view(&mut self, expanded: bool, cx: &mut Context<Self>) {
        if self.expanded_view != expanded {
            self.expanded_view = expanded;
            cx.notify();
        }
    }

    pub fn stop_watch(&mut self, cx: &mut Context<Self>) {
        self.enabled = false;
        self.watch_key = None;
        self.watch_task = None;
        self.source = None;
        self.manifest = None;
        self.page_tasks.clear();
        self.loading.clear();
        self.horizontal_scrolls.clear();
        self.rebuild_rows();
        cx.notify();
    }

    pub fn ensure_watch(&mut self, cx: &mut Context<Self>) {
        if !self.enabled && matches!(self.source, Some(DiffSource::Turn { .. })) {
            return;
        }
        self.enabled = true;
        self.sync_watch(cx);
    }

    pub fn show_turn_diff(
        &mut self,
        diff: TurnDiffManifest,
        target: Option<String>,
        file_path: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        self.enabled = false;
        self.watch_key = None;
        self.watch_task = None;
        self.page_tasks.clear();
        self.source = Some(DiffSource::Turn {
            chat_id: diff.chat_id.clone(),
            assistant_message_id: diff.assistant_message_id.clone(),
            target,
        });
        self.error = None;
        self.sequence = 0;
        self.expanded.clear();
        self.horizontal_scrolls.clear();
        self.loading.clear();
        self.page_errors.clear();
        self.pages.clear();
        self.page_order.clear();
        self.page_bytes = 0;
        let selected = file_path
            .and_then(|path| diff.files.iter().find(|file| file.path == path))
            .or_else(|| diff.files.first())
            .map(|file| (file.id.clone(), file.page_ids.first().cloned()));
        self.manifest = Some(CheckoutDiffManifest {
            catalog_revision: diff.catalog_revision,
            checkout_id: format!("turn:{}", diff.assistant_message_id),
            device_id: diff.device_id,
            cwd: diff.cwd,
            vcs: diff.vcs,
            label: Some("Turn changes".into()),
            files: diff.files,
            pages: diff.pages,
            additions: diff.additions,
            deletions: diff.deletions,
            truncated: diff.truncated,
            updated_at: diff.completed_at,
        });
        if let Some((file_id, page_id)) = selected {
            self.expanded.insert(file_id);
            if let Some(page_id) = page_id {
                self.load_page(page_id, cx);
            }
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn handle_scroll(&mut self, _event: &ListScrollEvent, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        cx.defer(move |cx| {
            weak.update(cx, |changes, cx| {
                let top = changes.list.logical_scroll_top().item_ix;
                let Some(ChangeRow {
                    kind: ChangeRowKind::PagePlaceholder { page_id, .. },
                    ..
                }) = changes.rows.get(top)
                else {
                    return;
                };
                let page_id = page_id.clone();
                changes.list.scroll_to(gpui::ListOffset {
                    item_ix: top,
                    offset_in_item: px(0.0),
                });
                changes.load_page(page_id, cx);
                cx.notify();
            })
            .ok();
        });
    }

    fn desired_watch(&self, cx: &App) -> Option<(String, Option<String>)> {
        let state = self.state.read(cx);
        let chat = state.selected_chat_row()?;
        let target = (state.local_device_id.as_deref() != Some(chat.device_id.as_str()))
            .then(|| chat.device_id.clone());
        Some((chat.id.clone(), target))
    }

    fn sync_watch(&mut self, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }
        let Some(key) = self.desired_watch(cx) else {
            self.watch_key = None;
            self.watch_task = None;
            self.manifest = None;
            self.rebuild_rows();
            return;
        };
        if self.watch_key.as_ref() == Some(&key) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.watch_key = Some(key.clone());
        self.source = Some(DiffSource::Checkout {
            chat_id: key.0.clone(),
            target: key.1.clone(),
        });
        self.manifest = None;
        self.sequence = 0;
        self.expanded.clear();
        self.horizontal_scrolls.clear();
        self.loading.clear();
        self.page_errors.clear();
        self.rebuild_rows();
        self.watch_task = Some(Self::spawn_watch(engine, key.0, key.1, cx));
    }

    fn spawn_watch(
        engine: EngineHandle,
        chat_id: String,
        target: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let mut params = serde_json::Map::from_iter([(
                    "chatId".into(),
                    serde_json::Value::String(chat_id.clone()),
                )]);
                if let Some(target) = &target {
                    params.insert("targetDeviceId".into(), target.clone().into());
                }
                match engine
                    .client()
                    .subscribe(
                        methods::WATCH_CHECKOUT_DIFF_V2,
                        serde_json::Value::Object(params),
                    )
                    .await
                {
                    Ok(mut receiver) => {
                        while let Some(value) = receiver.recv().await {
                            let Ok(frame) = serde_json::from_value::<CheckoutDiffWatchFrame>(value)
                            else {
                                tracing::warn!("changes: malformed V2 diff frame");
                                continue;
                            };
                            let result = this.update(cx, |changes, cx| {
                                changes.error = None;
                                if changes.apply_frame(frame).is_err() {
                                    changes.error =
                                        Some("Diff stream desynchronized — retrying".into());
                                    return false;
                                }
                                cx.notify();
                                true
                            });
                            if !matches!(result, Ok(true)) {
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        if this
                            .update(cx, |changes, cx| {
                                changes.error =
                                    Some(format!("Diff watch unavailable: {error}").into());
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        })
    }

    fn apply_frame(&mut self, frame: CheckoutDiffWatchFrame) -> Result<(), ()> {
        let (sequence, manifest, bootstrap_pages) = match frame {
            CheckoutDiffWatchFrame::Bootstrap { bootstrap } => {
                (bootstrap.sequence, bootstrap.manifest, bootstrap.pages)
            }
            CheckoutDiffWatchFrame::Manifest { sequence, manifest } => {
                if sequence != self.sequence.wrapping_add(1) {
                    return Err(());
                }
                (sequence, manifest, Vec::new())
            }
        };
        let referenced: HashSet<&str> =
            manifest.pages.iter().map(|page| page.id.as_str()).collect();
        self.pages.retain(|id, page| {
            let keep = referenced.contains(id.as_str());
            if !keep {
                self.page_bytes = self.page_bytes.saturating_sub(page.bytes);
            }
            keep
        });
        self.page_order
            .retain(|id| referenced.contains(id.as_str()));
        let dropped_highlight_bytes: usize = self
            .highlights
            .iter()
            .filter(|(id, _)| !referenced.contains(id.as_str()))
            .map(|(_, slot)| slot.bytes)
            .sum();
        self.highlight_bytes = self.highlight_bytes.saturating_sub(dropped_highlight_bytes);
        self.highlights
            .retain(|id, _| referenced.contains(id.as_str()));
        self.highlight_order
            .retain(|id| referenced.contains(id.as_str()));
        self.loading.retain(|id| referenced.contains(id.as_str()));
        self.page_tasks
            .retain(|id, _| referenced.contains(id.as_str()));
        self.page_errors
            .retain(|id| referenced.contains(id.as_str()));
        self.expanded
            .retain(|id| manifest.files.iter().any(|file| &file.id == id));
        self.horizontal_scrolls
            .retain(|id, _| manifest.files.iter().any(|file| &file.id == id));
        self.sequence = sequence;
        self.manifest = Some(manifest);
        for page in bootstrap_pages {
            self.insert_page(page);
        }
        self.rebuild_rows();
        Ok(())
    }

    fn toggle_file(&mut self, file: &DiffFileDescriptor, cx: &mut Context<Self>) {
        if self.expanded.remove(&file.id) {
            for page_id in &file.page_ids {
                self.loading.remove(page_id);
                self.page_tasks.remove(page_id);
            }
        } else {
            self.expanded.insert(file.id.clone());
            if let Some(page) = file.page_ids.first() {
                self.load_page(page.clone(), cx);
            }
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn load_page(&mut self, page_id: String, cx: &mut Context<Self>) {
        if self.pages.contains_key(&page_id) || !self.loading.insert(page_id.clone()) {
            return;
        }
        self.page_errors.remove(&page_id);
        let Some(source) = self.source.clone() else {
            self.loading.remove(&page_id);
            return;
        };
        let Some(manifest) = self.manifest.as_ref() else {
            self.loading.remove(&page_id);
            return;
        };
        let catalog_revision = manifest.catalog_revision.clone();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.loading.remove(&page_id);
            return;
        };
        let task_id = page_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let mut result = fetch_source_page(&engine, &source, &catalog_revision, &page_id).await;
            for delay in [250u64, 1_000] {
                if result.is_ok() {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(delay))
                    .await;
                result = fetch_source_page(&engine, &source, &catalog_revision, &page_id).await;
            }
            let parsed = match result {
                Ok(page) => {
                    let patch = page.patch.clone();
                    let files = cx
                        .background_executor()
                        .spawn(async move { parse_patch(&patch) })
                        .await;
                    Ok((page, files.into_iter().next()))
                }
                Err(error) => Err(error),
            };
            this.update(cx, |changes, cx| {
                changes.loading.remove(&page_id);
                if changes
                    .manifest
                    .as_ref()
                    .is_none_or(|manifest| manifest.catalog_revision != catalog_revision)
                {
                    return;
                }
                match parsed {
                    Ok((page, Some(file))) => {
                        changes.page_errors.remove(&page_id);
                        changes.insert_parsed_page(page, file);
                    }
                    Ok((_, None)) => {
                        changes.page_errors.insert(page_id.clone());
                    }
                    Err(error) => {
                        tracing::warn!(%page_id, %error, "diff page load failed");
                        changes.page_errors.insert(page_id.clone());
                    }
                }
                changes.rebuild_rows();
                cx.notify();
            })
            .ok();
        });
        self.page_tasks.insert(task_id, task);
    }

    fn insert_page(&mut self, page: CheckoutDiffPage) {
        if let Some(file) = parse_patch(&page.patch).into_iter().next() {
            self.insert_parsed_page(page, file);
        }
    }

    fn insert_parsed_page(&mut self, page: CheckoutDiffPage, file: FileDiff) {
        self.access_clock = self.access_clock.wrapping_add(1);
        let bytes = page.patch.len().saturating_mul(2);
        if let Some(previous) = self.pages.insert(
            page.id.clone(),
            CachedPage {
                file: Arc::new(file),
                bytes,
                access: self.access_clock,
            },
        ) {
            self.page_bytes = self.page_bytes.saturating_sub(previous.bytes);
        }
        self.page_bytes += bytes;
        self.page_order.retain(|id| id != &page.id);
        self.page_order.push_back(page.id.clone());
        while self.page_bytes > PAGE_CACHE_BYTES && self.pages.len() > 1 {
            let Some(oldest) = self.page_order.pop_front() else {
                break;
            };
            if oldest == page.id {
                self.page_order.push_back(oldest);
                break;
            }
            if let Some(removed) = self.pages.remove(&oldest) {
                self.page_bytes = self.page_bytes.saturating_sub(removed.bytes);
                if let Some(slot) = self.highlights.remove(&oldest) {
                    self.highlight_bytes = self.highlight_bytes.saturating_sub(slot.bytes);
                }
                self.highlight_order.retain(|id| id != &oldest);
            }
        }
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        let mut file_columns = HashMap::new();
        if let Some(manifest) = &self.manifest {
            for (file_index, file) in manifest.files.iter().enumerate() {
                self.horizontal_scrolls.entry(file.id.clone()).or_default();
                rows.push(ChangeRow {
                    id: format!("file:{}", file.id),
                    version: hash64(&[&file.id, &manifest.catalog_revision]),
                    kind: ChangeRowKind::FileHeader { file: file_index },
                });
                if !self.expanded.contains(&file.id) {
                    continue;
                }
                if file.page_ids.is_empty() {
                    rows.push(ChangeRow {
                        id: format!("file-unavailable:{}", file.id),
                        version: hash64(&[&file.id, &format!("{:?}", file.completeness)]),
                        kind: ChangeRowKind::Unavailable { file: file_index },
                    });
                }
                for page_id in &file.page_ids {
                    let Some(page) = self.pages.get(page_id) else {
                        rows.push(ChangeRow {
                            id: format!("page-placeholder:{page_id}"),
                            version: u64::from(self.loading.contains(page_id))
                                | (u64::from(self.page_errors.contains(page_id)) << 1),
                            kind: ChangeRowKind::PagePlaceholder {
                                file: file_index,
                                page_id: page_id.clone(),
                            },
                        });
                        continue;
                    };
                    file_columns
                        .entry(file.id.clone())
                        .and_modify(|columns: &mut usize| {
                            *columns = (*columns).max(file_display_columns(&page.file));
                        })
                        .or_insert_with(|| file_display_columns(&page.file));
                    for (notice, _) in file_notices(&page.file).iter().enumerate() {
                        rows.push(ChangeRow {
                            id: format!("{page_id}:notice:{notice}"),
                            version: page.access,
                            kind: ChangeRowKind::Notice {
                                file: file_index,
                                page_id: page_id.clone(),
                                notice,
                            },
                        });
                    }
                    let mut flat_line = 0;
                    for (hunk, value) in page.file.hunks.iter().enumerate() {
                        rows.push(ChangeRow {
                            id: format!("{page_id}:hunk:{hunk}"),
                            version: page.access,
                            kind: ChangeRowKind::HunkHeader {
                                file: file_index,
                                page_id: page_id.clone(),
                                hunk,
                            },
                        });
                        for line in 0..value.lines.len() {
                            rows.push(ChangeRow {
                                id: format!("{page_id}:hunk:{hunk}:line:{line}"),
                                version: page.access,
                                kind: ChangeRowKind::Line {
                                    file: file_index,
                                    page_id: page_id.clone(),
                                    hunk,
                                    line,
                                    flat_line,
                                },
                            });
                            flat_line += 1;
                        }
                    }
                }
            }
        }
        if let Some((range, count)) = row_splice(&self.rows, &rows) {
            self.list.splice(range, count);
        }
        self.file_columns = file_columns;
        self.rows = rows;
    }

    fn trim_highlights(&mut self, keep: &str) {
        while self.highlight_bytes > HIGHLIGHT_CACHE_BYTES && self.highlights.len() > 1 {
            let Some(oldest) = self.highlight_order.pop_front() else {
                break;
            };
            if oldest == keep {
                self.highlight_order.push_back(oldest);
                break;
            }
            if let Some(slot) = self.highlights.remove(&oldest) {
                self.highlight_bytes = self.highlight_bytes.saturating_sub(slot.bytes);
            }
        }
    }

    fn request_highlight(&mut self, page_id: &str, cx: &mut Context<Self>) {
        if self.highlights.contains_key(page_id) {
            return;
        }
        let Some(page) = self.pages.get(page_id) else {
            return;
        };
        let Some(language) = lang_for_path(&page.file.path) else {
            return;
        };
        let texts: Vec<_> = page
            .file
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter().map(|line| (line.kind, line.text.clone())))
            .collect();
        let id = page_id.to_string();
        let task = cx.spawn(async move |this, cx| {
            let lines = cx
                .background_executor()
                .spawn(async move {
                    let mut output = Vec::with_capacity(texts.len());
                    let mut old_carry = LineCarry::None;
                    let mut new_carry = LineCarry::None;
                    for (index, (kind, text)) in texts.iter().enumerate() {
                        output.push(match kind {
                            LineKind::Meta => Vec::new(),
                            LineKind::Del => {
                                let (tokens, carry) = tokenize_line(language, text, old_carry);
                                old_carry = carry;
                                tokens
                            }
                            LineKind::Add => {
                                let (tokens, carry) = tokenize_line(language, text, new_carry);
                                new_carry = carry;
                                tokens
                            }
                            LineKind::Context => {
                                let (tokens, old) = tokenize_line(language, text, old_carry);
                                old_carry = old;
                                new_carry = tokenize_line(language, text, new_carry).1;
                                tokens
                            }
                        });
                        if index % 128 == 127 {
                            yield_now().await;
                        }
                    }
                    output
                })
                .await;
            this.update(cx, |changes, cx| {
                let bytes = lines
                    .iter()
                    .map(|line| line.len().saturating_mul(std::mem::size_of::<Token>()))
                    .sum();
                if let Some(slot) = changes.highlights.get_mut(&id) {
                    changes.highlight_bytes = changes.highlight_bytes.saturating_sub(slot.bytes);
                    slot.bytes = bytes;
                    slot.lines = Some(Arc::new(lines));
                    changes.highlight_bytes += bytes;
                    changes.highlight_order.retain(|candidate| candidate != &id);
                    changes.highlight_order.push_back(id.clone());
                    changes.trim_highlights(&id);
                    cx.notify();
                }
            })
            .ok();
        });
        self.highlights.insert(
            page_id.to_string(),
            HighlightSlot {
                lines: None,
                bytes: 0,
                _task: Some(task),
            },
        );
    }

    fn file_content_width(&self, file_index: usize, cx: &App) -> f32 {
        let Some(file) = self
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.files.get(file_index))
        else {
            return 0.0;
        };
        let columns = self.file_columns.get(&file.id).copied().unwrap_or(0);
        ACCENT_BAR_WIDTH
            + 2.0 * GUTTER_WIDTH
            + MARKER_WIDTH
            + 2.0 * Theme::SPACE_LG
            + columns as f32
                * f32::from(Theme::of(cx).font_sizes.code)
                * MONOSPACE_GLYPH_WIDTH_RATIO
    }

    fn scroll_file_row(
        &self,
        row_id: &str,
        file_index: usize,
        content: AnyElement,
        cx: &App,
    ) -> AnyElement {
        let Some(file) = self
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.files.get(file_index))
        else {
            return content;
        };
        let Some(scroll) = self.horizontal_scrolls.get(&file.id) else {
            return content;
        };
        let mut scroller = div()
            .id(SharedString::from(format!("diff-scroll:{row_id}")))
            .w_full()
            .overflow_x_scroll()
            .track_scroll(scroll)
            .child(
                div()
                    .w_full()
                    .min_w(px(self.file_content_width(file_index, cx)))
                    .child(content),
            );
        // A one-axis GPUI scroller otherwise maps vertical wheel deltas onto
        // its horizontal axis. Preserve vertical movement for the virtual list.
        scroller.style().restrict_scroll_to_axis = Some(true);
        scroller.into_any_element()
    }

    fn render_row(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = self.rows.get(index).cloned() else {
            return gpui::Empty.into_any_element();
        };
        let row_id = row.id.clone();
        match row.kind {
            ChangeRowKind::FileHeader { file } => self.render_file_header(index, file, cx),
            ChangeRowKind::PagePlaceholder { file, page_id } => {
                self.render_placeholder(file, page_id, cx)
            }
            ChangeRowKind::Unavailable { file } => {
                let theme = Theme::of(cx);
                let label = self
                    .manifest
                    .as_ref()
                    .and_then(|manifest| manifest.files.get(file))
                    .map_or("Diff contents unavailable", |file| {
                        match file.completeness {
                            DiffCompleteness::Binary => "Binary file — contents not shown",
                            DiffCompleteness::SnapshotTruncated => {
                                "Not included in the partial snapshot"
                            }
                            DiffCompleteness::OversizedLine => "Oversized diff contents omitted",
                            DiffCompleteness::Complete => "No textual changes",
                        }
                    });
                div()
                    .h(px(NOTICE_HEIGHT))
                    .flex()
                    .items_center()
                    .px(px(Theme::SPACE_LG))
                    .text_size(px(11.0))
                    .text_color(theme.text_faint)
                    .child(label)
                    .into_any_element()
            }
            ChangeRowKind::Notice {
                file,
                page_id,
                notice,
            } => {
                let theme = Theme::of(cx);
                let text = self
                    .pages
                    .get(&page_id)
                    .and_then(|page| file_notices(&page.file).get(notice).cloned())
                    .unwrap_or_default();
                let content = div()
                    .h(px(NOTICE_HEIGHT))
                    .flex()
                    .items_center()
                    .px(px(Theme::SPACE_LG))
                    .text_size(px(11.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(text))
                    .into_any_element();
                self.scroll_file_row(&row_id, file, content, cx)
            }
            ChangeRowKind::HunkHeader {
                file,
                page_id,
                hunk,
            } => {
                let theme = Theme::of(cx);
                let header = self
                    .pages
                    .get(&page_id)
                    .and_then(|page| page.file.hunks.get(hunk))
                    .map(|hunk| hunk.header.clone())
                    .unwrap_or_default();
                let content = div()
                    .h(px(HUNK_HEADER_HEIGHT))
                    .flex()
                    .items_center()
                    .px(px(Theme::SPACE_LG))
                    .bg(theme.diff_hunk_bg)
                    .font_family(theme.font_mono.clone())
                    .text_size(px(11.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(header))
                    .into_any_element();
                self.scroll_file_row(&row_id, file, content, cx)
            }
            ChangeRowKind::Line {
                file,
                page_id,
                hunk,
                line,
                flat_line,
            } => {
                self.request_highlight(&page_id, cx);
                let content = self.render_line(&page_id, hunk, line, flat_line, cx);
                self.scroll_file_row(&row_id, file, content, cx)
            }
        }
    }

    fn render_file_header(
        &mut self,
        index: usize,
        file_index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(file) = self
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.files.get(file_index))
            .cloned()
        else {
            return gpui::Empty.into_any_element();
        };
        let expanded = self.expanded.contains(&file.id);
        let click_file = file.clone();
        div()
            .id(SharedString::from(format!("file-hdr-{index}")))
            .w_full()
            .h(px(FILE_HEADER_HEIGHT))
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(Theme::SPACE_MD))
            .border_b_1()
            .border_color(crate::theme::hairline(0.04))
            .bg(crate::theme::ink(0.025))
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::ink(0.05)))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_file(&click_file, cx)))
            .child(
                crate::icons::icon(if expanded {
                    crate::icons::ALT_ARROW_DOWN
                } else {
                    crate::icons::ALT_ARROW_RIGHT
                })
                .size(px(13.0))
                .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(12.0))
                    .text_color(theme.text_dim)
                    .child(SharedString::from(file.path.clone())),
            )
            .when(file.binary, |element| {
                element.child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme.text_faint)
                        .child("BIN"),
                )
            })
            .child(
                div()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(11.0))
                    .text_color(theme.diff_add)
                    .child(SharedString::from(format!("+{}", file.additions))),
            )
            .child(
                div()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(11.0))
                    .text_color(theme.diff_del)
                    .child(SharedString::from(format!("−{}", file.deletions))),
            )
            .into_any_element()
    }

    fn render_placeholder(
        &mut self,
        file_index: usize,
        page_id: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let descriptor = self
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.pages.iter().find(|page| page.id == page_id));
        let height = descriptor.map_or(80.0, |page| {
            (page.notice_count as f32 * NOTICE_HEIGHT
                + page.hunk_count as f32 * HUNK_HEADER_HEIGHT
                + page.line_count as f32 * Theme::of(cx).font_sizes.diff_line_height())
            .clamp(44.0, 24_000.0)
        });
        let failed = self.page_errors.contains(&page_id);
        if !failed && !self.loading.contains(&page_id) {
            let requested = page_id.clone();
            let weak = cx.weak_entity();
            cx.defer(move |cx| {
                weak.update(cx, |changes, cx| changes.load_page(requested, cx))
                    .ok();
            });
        }
        let file_unavailable = self
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.files.get(file_index))
            .is_some_and(|file| file.completeness == DiffCompleteness::SnapshotTruncated);
        let label = if failed {
            "Couldn’t load this diff page · Retry"
        } else if file_unavailable {
            "Partial snapshot"
        } else {
            "Loading changes…"
        };
        let state = self.state.clone();
        let weak = cx.weak_entity();
        div()
            .id(SharedString::from(format!("diff-page:{page_id}")))
            .h(px(height))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(11.0))
            .text_color(Theme::of(cx).text_muted)
            .when(failed, |element| {
                element.cursor_pointer().on_click(move |_, _, cx| {
                    let requested = page_id.clone();
                    weak.update(cx, |changes, cx| changes.load_page(requested, cx))
                        .ok();
                    state.update(cx, |_, cx| cx.notify());
                })
            })
            .child(label)
            .into_any_element()
    }

    fn render_line(
        &self,
        page_id: &str,
        hunk: usize,
        line: usize,
        flat_line: usize,
        cx: &App,
    ) -> AnyElement {
        let theme = Theme::of(cx);
        let Some(value) = self
            .pages
            .get(page_id)
            .and_then(|page| page.file.hunks.get(hunk))
            .and_then(|hunk| hunk.lines.get(line))
        else {
            return gpui::Empty.into_any_element();
        };
        if value.kind == LineKind::Meta {
            return div()
                .h(px(theme.font_sizes.diff_line_height()))
                .flex()
                .items_center()
                .pl(px(ACCENT_BAR_WIDTH
                    + 2.0 * GUTTER_WIDTH
                    + MARKER_WIDTH
                    + 12.0))
                .text_size(px(10.5))
                .text_color(theme.text_faint)
                .italic()
                .child(SharedString::from(value.text.clone()))
                .into_any_element();
        }
        let (marker, color, background) = match value.kind {
            LineKind::Add => ("+", theme.diff_add, Some(theme.diff_add.opacity(0.055))),
            LineKind::Del => ("−", theme.diff_del, Some(theme.diff_del.opacity(0.055))),
            _ => ("·", theme.text_faint.opacity(0.5), None),
        };
        let tokens = self
            .highlights
            .get(page_id)
            .and_then(|slot| slot.lines.as_ref())
            .and_then(|lines| lines.get(flat_line))
            .map_or(&[][..], Vec::as_slice);
        let mono = font(theme.font_mono.clone());
        let runs = render::runs_with_palette(
            &value.text,
            tokens,
            &mono,
            theme.text.opacity(0.92),
            |class| render::token_color(class, theme),
        );
        let gutter = |number: Option<u32>| {
            div()
                .w(px(GUTTER_WIDTH))
                .flex()
                .justify_end()
                .pr(px(8.0))
                .font_family(theme.font_mono.clone())
                .text_size(px(11.0))
                .text_color(theme.text_faint.opacity(0.8))
                .child(SharedString::from(
                    number.map(|number| number.to_string()).unwrap_or_default(),
                ))
        };
        div()
            .h(px(theme.font_sizes.diff_line_height()))
            .flex()
            .items_center()
            .when_some(background, |element, background| element.bg(background))
            .child(div().w(px(ACCENT_BAR_WIDTH)).h_full().when(
                value.kind == LineKind::Add || value.kind == LineKind::Del,
                |element| element.bg(color.opacity(0.55)),
            ))
            .child(gutter(value.old_no))
            .child(gutter(value.new_no))
            .child(
                div()
                    .w(px(MARKER_WIDTH))
                    .flex()
                    .justify_center()
                    .font_family(theme.font_mono.clone())
                    .text_color(color)
                    .child(marker),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .pl(px(12.0))
                    .font_family(theme.font_mono.clone())
                    .text_size(px(f32::from(theme.font_sizes.code)))
                    .whitespace_nowrap()
                    .child(gpui::StyledText::new(value.text.clone()).with_runs(runs)),
            )
            .into_any_element()
    }
}

impl Render for Changes {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let error = self.error.clone();
        let showing_turn = matches!(self.source, Some(DiffSource::Turn { .. }));
        let content: AnyElement = match self.manifest.as_ref() {
            None if self.state.read(cx).selected_chat_row().is_some() => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(crate::loaders::activity_orb(
                    "changes-preparing",
                    &theme,
                    16.0,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            None => empty_state("No uncommitted changes", &theme),
            Some(manifest) if manifest.files.is_empty() => empty_state(
                if manifest.vcs == jolt_proto::VcsKind::Jujutsu {
                    "Working copy is clean"
                } else {
                    "No uncommitted changes"
                },
                &theme,
            ),
            Some(manifest) => {
                let heading = if showing_turn {
                    format!(
                        "{} file{} changed",
                        manifest.files.len(),
                        if manifest.files.len() == 1 { "" } else { "s" }
                    )
                } else if manifest.vcs == jolt_proto::VcsKind::Jujutsu {
                    format!(
                        "{} · {} files",
                        manifest.label.as_deref().unwrap_or("Working copy"),
                        manifest.files.len()
                    )
                } else {
                    format!("{} Uncommitted changes", manifest.files.len())
                };
                let additions = manifest.additions;
                let deletions = manifest.deletions;
                let truncated = manifest.truncated;
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(36.0))
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .px(px(Theme::SPACE_LG))
                            .border_b_1()
                            .border_color(crate::theme::hairline(0.06))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(12.0))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(heading)),
                            )
                            .child(
                                div()
                                    .font_family(theme.font_mono.clone())
                                    .text_size(px(11.0))
                                    .text_color(theme.diff_add)
                                    .child(SharedString::from(format!("+{additions}"))),
                            )
                            .child(
                                div()
                                    .font_family(theme.font_mono.clone())
                                    .text_size(px(11.0))
                                    .text_color(theme.diff_del)
                                    .child(SharedString::from(format!("−{deletions}"))),
                            )
                            .when(truncated, |element| {
                                element.child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(theme.warning)
                                        .child("Partial snapshot"),
                                )
                            })
                            .child(
                                div()
                                    .id("toggle-expanded-diff")
                                    .size(px(26.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .text_color(theme.text_muted)
                                    .hover(|style| style.bg(crate::theme::wash(0.08)))
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(ChangesEvent::ToggleExpanded);
                                    }))
                                    .child(
                                        crate::icons::icon(if self.expanded_view {
                                            crate::icons::RESTORE
                                        } else {
                                            crate::icons::MAXIMIZE
                                        })
                                        .size(px(15.0))
                                        .text_color(theme.text_muted),
                                    ),
                            ),
                    )
                    .child(
                        list(self.list.clone(), cx.processor(Self::render_row))
                            .flex_1()
                            .with_sizing_behavior(gpui::ListSizingBehavior::Auto),
                    )
                    .into_any_element()
            }
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .when_some(error, |element, message| {
                element.child(
                    div()
                        .px(px(Theme::SPACE_MD))
                        .py(px(4.0))
                        .text_size(px(11.0))
                        .text_color(theme.warning)
                        .child(message),
                )
            })
            .child(content)
    }
}

impl EventEmitter<ChangesEvent> for Changes {}

fn empty_state(label: &'static str, theme: &Theme) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(12.0))
        .text_color(theme.text_faint)
        .child(label)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_columns_expands_tabs_to_four_column_stops() {
        assert_eq!(display_columns("a\tb"), 5);
        assert_eq!(display_columns("abcd\te"), 9);
    }

    #[test]
    fn parses_basic_patch() {
        let files = parse_patch(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n",
        );
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].additions, 1);
        assert_eq!(files[0].deletions, 1);
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
}
