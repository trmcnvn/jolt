//! The conversation view: virtualized transcript with block-granularity rows,
//! stick-to-bottom, tool-group folding, and streaming markdown.
//!
//! Row model (docs/using-jolt.md):
//! - one row per BLOCK: user message = one bubble row; assistant messages split
//!   into one row per markdown top-level block, plus consecutive-tool groups,
//!   input/error chips, and durable harness-switch boundaries;
//! - stable row ids `{msgId}#{partId}.{blockIx}` / `{msgId}#g{groupIx}` — LIVE
//!   (streaming) entries split per block exactly like completed ones (the list
//!   virtualizes them, so a fading live reply re-renders only its visible tail
//!   each frame — flat cost in the reply length); on completion each block row
//!   keeps its id, so row identity is continuous and nothing flickers;
//! - rows are cached per entry keyed by a content fingerprint — only changed
//!   messages rebuild (the anti-"streaming stutter" trick);
//! - row-set changes diff by (id, version) into one minimal `splice`.
//!
//! Stick-to-bottom is a velocity spring (mugen §1e, the same shape as
//! stackblitz's use-stick-to-bottom): while pinned, a per-frame stepper glides
//! the viewport toward the list end with a feed-forward term tracking the
//! smoothed target growth, so 120ms doc commits read as a continuous glide
//! instead of per-commit snaps. The pin breaks only on user input (the list's
//! scroll handler fires exclusively from its wheel/touch path) and re-engages
//! inside the 70px band; own-send re-engages with the same glide.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, ClipboardItem, Context, Entity, EventEmitter, ListAlignment, ListScrollEvent,
    ListState, ObjectFit, Pixels, SharedString, StyledImage as _, Subscription, Task, Window, div,
    img, list, prelude::*, px,
};

use jolt_proto::{HarnessId, ToolCall, TurnDiffManifest};
use jolt_session_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};

use crate::markdown::LinkTarget;
use crate::markdown::highlight::{Lang, LineCarry, Token, lang_for_tag, tokenize_line};
use crate::markdown::parser::{Block, BlockTree, IncrementalParser, parse_full};
use crate::markdown::render::{self, RenderCache, RenderOptions};
use crate::markdown::veil::RowVeil;
use crate::motion::{self, AnimationExt as _, RESIZE};
use crate::state::AppState;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Constants (mugen ports)
// ---------------------------------------------------------------------------

/// Re-engage the bottom pin when the user returns within this many px of the end.
pub const STICK_THRESHOLD_PX: f32 = 70.0;
/// List overdraw beyond the viewport.
/// Two typical viewports of leading overdraw hide historical page fetches
/// during ordinary scrolling; fast flings still clamp at the cold boundary.
pub const OVERDRAW_PX: f32 = 1_200.0;
/// Show the scroll-to-bottom button beyond this distance from the end.
pub const SCROLL_BUTTON_THRESHOLD_PX: f32 = 320.0;
/// Vertical gap opening a new turn (new message entry).
pub const GAP_TURN: f32 = 14.0;
/// Vertical gap between blocks within a turn.
pub const GAP_BLOCK: f32 = 8.0;
/// Transcript column max width (jolt 46rem).
pub const MAX_CONTENT_WIDTH: f32 = 736.0;
/// Tool chip row height / gap — analytic, so fold heights need no measurement.
/// A row is the guide rail plus a 30px chip card centered in a 38px row. Rows
/// stack without gaps so the rail reads continuously.
pub const CHIP_HEIGHT: f32 = 38.0;
pub const CHIP_GAP: f32 = 0.0;
pub const CHIP_CARD_HEIGHT: f32 = 30.0;
const CHIPS_TOP_PAD: f32 = 2.0;
/// How long a user fold toggle keeps its height tween armed: the RESIZE
/// spec's 200ms plus margin. Past this the fold renders statically — an armed
/// tween replays on remount, i.e. on every scroll-back-into-view.
const FOLD_TWEEN_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);
/// User-bubble attachment thumbnails: 112×80 thumbnails in a fixed-height
/// strip so load-state changes never shift the virtualizer.
pub const ATT_THUMB_W: f32 = 112.0;
pub const ATT_THUMB_H: f32 = 80.0;
pub const ATT_STRIP_H: f32 = ATT_THUMB_H + 10.0;
const CHANGE_TREE_INDENT: f32 = 16.0;
const CHANGE_TREE_ROW_HEIGHT: f32 = 28.0;

// ---------------------------------------------------------------------------
// Stick-to-bottom spring (mugen §1e — same constants as its DEFAULT_SPRING,
// which follows the shape of stackblitz/use-stick-to-bottom)
// ---------------------------------------------------------------------------

/// Retains velocity frame-to-frame (higher = more glide).
pub const SPRING_DAMPING: f32 = 0.7;
/// Pull toward the target (higher = snappier).
pub const SPRING_STIFFNESS: f32 = 0.05;
/// Inertia (higher = slower to start/stop).
pub const SPRING_MASS: f32 = 1.25;
/// Reference frame for the fixed-timestep integration (60fps).
pub const SPRING_FRAME_MS: f32 = 1000.0 / 60.0;
/// Cap on simulated frames per tick — a hitch catches up instead of teleporting.
pub const SPRING_MAX_CATCHUP_FRAMES: f32 = 8.0;
/// EMA rate for the feed-forward target-growth estimate.
pub const SPRING_GROWTH_EMA: f32 = 0.12;
/// While streaming, chase up to this many px above the true bottom (keeps the
/// growing tail visible instead of hugging a moving edge).
pub const SPRING_CHASE_MAX_LEAD: f32 = 32.0;
/// Treat as exactly pinned within this distance of the bottom.
pub const AT_BOTTOM_PX: f32 = 2.0;
/// Keep the spring loop warm this long after landing, so a streaming pause
/// resumes at cruise instead of re-accelerating from zero.
pub const SPRING_SETTLE_GRACE_MS: u64 = 500;
/// Teleport when farther than this many viewports from the end; glide the rest.
pub const GLIDE_MAX_VIEWPORTS: f32 = 2.5;

/// Pure stick-to-bottom spring stepper — the mugen `tick()` integration:
/// velocity relaxes toward `(damping·v + stiffness·diff)/mass` per 60fps
/// sub-frame, position advances by `v + target_vel` where `target_vel` is a
/// feed-forward EMA of target growth px/frame, and the chase point sits up to
/// [`SPRING_CHASE_MAX_LEAD`] px above the true bottom proportional to growth.
#[derive(Debug, Clone, Copy)]
pub struct StickSpring {
    /// Spring velocity, px per 60fps frame.
    velocity: f32,
    /// Feed-forward: smoothed target growth, px per 60fps frame.
    target_vel: f32,
    /// Target observed at the previous tick (`None` = fresh/parked).
    last_target: Option<f32>,
}

impl Default for StickSpring {
    fn default() -> Self {
        Self::new()
    }
}

impl StickSpring {
    pub fn new() -> Self {
        Self {
            velocity: 0.0,
            target_vel: 0.0,
            last_target: None,
        }
    }

    /// Park the spring (drops all state; the next tick starts cold).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Residual motion below mugen's settle thresholds (`v < .05 && targetVel
    /// < .05`)?
    pub fn is_idle(&self) -> bool {
        self.velocity < 0.05 && self.target_vel < 0.05
    }

    #[cfg(test)]
    pub(crate) fn target_vel(&self) -> f32 {
        self.target_vel
    }

    /// Advance one tick. `pos`/`target` are scroll offsets in px (larger =
    /// closer to the bottom); `frames` is elapsed time in 60fps frames
    /// (clamped by the caller to [`SPRING_MAX_CATCHUP_FRAMES`]). Returns the
    /// new position: never overshoots `target`, monotone while approaching,
    /// and snaps exactly once within 0.5px.
    pub fn step(&mut self, mut pos: f32, target: f32, mut frames: f32) -> f32 {
        let grew = self.last_target.map_or(0.0, |last| target - last);
        self.last_target = Some(target);
        if grew < -1.0 {
            // Target shrank (row collapse/removal) — growth estimate is stale.
            self.target_vel = 0.0;
        } else {
            let observed = grew.max(0.0) / frames.max(0.25);
            self.target_vel += SPRING_GROWTH_EMA * (observed - self.target_vel);
        }
        let chase = target - (self.target_vel * 9.0).min(SPRING_CHASE_MAX_LEAD);
        let mut v = self.velocity;
        while frames > 0.0 {
            let h = frames.min(1.0);
            frames -= h;
            let diff = (chase - pos).max(0.0);
            v += h * ((SPRING_DAMPING * v + SPRING_STIFFNESS * diff) / SPRING_MASS - v);
            pos = (pos + (v + self.target_vel) * h).min(target);
        }
        self.velocity = v;
        if target - pos <= 0.5 { target } else { pos }
    }
}

mod projection;

use projection::*;
pub use projection::{
    ParseOutcome, Row, RowKind, ToolItem, chips_height, diff_rows, flavour_seed, flavour_word,
    format_elapsed, format_timestamp, parse_for_row, rows_for_entry, single_line, tool_activity,
    tool_chip_content, tool_group_summary, top_gap_for,
};

// ---------------------------------------------------------------------------
// Highlight store (background, time-sliced, paint-only)
// ---------------------------------------------------------------------------

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

struct HighlightEntry {
    code_len: usize,
    lines: Option<Arc<Vec<Vec<Token>>>>,
    _task: Option<Task<()>>,
}

/// Cache of tokenized code blocks keyed by `(row id, block ix)`. Tokenization
/// runs on the background executor, time-sliced; results apply as paint-only
/// run colors when they land.
#[derive(Default)]
struct HighlightStore {
    entries: HashMap<(SharedString, usize), HighlightEntry>,
}

impl HighlightStore {
    /// Current tokens if ready; kicks a background tokenize when stale/missing.
    fn request(
        &mut self,
        row_id: SharedString,
        block_ix: usize,
        lang: Lang,
        code: &str,
        cx: &mut Context<Transcript>,
    ) -> Option<Arc<Vec<Vec<Token>>>> {
        let key = (row_id.clone(), block_ix);
        if let Some(entry) = self.entries.get(&key)
            && entry.code_len == code.len()
        {
            return entry.lines.clone();
        }
        // Keep stale lines visible while the fresh parse runs (paint-only, so a
        // briefly stale color is harmless; lengths shift at most on the tail).
        let stale = self.entries.get(&key).and_then(|e| e.lines.clone());
        let code = code.to_string();
        let code_len = code.len();
        let task = cx.spawn(async move |this, cx| {
            let lines = cx
                .background_executor()
                .spawn(async move {
                    let mut carry = LineCarry::None;
                    let mut out = Vec::new();
                    for (ix, line) in code.split('\n').enumerate() {
                        let (tokens, next) = tokenize_line(lang, line, carry);
                        carry = next;
                        out.push(tokens);
                        if ix % 128 == 127 {
                            yield_now().await;
                        }
                    }
                    out
                })
                .await;
            this.update(cx, |transcript, cx| {
                if let Some(entry) = transcript.highlights.entries.get_mut(&key)
                    && entry.code_len == code_len
                {
                    entry.lines = Some(Arc::new(lines));
                    cx.notify();
                }
            })
            .ok();
        });
        self.entries.insert(
            (row_id, block_ix),
            HighlightEntry {
                code_len,
                lines: stale.clone(),
                _task: Some(task),
            },
        );
        stale
    }
}

// ---------------------------------------------------------------------------
// Transcript entity
// ---------------------------------------------------------------------------

struct CachedRows {
    fingerprint: u64,
    rows: Vec<Row>,
}

#[derive(Default, Clone, Copy)]
struct FoldState {
    /// User choice (click); `None` uses the collapsed default.
    open: Option<bool>,
    /// Bumped per toggle — keys the 200ms height tween.
    epoch: usize,
    /// Height at the moment of the toggle (the tween's start). The destination
    /// is always the *current* target height, so content growth after a toggle
    /// snaps instead of replaying a stale tween.
    from: f32,
    /// When the toggle happened. The tween is armed only for a short window
    /// after the click: gpui replays an element's animation on REMOUNT, and a
    /// virtualized row scrolling back into view is a remount — an armed-forever
    /// tween made every once-collapsed group flash open→closed on each
    /// reappearance (user report).
    toggled_at: Option<Instant>,
}

#[derive(Clone)]
pub enum TranscriptEvent {
    OpenTurnDiff {
        diff: TurnDiffManifest,
        file_path: Option<String>,
    },
}

#[derive(Clone)]
struct SavedScrollPosition {
    row_id: SharedString,
    entry_id: SharedString,
    page_id: Option<String>,
    row_index: usize,
    offset_in_item: Pixels,
    show_jump_button: bool,
    last_scroll_distance: f32,
}

pub struct Transcript {
    state: Entity<AppState>,
    list: ListState,
    rows: Vec<Row>,
    chat_id: Option<String>,
    /// Device-local viewport anchors keyed by chat. Stable row ids preserve
    /// the same reading position even when rows are inserted while away.
    scroll_positions: HashMap<String, SavedScrollPosition>,
    pending_scroll_restore: Option<SavedScrollPosition>,
    page_by_entry: HashMap<String, String>,
    row_cache: HashMap<String, CachedRows>,
    live_parsers: HashMap<String, IncrementalParser>,
    tree_cache: HashMap<String, (usize, Arc<BlockTree>)>,
    folds: HashMap<SharedString, FoldState>,
    /// Collapsed directory paths within expanded assistant-turn diff trees.
    /// Absence means fully expanded; state is device-local and per diff row.
    collapsed_change_paths: HashMap<SharedString, HashSet<String>>,
    /// Streaming fade veils, one per live markdown row (dropped on completion).
    veils: HashMap<SharedString, Rc<RefCell<RowVeil>>>,
    /// Live rows present in the transcript's REPLAY after (re)attaching to a
    /// chat: their veils are created pre-seeded, so text that was already
    /// streamed before the switch never fades in — only appends after it do
    /// (mugen's `FadePainter.attach` baseline; user report: switching back to
    /// a streaming session dissolved the entire reply).
    veil_baseline: std::collections::HashSet<SharedString>,
    /// Armed at attach, disarmed on the first sync whose transcript is
    /// non-empty: the baseline must be captured from the doc REPLAY frame,
    /// not the attach-time sync — selection clears the transcript and the
    /// replay lands async, so capturing at attach seeded nothing and the
    /// still-streaming reply faded in whole on every session switch (user
    /// report, round 2).
    veil_attach_pending: bool,
    /// Cross-frame flatten/shape-input cache (see [`RenderCache`]): fade
    /// frames reuse settled blocks' text+runs; the incremental parser's stable
    /// boundary invalidates only the live tail per commit.
    render_cache: Rc<RefCell<RenderCache>>,
    highlights: HighlightStore,
    show_jump_button: bool,
    /// Distance from the bottom at the last observation (wheel event or spring
    /// tick) — restick and escape are direction-aware
    /// (see [`Transcript::should_restick`]).
    last_scroll_distance: f32,
    /// The stick-to-bottom pin. Broken only by user input (wheel/touch up);
    /// re-engaged inside the 70px band, on own-send, and on the jump button.
    pinned: bool,
    spring: StickSpring,
    /// Wall-clock of the previous spring tick (`None` = parked).
    spring_last_tick: Option<Instant>,
    /// When the spring last landed on the bottom (settle-grace bookkeeping).
    spring_settled_at: Option<Instant>,
    /// A doc commit / wake happened before layout measured it — run at least
    /// one spring tick even though the pre-layout distance still reads 0.
    spring_kick: bool,
    /// One `on_next_frame` callback in flight at most.
    spring_scheduled: bool,
    scroll_anim: Option<Task<()>>,
    /// User-message destination selected by transcript keyboard/rail navigation.
    /// The stable message id keeps repeated keys ordered while rows move or load.
    turn_navigation_target: Option<String>,
    /// `(one-based target, total)` while keyboard/rail navigation waits for a
    /// historical page to materialize.
    turn_navigation_loading: Option<(usize, usize)>,
    /// Brief paint-only destination cue after a user-message jump.
    turn_navigation_highlight: Option<SharedString>,
    turn_navigation_highlight_clear: Option<Task<()>>,
    /// MessageRail width gate (set by the shell from the container width).
    rail_enabled: bool,
    /// Hovered rail tick (grows + shows the preview card).
    rail_hover: Option<usize>,
    /// `(row id, entry id)` under the pointer, used to reveal the entry's
    /// timestamp strip. Keyed by row so a row-to-row move within one entry cannot
    /// clear the reveal when the old row's leave event arrives after the new
    /// row's enter (enter/leave order across rows is not guaranteed).
    hovered_entry: Option<(SharedString, SharedString)>,
    /// Message whose hover action is showing copied feedback.
    copied_entry: Option<SharedString>,
    copied_entry_clear: Option<Task<()>>,
    /// Code block showing "Copied" feedback: `(row id, block ix)`, cleared by
    /// the companion task after ~1.2s.
    copied_code: Option<(SharedString, usize)>,
    copied_clear: Option<Task<()>>,
    /// Transcript attachment being viewed full-size (click a user thumbnail).
    attachment_preview: Option<crate::attachments::PreviewImage>,
    /// In-flight ReadAttachmentChunk loads, keyed `(deviceId, path)` — one per
    /// source; results land in the global attachment cache.
    attachment_loads: HashMap<(String, String), Task<()>>,
    /// Scheduled retry wake-ups for errored sources (the 2s→15s ladder).
    attachment_retries: HashMap<(String, String), Task<()>>,
    _observe: Subscription,
}

impl EventEmitter<TranscriptEvent> for Transcript {}

impl Transcript {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        // FollowMode stays Normal: the tail pin is ours (a per-frame spring),
        // not the list's per-layout hard snap.
        let list = ListState::new(0, ListAlignment::Bottom, px(OVERDRAW_PX));
        let weak = cx.weak_entity();
        list.set_scroll_handler(move |event: &ListScrollEvent, _window, cx| {
            weak.update(cx, |this: &mut Transcript, cx| {
                this.handle_scroll(event, cx)
            })
            .ok();
        });
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.sync(cx));
        let mut this = Self {
            state,
            list,
            rows: Vec::new(),
            chat_id: None,
            scroll_positions: HashMap::new(),
            pending_scroll_restore: None,
            page_by_entry: HashMap::new(),
            row_cache: HashMap::new(),
            live_parsers: HashMap::new(),
            tree_cache: HashMap::new(),
            folds: HashMap::new(),
            collapsed_change_paths: HashMap::new(),
            veils: HashMap::new(),
            veil_baseline: std::collections::HashSet::new(),
            veil_attach_pending: true,
            render_cache: Rc::new(RefCell::new(RenderCache::default())),
            highlights: HighlightStore::default(),
            show_jump_button: false,
            last_scroll_distance: 0.0,
            pinned: true,
            spring: StickSpring::new(),
            spring_last_tick: None,
            spring_settled_at: None,
            spring_kick: false,
            spring_scheduled: false,
            scroll_anim: None,
            turn_navigation_target: None,
            turn_navigation_loading: None,
            turn_navigation_highlight: None,
            turn_navigation_highlight_clear: None,
            rail_enabled: true,
            rail_hover: None,
            hovered_entry: None,
            copied_entry: None,
            copied_entry_clear: None,
            copied_code: None,
            copied_clear: None,
            attachment_preview: None,
            attachment_loads: HashMap::new(),
            attachment_retries: HashMap::new(),
            _observe: observe,
        };
        this.sync(cx);
        this
    }

    // ---- rail plumbing (rendering lives in crate::rail) ----

    /// Shell-driven width gate: the rail hides below 48rem of container width.
    pub fn set_rail_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.rail_enabled != enabled {
            self.rail_enabled = enabled;
            cx.notify();
        }
    }

    pub(crate) fn rail_enabled(&self) -> bool {
        self.rail_enabled
    }

    pub(crate) fn rail_hover(&self) -> Option<usize> {
        self.rail_hover
    }

    pub(crate) fn set_rail_hover(&mut self, hover: Option<usize>) {
        self.rail_hover = hover;
    }

    pub(crate) fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub(crate) fn list_state(&self) -> &ListState {
        &self.list
    }

    pub(crate) fn state_entity(&self) -> &Entity<AppState> {
        &self.state
    }

    /// Prepare any programmatic jump away from the live tail. This must run for
    /// snaps as well as animations so reduced motion cannot remain bottom-pinned.
    pub(crate) fn begin_navigation_scroll(&mut self) {
        self.pending_scroll_restore = None;
        self.scroll_anim = None;
        self.pinned = false;
        self.spring.reset();
        self.spring_last_tick = None;
        self.spring_settled_at = None;
    }

    /// Replace the transcript's scroll animation task (rail click / jump).
    pub(crate) fn set_scroll_task(&mut self, task: Task<()>) {
        self.begin_navigation_scroll();
        self.scroll_anim = Some(task);
    }

    pub(crate) fn turn_navigation_target(&self) -> Option<&str> {
        self.turn_navigation_target.as_deref()
    }

    pub(crate) fn set_turn_navigation_target(
        &mut self,
        message_id: String,
        loading: Option<(usize, usize)>,
    ) {
        self.turn_navigation_target = Some(message_id);
        self.turn_navigation_loading = loading;
    }

    pub(crate) fn clear_turn_navigation_loading(&mut self) {
        self.turn_navigation_loading = None;
    }

    pub(crate) fn clear_turn_navigation(&mut self) {
        self.turn_navigation_target = None;
        self.turn_navigation_loading = None;
    }

    pub(crate) fn finish_navigation_scroll(&mut self) {
        let distance = self.distance_from_bottom();
        self.last_scroll_distance = distance;
        self.show_jump_button = distance > SCROLL_BUTTON_THRESHOLD_PX || !self.is_glued();
    }

    pub(crate) fn highlight_user_message(&mut self, message_id: String, cx: &mut Context<Self>) {
        let highlighted = SharedString::from(message_id);
        self.turn_navigation_highlight = Some(highlighted.clone());
        self.turn_navigation_highlight_clear = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(650))
                .await;
            this.update(cx, |transcript, cx| {
                if transcript.turn_navigation_highlight.as_ref() == Some(&highlighted) {
                    transcript.turn_navigation_highlight = None;
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn save_scroll_position(&mut self) {
        let Some(chat_id) = self.chat_id.as_ref() else {
            return;
        };
        // If the destination page is still loading, retain the original exact
        // anchor rather than replacing it with the temporary placeholder.
        if self.pending_scroll_restore.is_some() {
            return;
        }
        if self.pinned {
            self.scroll_positions.remove(chat_id);
            return;
        }
        let offset = self.list.logical_scroll_top();
        let Some(row) = self.rows.get(offset.item_ix) else {
            self.scroll_positions.remove(chat_id);
            return;
        };
        self.scroll_positions.insert(
            chat_id.clone(),
            SavedScrollPosition {
                row_id: row.id.clone(),
                entry_id: row.entry_id.clone(),
                page_id: self.page_by_entry.get(row.entry_id.as_ref()).cloned(),
                row_index: offset.item_ix,
                offset_in_item: offset.offset_in_item,
                show_jump_button: self.show_jump_button,
                last_scroll_distance: self.last_scroll_distance,
            },
        );
    }

    fn restore_scroll_position(&mut self) {
        let Some(saved) = self.pending_scroll_restore.clone() else {
            return;
        };
        let exact = self.rows.iter().position(|row| row.id == saved.row_id);
        let same_entry = || {
            self.rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.entry_id == saved.entry_id)
                .min_by_key(|(index, _)| index.abs_diff(saved.row_index))
                .map(|(index, _)| index)
        };
        if let Some(item_ix) = exact.or_else(same_entry) {
            self.list.scroll_to(gpui::ListOffset {
                item_ix,
                offset_in_item: saved.offset_in_item,
            });
            self.pending_scroll_restore = None;
        } else if let Some(item_ix) = saved.page_id.as_deref().and_then(|page_id| {
            self.rows.iter().position(|row| {
                matches!(
                    &row.kind,
                    RowKind::HistoryPlaceholder { page_id: row_page, .. }
                        if row_page.as_ref() == page_id
                )
            })
        }) {
            // Put the cold page in view so its existing render hook loads it;
            // keep the exact anchor pending for the page-replacement frame.
            self.list.scroll_to(gpui::ListOffset {
                item_ix,
                offset_in_item: px(0.0),
            });
        } else if !self.rows.is_empty() {
            // The anchor was deleted while this chat was away. Preserve the
            // nearest surviving logical position instead of resetting.
            self.list.scroll_to(gpui::ListOffset {
                item_ix: saved.row_index.min(self.rows.len() - 1),
                offset_in_item: saved.offset_in_item,
            });
            self.pending_scroll_restore = None;
        } else {
            return;
        }
        self.pinned = false;
        self.show_jump_button = saved.show_jump_button;
        self.last_scroll_distance = saved.last_scroll_distance;
    }

    pub(crate) fn distance_from_bottom(&self) -> f32 {
        let max = f32::from(self.list.max_offset_for_scrollbar().y);
        let cur = f32::from(self.list.scroll_px_offset_for_scrollbar().y);
        (max + cur).max(0.0)
    }

    /// Whether a user scroll should re-engage the bottom pin: inside the 70px
    /// stick band *and* moving toward the bottom. Direction matters — a small
    /// wheel-up notch near the bottom stays inside the band, and re-sticking
    /// on it would snap the view straight back, making the pin unbreakable.
    pub fn should_restick(distance: f32, previous_distance: f32) -> bool {
        distance <= STICK_THRESHOLD_PX && distance < previous_distance
    }

    fn handle_scroll(&mut self, _event: &ListScrollEvent, cx: &mut Context<Self>) {
        // The list invokes this handler ONLY from its wheel/touch input path
        // (programmatic scroll_by/scroll_to never re-enter it), while holding
        // its internal RefCell borrow — reading the ListState back
        // synchronously panics with "already mutably borrowed". Defer to the
        // end of the effect cycle, after the list has released its borrow.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            this.update(cx, |this: &mut Transcript, cx| {
                // Any direct manipulation supersedes async navigation or a
                // page-backed restoration; the user's gesture always wins.
                this.pending_scroll_restore = None;
                this.scroll_anim = None;
                this.clear_turn_navigation();
                let top = this.list.logical_scroll_top().item_ix;
                if let Some(Row {
                    kind: RowKind::HistoryPlaceholder { page_id, .. },
                    ..
                }) = this.rows.get(top)
                {
                    let page_id = page_id.to_string();
                    // Clamp to the loaded side of the cold boundary. This
                    // programmatic reposition discards the current wheel/touch
                    // momentum; loading never resumes motion without new input.
                    this.list.scroll_to(gpui::ListOffset {
                        item_ix: (top + 1).min(this.rows.len()),
                        offset_in_item: px(0.0),
                    });
                    this.pinned = false;
                    this.spring.reset();
                    this.state.update(cx, |state, cx| {
                        state.load_transcript_page(page_id, cx);
                    });
                    cx.notify();
                    return;
                }
                let distance = this.distance_from_bottom();
                let previous = this.last_scroll_distance;
                this.last_scroll_distance = distance;
                if distance > previous + 1.0 && distance > AT_BOTTOM_PX {
                    // User input moving away from the bottom breaks the pin.
                    // Content growth never lands here — it doesn't fire the
                    // scroll handler (mugen §1e: interrupt from input, not
                    // scrollbar position).
                    this.pinned = false;
                    this.spring.reset();
                    this.spring_last_tick = None;
                } else if distance <= AT_BOTTOM_PX || Self::should_restick(distance, previous) {
                    // Returning toward the bottom inside the 70px band (or
                    // arriving at it) re-engages the pin with a glide.
                    if !this.pinned {
                        this.pinned = true;
                        this.wake_spring();
                    }
                }
                let show = distance > SCROLL_BUTTON_THRESHOLD_PX && !this.pinned;
                if show != this.show_jump_button {
                    this.show_jump_button = show;
                }
                cx.notify();
            })
            .ok();
        });
    }

    /// Own-send re-engage: glide to the end, then stay pinned.
    pub fn on_own_send(&mut self, cx: &mut Context<Self>) {
        self.engage_pin(cx);
    }

    /// Whether the transcript is currently pinned to the bottom.
    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Whether the shell should float the "Scroll to bottom" pill (scrolled
    /// more than [`SCROLL_BUTTON_THRESHOLD_PX`] off the end, unpinned).
    pub fn jump_button_shown(&self) -> bool {
        self.show_jump_button
    }

    /// The scroll-to-bottom pill's click: glide back to the end and re-pin.
    pub fn jump_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.engage_pin(cx);
    }

    /// Re-engage the bottom pin with a glide. Long jumps teleport to within
    /// [`GLIDE_MAX_VIEWPORTS`] of the end first (mugen `springToBottom`);
    /// reduced motion snaps.
    fn engage_pin(&mut self, cx: &mut Context<Self>) {
        self.pending_scroll_restore = None;
        self.scroll_anim = None;
        self.clear_turn_navigation();
        self.pinned = true;
        self.show_jump_button = false;
        if motion::reduced_motion(cx) {
            self.list.scroll_to_end();
            cx.notify();
            return;
        }
        let viewport = f32::from(self.list.viewport_bounds().size.height);
        let distance = self.distance_from_bottom();
        let glide_max = GLIDE_MAX_VIEWPORTS * viewport;
        if viewport > 0.0 && distance > glide_max {
            self.list.scroll_by(px(distance - glide_max));
        }
        self.wake_spring();
        cx.notify();
    }

    /// Arm the per-frame spring driver — `render` schedules the next frame
    /// while [`Self::spring_should_run`].
    fn wake_spring(&mut self) {
        self.spring_settled_at = None;
        self.spring_kick = true;
    }

    /// Whether the spring loop needs another frame: off the bottom, carrying
    /// residual motion, or inside the post-landing settle grace.
    fn spring_should_run(&self) -> bool {
        self.spring_kick
            || self.distance_from_bottom() > 0.5
            || !self.spring.is_idle()
            || self.spring_settled_at.is_some()
    }

    /// Whether the scroll offset is in a bottom-glued representation (`None`
    /// or anchored past the end) — states where the next layout hard-snaps to
    /// the new end instead of holding a pixel position.
    pub(crate) fn is_glued(&self) -> bool {
        self.list.logical_scroll_top().item_ix >= self.rows.len()
    }

    /// One spring frame: observe target growth, step the stepper, apply the
    /// delta, park after the settle grace. Runs from `window.on_next_frame`,
    /// i.e. after layout — measurements are fresh.
    fn step_spring(&mut self, cx: &mut Context<Self>) {
        self.spring_kick = false;
        if !self.pinned {
            self.spring_last_tick = None;
            return;
        }
        let now = Instant::now();
        let frames = match self.spring_last_tick {
            Some(last) => (now.duration_since(last).as_secs_f32() * 1000.0 / SPRING_FRAME_MS)
                .min(SPRING_MAX_CATCHUP_FRAMES),
            None => 1.0,
        };
        self.spring_last_tick = Some(now);

        let target = f32::from(self.list.max_offset_for_scrollbar().y);
        let mut distance = self.distance_from_bottom();
        // Long jumps (chat switch mid-history, huge pastes) teleport first.
        let viewport = f32::from(self.list.viewport_bounds().size.height);
        let glide_max = GLIDE_MAX_VIEWPORTS * viewport;
        if viewport > 0.0 && distance > glide_max {
            self.list.scroll_by(px(distance - glide_max));
            distance = glide_max;
        }
        let pos = target - distance;
        let next = self.spring.step(pos, target, frames);
        if next > pos {
            self.list.scroll_by(px(next - pos));
        }
        self.last_scroll_distance = (target - next).max(0.0);

        if target - next <= 0.5 {
            let settled = *self.spring_settled_at.get_or_insert(now);
            if now.duration_since(settled) >= Duration::from_millis(SPRING_SETTLE_GRACE_MS)
                && self.spring.is_idle()
            {
                // Park: stop scheduling frames until the next wake.
                self.spring.reset();
                self.spring_last_tick = None;
                self.spring_settled_at = None;
                return;
            }
        } else {
            self.spring_settled_at = None;
        }
        cx.notify();
    }

    /// Rebuild rows from app state; splice minimal ranges into the list.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let (selected, entries, manifest, pages, loading, errors, echoes) = {
            let s = self.state.read(cx);
            (
                s.selected_chat.clone(),
                s.transcript.clone(),
                s.transcript_manifest.clone(),
                s.transcript_pages.clone(),
                s.transcript_loading_pages.clone(),
                s.transcript_page_errors.clone(),
                s.pending_echoes().to_vec(),
            )
        };

        let attached = selected != self.chat_id;
        if attached {
            self.save_scroll_position();
            self.chat_id = selected.clone();
            self.pending_scroll_restore = selected
                .as_ref()
                .and_then(|chat_id| self.scroll_positions.get(chat_id).cloned());
            self.rows.clear();
            self.row_cache.clear();
            self.live_parsers.clear();
            self.tree_cache.clear();
            self.folds.clear();
            self.collapsed_change_paths.clear();
            self.veils.clear();
            self.page_by_entry.clear();
            self.copied_entry = None;
            self.copied_entry_clear = None;
            self.clear_turn_navigation();
            self.turn_navigation_highlight = None;
            self.turn_navigation_highlight_clear = None;
            self.render_cache.borrow_mut().clear();
            self.highlights.entries.clear();
            self.list.reset(0);
            self.pinned = self.pending_scroll_restore.is_none();
            self.spring.reset();
            self.spring_last_tick = None;
            self.spring_settled_at = None;
            self.spring_kick = false;
            if let Some(saved) = &self.pending_scroll_restore {
                self.show_jump_button = saved.show_jump_button;
                self.last_scroll_distance = saved.last_scroll_distance;
            } else {
                self.show_jump_button = false;
                self.last_scroll_distance = 0.0;
            }
        }

        self.page_by_entry = pages
            .iter()
            .flat_map(|page| {
                page.messages
                    .iter()
                    .map(move |entry| (entry.id.clone(), page.id.clone()))
            })
            .collect();

        let mut new_rows: Vec<Row> = Vec::new();
        if let Some(manifest) = &manifest {
            for descriptor in &manifest.pages {
                if let Some(page) = pages.iter().find(|page| page.id == descriptor.id) {
                    for entry in &page.messages {
                        new_rows.extend(self.rows_for(entry, false));
                    }
                } else {
                    let estimated_height = (descriptor.message_count as f32 * 92.0
                        + descriptor.estimated_bytes as f32 * 0.18)
                        .clamp(320.0, 48_000.0);
                    new_rows.push(Row {
                        id: SharedString::from(format!("history-page:{}", descriptor.id)),
                        version: fnv1a(descriptor.revision.as_bytes()),
                        turn_start: true,
                        kind: RowKind::HistoryPlaceholder {
                            page_id: descriptor.id.clone().into(),
                            estimated_height,
                            loading: loading.contains(&descriptor.id),
                            failed: errors.contains(&descriptor.id),
                        },
                        entry_id: SharedString::from(format!("history-page:{}", descriptor.id)),
                        timestamp: None,
                        copy_text: None,
                    });
                }
            }
        } else {
            for entry in &entries {
                new_rows.extend(self.rows_for(entry, false));
            }
        }
        for echo in &echoes {
            new_rows.extend(self.rows_for(echo, true));
        }

        // Loaded-page eviction must release completed markdown trees too; a
        // virtual list bounds mounted views, not these derived caches.
        let loaded_entry_ids: HashSet<&str> = pages
            .iter()
            .flat_map(|page| page.messages.iter().map(|entry| entry.id.as_str()))
            .chain(entries.iter().map(|entry| entry.id.as_str()))
            .chain(echoes.iter().map(|entry| entry.id.as_str()))
            .collect();
        self.row_cache
            .retain(|entry_id, _| loaded_entry_ids.contains(entry_id.as_str()));
        self.tree_cache.retain(|part_key, _| {
            part_key
                .split_once('#')
                .is_some_and(|(entry_id, _)| loaded_entry_ids.contains(entry_id))
        });

        // Text already streamed before this (re)attach is the veil BASELINE:
        // its rows' veils seed instead of fading (render creates them from
        // this set), so only post-switch appends animate. Captured from the
        // first NON-EMPTY transcript after attach — the replay frame — never
        // the attach-time sync, whose transcript is still empty (selection
        // clears it; the doc watch refills it async).
        if attached {
            self.veil_baseline.clear();
            self.veil_attach_pending = true;
        }
        if self.veil_attach_pending && !entries.is_empty() {
            self.veil_attach_pending = false;
            self.veil_baseline = new_rows
                .iter()
                .filter(|r| matches!(r.kind, RowKind::LiveMarkdown { .. }))
                .map(|r| r.id.clone())
                .collect();
        }

        // Veils live exactly as long as their live row — drop them on the
        // live→complete flip (any mid-fade chunk snaps to full, matching the
        // row's version splice).
        self.veils.retain(|id, _| {
            new_rows
                .iter()
                .any(|r| &r.id == id && matches!(r.kind, RowKind::LiveMarkdown { .. }))
        });
        self.veil_baseline.retain(|id| {
            new_rows
                .iter()
                .any(|r| &r.id == id && matches!(r.kind, RowKind::LiveMarkdown { .. }))
        });

        let was_empty = self.rows.is_empty();
        match diff_rows(&self.rows, &new_rows) {
            None => {
                self.rows = new_rows;
                return;
            }
            Some((old_range, count)) => {
                // Any replaced row's cached flatten results are stale — and
                // because live replies update only the rows whose content hash
                // changed (the tail), this is O(changed rows) per commit, never
                // O(reply).
                for row in &self.rows[old_range.clone()] {
                    self.render_cache.borrow_mut().invalidate_row(&row.id);
                }
                if rows_changed_in_place(&self.rows, &new_rows, &old_range, count) {
                    // `splice` resets the logical offset to the top when the
                    // changed row contains the viewport. That makes a tall,
                    // growing tool group jump up before the bottom spring pulls
                    // it back down. Remeasure keeps the same pixel anchor.
                    self.list.remeasure_items(old_range);
                } else {
                    self.list.splice(old_range, count);
                }
            }
        }
        self.rows = new_rows;
        self.restore_scroll_position();
        if self.pinned {
            if motion::reduced_motion(cx) || was_empty {
                // First fill (chat open) lands at the bottom instantly
                // (mugen initialScroll:'bottom'); reduced motion always snaps.
                self.list.scroll_to_end();
            } else if self.is_glued() {
                // A glued offset (`None` / anchored past the end) makes the
                // upcoming layout hard-snap to the new end — the per-commit
                // stutter. Materialize a pixel anchor a hair above the bottom
                // so layout holds position and the spring glides the growth.
                self.list.scroll_by(px(-0.75));
            }
            self.spring_kick = true;
        }
        cx.notify();
    }

    /// Cached row build for one entry (streaming entries bypass the cache).
    fn rows_for(&mut self, entry: &SessionMessageEntry, pending: bool) -> Vec<Row> {
        let streaming = entry.status == Some(MessageStatus::Streaming);
        let fingerprint = entry_fingerprint(entry, pending);
        if !streaming
            && let Some(cached) = self.row_cache.get(&entry.id)
            && cached.fingerprint == fingerprint
        {
            return cached.rows.clone();
        }

        let live_parsers = &mut self.live_parsers;
        let tree_cache = &mut self.tree_cache;
        let mut parse = |key: &str, text: &str| -> Arc<BlockTree> {
            // Streaming prose is omitted until its provider message boundary,
            // so every text part reaching the parser is already stable and can
            // use the completed-tree cache even while tools keep the entry live.
            parse_for_row(false, key, text, live_parsers, tree_cache).0
        };
        let rows = rows_for_entry(entry, pending, &mut parse);

        if !streaming {
            self.row_cache.insert(
                entry.id.clone(),
                CachedRows {
                    fingerprint,
                    rows: rows.clone(),
                },
            );
        }
        rows
    }

    fn toggle_changes(&mut self, row_id: SharedString) {
        let entry = self.folds.entry(row_id.clone()).or_default();
        entry.open = Some(!entry.open.unwrap_or(false));
        if let Some(index) = self.rows.iter().position(|row| row.id == row_id) {
            self.list.remeasure_items(index..index + 1);
        }
    }

    fn toggle_change_path(&mut self, row_id: SharedString, path: String) {
        let collapsed = self
            .collapsed_change_paths
            .entry(row_id.clone())
            .or_default();
        if !collapsed.remove(&path) {
            collapsed.insert(path);
        }
        if let Some(index) = self.rows.iter().position(|row| row.id == row_id) {
            self.list.remeasure_items(index..index + 1);
        }
    }

    fn toggle_fold(&mut self, row_id: SharedString, tool_count: usize, active: bool) {
        let entry = self.folds.entry(row_id).or_default();
        let currently_open = entry.open.unwrap_or(false);
        entry.from = chips_height(visible_tool_range(tool_count, currently_open, active).len());
        entry.open = Some(!currently_open);
        entry.epoch += 1;
        // Switching between the active preview and the full list changes which
        // chips are mounted, so animate only inactive groups where clipping is
        // visually continuous.
        entry.toggled_at = (!active).then(Instant::now);
    }

    // ---- attachment read-back and transcript cache ----

    /// Devices that may own a user message's attachment files: the chat's host
    /// device, which receives uploads, plus this device.
    fn attachment_device_ids(&self, cx: &Context<Self>) -> Vec<String> {
        let state = self.state.read(cx);
        let mut ids = Vec::new();
        if let Some(chat) = state.selected_chat_row() {
            ids.push(chat.device_id.clone());
        }
        if let Some(local) = state.local_device_id.clone()
            && !ids.contains(&local)
        {
            ids.push(local);
        }
        ids
    }

    /// Effective load state for one attachment across its candidate devices:
    /// first Loaded source wins; otherwise loads are (re)claimed and the
    /// snapshot degrades Loading → Error with a scheduled retry wake-up.
    fn attachment_state(
        &mut self,
        device_ids: &[String],
        path: &str,
        cx: &mut Context<Self>,
    ) -> crate::attachments::AttachmentSnapshot {
        use crate::attachments::{AttachmentSnapshot, attachment_snapshot, begin_load};
        for dev in device_ids {
            if let AttachmentSnapshot::Loaded(image) = attachment_snapshot(dev, path) {
                return AttachmentSnapshot::Loaded(image);
            }
        }
        let mut any_loading = false;
        let mut min_retry: Option<Duration> = None;
        for dev in device_ids {
            if begin_load(dev, path) {
                self.spawn_attachment_load(dev.clone(), path.to_string(), cx);
            }
            match attachment_snapshot(dev, path) {
                AttachmentSnapshot::Loaded(image) => return AttachmentSnapshot::Loaded(image),
                AttachmentSnapshot::Loading => any_loading = true,
                AttachmentSnapshot::Error { retry_in } => {
                    min_retry = Some(min_retry.map_or(retry_in, |m| m.min(retry_in)));
                }
            }
        }
        if any_loading {
            return AttachmentSnapshot::Loading;
        }
        match min_retry {
            Some(retry_in) => {
                if let Some(dev) = device_ids.first() {
                    self.schedule_attachment_retry((dev.clone(), path.to_string()), retry_in, cx);
                }
                AttachmentSnapshot::Error { retry_in }
            }
            // No candidate devices at all — the "unavailable" thumb, no retry.
            None => AttachmentSnapshot::Error {
                retry_in: Duration::MAX,
            },
        }
    }

    fn spawn_attachment_load(&mut self, device_id: String, path: String, cx: &mut Context<Self>) {
        use crate::attachments::{read_attachment_image, store_error, store_loaded};
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            store_error(&device_id, &path);
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        // Relay-forward only for a genuinely remote owner; the local device's
        // files are served directly.
        let target = (local.as_deref() != Some(device_id.as_str())).then(|| device_id.clone());
        let key = (device_id.clone(), path.clone());
        let task = cx.spawn(async move |this, cx| {
            match read_attachment_image(&engine, cx.background_executor(), target.as_deref(), &path)
                .await
            {
                Some(loaded) => store_loaded(&device_id, &path, loaded.name.into(), loaded.image),
                None => store_error(&device_id, &path),
            }
            this.update(cx, |transcript, cx| {
                transcript
                    .attachment_loads
                    .remove(&(device_id.clone(), path.clone()));
                cx.notify();
            })
            .ok();
        });
        self.attachment_loads.insert(key, task);
    }

    /// One wake-up per errored source: after the backoff elapses, a notify
    /// re-renders the thumb, whose `begin_load` then claims the retry.
    fn schedule_attachment_retry(
        &mut self,
        key: (String, String),
        delay: Duration,
        cx: &mut Context<Self>,
    ) {
        if delay == Duration::MAX || self.attachment_retries.contains_key(&key) {
            return;
        }
        let wake = key.clone();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(delay + Duration::from_millis(60))
                .await;
            this.update(cx, |transcript, cx| {
                transcript.attachment_retries.remove(&wake);
                cx.notify();
            })
            .ok();
        });
        self.attachment_retries.insert(key, task);
    }

    /// The right-aligned thumbnail strip above a user bubble.
    fn render_user_attachments(
        &mut self,
        row_id: &SharedString,
        atts: &[crate::attachments::UserImageAttachment],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::attachments::AttachmentSnapshot;
        let device_ids = self.attachment_device_ids(cx);
        let mut strip = div()
            .w_full()
            .h(px(ATT_STRIP_H))
            .flex()
            .flex_row()
            .justify_end()
            .items_start()
            .gap(px(8.0))
            .overflow_hidden()
            .px(px(4.0))
            .pt(px(4.0));
        for (aix, att) in atts.iter().enumerate() {
            let state = self.attachment_state(&device_ids, &att.path, cx);
            let frame = div()
                .flex_none()
                .w(px(ATT_THUMB_W))
                .h(px(ATT_THUMB_H))
                .rounded(px(8.0))
                .overflow_hidden();
            let thumb: AnyElement = match state {
                AttachmentSnapshot::Loaded(image) => {
                    let preview = crate::attachments::PreviewImage {
                        name: image.name.clone(),
                        image: image.image.clone(),
                    };
                    frame
                        .id(SharedString::from(format!("{row_id}#att{aix}")))
                        .border_1()
                        .border_color(crate::theme::hairline(0.11))
                        .bg(crate::theme::ink(0.035))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.attachment_preview = Some(preview.clone());
                            cx.notify();
                        }))
                        .child(
                            img(image.image.clone())
                                .size_full()
                                .object_fit(ObjectFit::Cover),
                        )
                        .into_any_element()
                }
                // Errored/unavailable: the dashed "missing" thumb.
                AttachmentSnapshot::Error { .. } => frame
                    .border_1()
                    .border_dashed()
                    .border_color(crate::theme::hairline(0.14))
                    .bg(crate::theme::ink(0.025))
                    .into_any_element(),
                // Loading: the pulsing skeleton (same wash as popover skeletons).
                AttachmentSnapshot::Loading => frame
                    .border_1()
                    .border_color(crate::theme::hairline(0.08))
                    .bg(crate::theme::ink(0.055))
                    .opacity(
                        0.35 + 0.4
                            * motion::pulse_wave(motion::pulse_delta(
                                &motion::JOLT_PULSE,
                                cx.entity_id(),
                                cx,
                            )),
                    )
                    .into_any_element(),
            };
            strip = strip.child(thumb);
        }
        strip.into_any_element()
    }

    // ---- rendering ----

    fn render_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(ix).cloned() else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        let top_gap = if ix == 0 {
            GAP_TURN + 10.0
        } else {
            top_gap_for(ix.checked_sub(1).and_then(|i| self.rows.get(i)), &row)
        };
        let bottom_pad = if ix + 1 == self.rows.len() {
            Theme::TRANSCRIPT_FADE_BAND + 8.0
        } else {
            0.0
        };

        let inner: AnyElement = match &row.kind {
            RowKind::User {
                tree,
                attachments,
                pending,
            } => {
                let attachments = attachments.clone();
                let tree = tree.clone();
                let pending = *pending;
                let navigation_highlighted = self
                    .turn_navigation_highlight
                    .as_ref()
                    .is_some_and(|message_id| message_id == &row.entry_id);
                // Attachment thumbnails sit above the right-aligned bubble;
                // image-only sends show no bubble at all.
                let mut column = div().w_full().flex().flex_col();
                if !attachments.is_empty() {
                    column = column.child(self.render_user_attachments(&row.id, &attachments, cx));
                }
                if !tree.is_empty() {
                    let opts = RenderOptions {
                        row_key: row.id.clone(),
                        veil: None,
                        cache: (!render_cache_disabled()).then(|| self.render_cache.clone()),
                        now: Instant::now(),
                        copy: Some(self.copy_ui_for(&row.id, cx)),
                        open_link: Some(self.open_link_for(cx)),
                        streaming: false,
                        svg_renderer: Some(cx.svg_renderer()),
                    };
                    let highlights = self.code_highlight_for(&row.id, &tree, None, cx);
                    let markdown = render::render_tree(&tree, &opts, &theme, window, &|ix| {
                        highlights.get(&ix).cloned().flatten()
                    });
                    // `min_w_0` is load-bearing: gpui text answers min/max-content
                    // probes with its UNWRAPPED width, so without it the bubble's
                    // automatic min-size is the full single-line width — the flex
                    // item can't shrink, `justify_end` pushes the overflow off the
                    // left edge, and long prompts render as one clipped line
                    // instead of wrapping inside the 80% column cap.
                    column = column.child(
                        div().w_full().flex().justify_end().child(
                            div()
                                .min_w_0()
                                .max_w(px(MAX_CONTENT_WIDTH * 0.8))
                                .bg(if navigation_highlighted {
                                    theme.accent.opacity(0.14)
                                } else {
                                    theme.surface_raised
                                })
                                .rounded(px(Theme::BUBBLE_RADIUS))
                                .px(px(16.0))
                                .py(px(10.0))
                                .text_color(theme.text)
                                .when(pending, |el| el.opacity(0.65))
                                .child(markdown),
                        ),
                    );
                }
                column.into_any_element()
            }
            RowKind::Markdown { tree, block_ix } => {
                let opts = RenderOptions {
                    row_key: row.id.clone(),
                    veil: None,
                    cache: (!render_cache_disabled()).then(|| self.render_cache.clone()),
                    now: Instant::now(),
                    copy: Some(self.copy_ui_for(&row.id, cx)),
                    open_link: Some(self.open_link_for(cx)),
                    streaming: false,
                    svg_renderer: Some(cx.svg_renderer()),
                };
                let highlight = self.code_highlight_for(&row.id, tree, Some(*block_ix), cx);
                let Some(top) = tree.blocks.get(*block_ix) else {
                    return gpui::Empty.into_any_element();
                };
                render::render_block(
                    &top.block,
                    *block_ix,
                    *block_ix,
                    &opts,
                    &theme,
                    window,
                    highlight
                        .get(block_ix)
                        .and_then(|o| o.as_deref())
                        .map(|v| v.as_slice()),
                )
            }
            RowKind::LiveMarkdown { tree, block_ix } => {
                // Per-appended-chunk fade veil (opacity only — layout commits
                // instantly). Reduced motion renders with no veil at all.
                // Baseline rows (text already streamed when the transcript
                // attached) start seeded: the existing reply must not fade in
                // on a session switch — only fresh appends animate.
                let veil = (!motion::reduced_motion(cx)).then(|| {
                    self.veils
                        .entry(row.id.clone())
                        .or_insert_with(|| {
                            if self.veil_baseline.contains(&row.id) {
                                Rc::new(RefCell::new(RowVeil::seeded()))
                            } else {
                                Rc::default()
                            }
                        })
                        .clone()
                });
                let opts = RenderOptions {
                    row_key: row.id.clone(),
                    veil: veil.clone(),
                    cache: (!render_cache_disabled()).then(|| self.render_cache.clone()),
                    now: Instant::now(),
                    copy: Some(self.copy_ui_for(&row.id, cx)),
                    open_link: Some(self.open_link_for(cx)),
                    streaming: true,
                    svg_renderer: Some(cx.svg_renderer()),
                };
                let highlight = self.code_highlight_for(&row.id, tree, Some(*block_ix), cx);
                let Some(top) = tree.blocks.get(*block_ix) else {
                    return gpui::Empty.into_any_element();
                };
                let timer = frame_stats_enabled().then(Instant::now);
                let el = render::render_block(
                    &top.block,
                    *block_ix,
                    *block_ix,
                    &opts,
                    &theme,
                    window,
                    highlight
                        .get(block_ix)
                        .and_then(|o| o.as_deref())
                        .map(|v| v.as_slice()),
                );
                if let Some(start) = timer {
                    record_live_frame_us(start.elapsed().as_micros() as u64);
                }
                // The attach pass for this row is done (every element rendered
                // above seeded its baseline synchronously): elements appearing
                // from the NEXT pass on are newly streamed and fade normally.
                if let Some(veil) = &veil {
                    veil.borrow_mut().finish_seeding();
                }
                // Drive the veil clock: while any chunk is still dissolving,
                // repaint next frame (self-limiting — one callback per frame).
                if veil.is_some_and(|v| v.borrow().is_fading()) {
                    let id = cx.entity_id();
                    window.on_next_frame(move |_, cx| cx.notify(id));
                }
                el
            }
            RowKind::ToolGroup { tools, active } => {
                self.render_tool_group(&row.id, tools, *active, &theme, cx)
            }
            RowKind::Changes { diff } => self.render_changes(&row.id, diff, &theme, cx),
            RowKind::InputChip { header, resolved } => {
                input_chip(header.clone(), *resolved, &theme)
            }
            RowKind::ErrorChip { message } => error_chip(message.clone(), &theme),
            RowKind::HarnessSwitch { from, to } => harness_switch(*from, *to, &theme),
            RowKind::HistoryPlaceholder {
                page_id,
                estimated_height,
                loading,
                failed,
            } => {
                let page_id = page_id.to_string();
                if !loading && !failed {
                    let state = self.state.clone();
                    let requested = page_id.clone();
                    cx.defer(move |cx| {
                        state.update(cx, |state, cx| {
                            state.load_transcript_page(requested, cx);
                        });
                    });
                }
                let label = if *failed {
                    "Couldn’t load these messages · Retry"
                } else {
                    "Loading earlier messages…"
                };
                let state = self.state.clone();
                let placeholder = div()
                    .h(px(*estimated_height))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(label);
                if *failed {
                    placeholder
                        .id(SharedString::from(format!("retry-history:{page_id}")))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            let requested = page_id.clone();
                            state.update(cx, |state, cx| {
                                state.load_transcript_page(requested, cx);
                            });
                        })
                        .into_any_element()
                } else {
                    placeholder.into_any_element()
                }
            }
        };

        // Hover-revealed timestamp strip: a reserved 16px lane under the
        // entry's last row; the label only
        // flips opacity, so revealing it never shifts the virtualizer's
        // layout. User entries align end (under the bubble), assistant start.
        let is_user_row = matches!(row.kind, RowKind::User { .. });
        let hovered = self
            .hovered_entry
            .as_ref()
            .is_some_and(|(_, entry)| entry == &row.entry_id);
        let message_copied = self.copied_entry.as_ref() == Some(&row.entry_id);
        // Assistant timestamp strips start 4px below the message text; the
        // native markdown column has no such
        // bottom padding, so the strip carries it as top inset (grown into the
        // reserved height: reveal still never shifts layout). User rows are
        // flush: the Timestamp follows the bubble HStack directly (VStack gap
        // defaults to 0 in mugen), the label's centering inside the 16px lane
        // supplies the remaining gap.
        let strip = row.timestamp.map(|ms| {
            let copy_button = row.copy_text.clone().map(|text| {
                let entry_id = row.entry_id.clone();
                div()
                    .id(SharedString::from(format!("copy-message:{}", row.entry_id)))
                    .size(px(16.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .hover(|style| style.bg(crate::theme::wash(0.12)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.copy_message(entry_id.clone(), text.clone(), cx);
                    }))
                    .child(
                        crate::icons::icon(if message_copied {
                            crate::icons::CHECK
                        } else {
                            crate::icons::COPY
                        })
                        .size(px(11.0))
                        .text_color(if message_copied {
                            theme.success_muted
                        } else {
                            theme.text_muted.opacity(0.55)
                        }),
                    )
            });
            let timestamp = div()
                .text_size(px(11.0))
                .text_color(theme.text_muted.opacity(0.55))
                .child(SharedString::from(format_timestamp(ms, &chrono::Local)));
            let metadata = div().flex().flex_row().items_center().gap(px(4.0));
            let metadata = if is_user_row {
                metadata.children(copy_button).child(timestamp)
            } else {
                metadata.child(timestamp).children(copy_button)
            };
            div()
                .h(px(if is_user_row { 16.0 } else { 20.0 }))
                .when(!is_user_row, |el| el.pt(px(4.0)))
                .w_full()
                .flex()
                .items_center()
                // No horizontal inset because the message text and timestamp
                // begin on the same edge (group
                // padding 4 + inner VStack 4 = 8 = group 4 + px-1 4). Here the
                // markdown text / user bubble sit AT the content column edges,
                // so the label must too — assistant label's left edge on the
                // text's first-character x, user label's right edge on the
                // bubble's right edge (user-reported 4px drift). Keep the copy
                // icon inside of the timestamp on user rows to preserve that
                // right-edge alignment.
                .when(is_user_row, |el| el.justify_end())
                .when(hovered, |el| {
                    el.child(motion::fade_quick(
                        SharedString::from(format!("ts-{}", row.id)),
                        metadata,
                    ))
                })
        });
        let entry_id = row.entry_id.clone();
        let row_id = row.id.clone();
        div()
            .id(row.id.clone())
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    let next = Some((row_id.clone(), entry_id.clone()));
                    if this.hovered_entry != next {
                        let entry_changed = this
                            .hovered_entry
                            .as_ref()
                            .is_none_or(|(_, entry)| entry != &entry_id);
                        this.hovered_entry = next;
                        if entry_changed {
                            cx.notify();
                        }
                    }
                } else if this
                    .hovered_entry
                    .as_ref()
                    .is_some_and(|(row, _)| row == &row_id)
                {
                    // Only the row that OWNS the current reveal may clear it —
                    // a stale leave from an earlier row must not blank the
                    // strip the newly entered row just lit.
                    this.hovered_entry = None;
                    cx.notify();
                }
            }))
            .w_full()
            .flex()
            .justify_center()
            .pt(px(top_gap))
            .pb(px(bottom_pad))
            // Wide gutters (jolt `px-4 @3xl:px-12`) around the 46rem column.
            .px(px(48.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(MAX_CONTENT_WIDTH))
                    .min_w_0()
                    .child(inner)
                    .children(strip),
            )
            .into_any_element()
    }

    fn copy_message(&mut self, entry_id: SharedString, text: SharedString, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        self.copied_entry = Some(entry_id);
        self.copied_entry_clear = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1200))
                .await;
            this.update(cx, |this, cx| {
                this.copied_entry = None;
                this.copied_entry_clear = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Copy-button wiring for one row's code blocks ([`render::CopyUi`]):
    /// click writes the block's code to the clipboard and shows a transient
    /// "Copied" check on that block for ~1.2s (overlay — no layout shift).
    fn copy_ui_for(&self, row_id: &SharedString, cx: &mut Context<Self>) -> render::CopyUi {
        let copied_ix = self
            .copied_code
            .as_ref()
            .filter(|(id, _)| id == row_id)
            .map(|(_, ix)| *ix);
        let row_key = row_id.clone();
        let entity = cx.weak_entity();
        let handler: render::CopyHandler = Rc::new(move |ix, code, _window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(code.to_string()));
            let row_key = row_key.clone();
            entity
                .update(cx, |this, cx| {
                    this.copied_code = Some((row_key, ix));
                    this.copied_clear = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(1200))
                            .await;
                        this.update(cx, |this, cx| {
                            this.copied_code = None;
                            this.copied_clear = None;
                            cx.notify();
                        })
                        .ok();
                    }));
                    cx.notify();
                })
                .ok();
        });
        render::CopyUi { handler, copied_ix }
    }

    fn open_link_for(&self, cx: &mut Context<Self>) -> render::OpenLinkFn {
        let cwd = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|chat| chat.cwd.as_deref())
            .map(std::path::PathBuf::from);
        Rc::new(move |raw, _window, cx| {
            let target = LinkTarget::parse(raw, cwd.as_deref());
            if let Some(url) = target.open_url() {
                cx.open_url(&url);
            } else {
                tracing::warn!(destination = raw, "could not convert link target to URL");
            }
        })
    }

    /// Request highlights for the code blocks of a tree. `only` limits to one
    /// block index (split rows); `None` covers the whole tree (live rows).
    fn code_highlight_for(
        &mut self,
        row_id: &SharedString,
        tree: &Arc<BlockTree>,
        only: Option<usize>,
        cx: &mut Context<Self>,
    ) -> HashMap<usize, Option<Arc<Vec<Vec<Token>>>>> {
        let mut out = HashMap::new();
        for (ix, top) in tree.blocks.iter().enumerate() {
            if only.is_some_and(|o| o != ix) {
                continue;
            }
            if let Block::CodeBlock { language, code } = &top.block
                && let Some(lang) = language.as_deref().and_then(lang_for_tag)
            {
                out.insert(
                    ix,
                    self.highlights.request(row_id.clone(), ix, lang, code, cx),
                );
            }
        }
        out
    }

    fn render_changes(
        &mut self,
        row_id: &SharedString,
        diff: &Arc<TurnDiffManifest>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self
            .folds
            .get(row_id)
            .and_then(|fold| fold.open)
            .unwrap_or(false);
        let count = diff.files.len();
        let partial = (diff.attribution == jolt_proto::TurnDiffAttribution::Partial)
            .then_some(" · partial")
            .unwrap_or_default();
        let title = format!(
            "{count} changed file{}{partial}",
            if count == 1 { "" } else { "s" }
        );
        let toggle_id = row_id.clone();
        let open_diff = diff.clone();

        let header = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .id(SharedString::from(format!("{row_id}-toggle")))
                    .min_w_0()
                    .flex_1()
                    .h(px(32.0))
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .cursor_pointer()
                    .text_size(px(12.0))
                    .text_color(theme.text)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_changes(toggle_id.clone());
                        cx.notify();
                    }))
                    .child(
                        div()
                            .size(px(18.0))
                            .flex_none()
                            .rounded(px(5.0))
                            .bg(crate::theme::ink(0.06))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(10.0))
                            .text_color(theme.text_muted.opacity(0.7))
                            .child(SharedString::from(if open { "▾" } else { "▸" })),
                    )
                    .child(
                        div()
                            .truncate()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(title),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_family(theme.font_mono.clone())
                            .text_color(theme.diff_add)
                            .child(SharedString::from(format!("+{}", diff.additions))),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_family(theme.font_mono.clone())
                            .text_color(theme.diff_del)
                            .child(SharedString::from(format!("−{}", diff.deletions))),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(format!("{row_id}-open")))
                    .flex_none()
                    .px(px(8.0))
                    .h(px(24.0))
                    .flex()
                    .items_center()
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .hover(|style| style.text_color(theme.text).bg(crate::theme::ink(0.04)))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.emit(TranscriptEvent::OpenTurnDiff {
                            diff: (*open_diff).clone(),
                            file_path: None,
                        });
                    }))
                    .child("Open diff"),
            );

        let files = open.then(|| {
            let rows = change_tree_rows(&diff.files, self.collapsed_change_paths.get(row_id));
            div()
                .pt(px(2.0))
                .pb(px(4.0))
                .flex()
                .flex_col()
                .children(rows.into_iter().map(|entry| match entry {
                    ChangeTreeRow::Directory {
                        path,
                        name,
                        depth,
                        collapsed,
                    } => {
                        let toggle_row = row_id.clone();
                        let toggle_path = path.clone();
                        div()
                            .id(SharedString::from(format!("{row_id}-dir-{path}")))
                            .h(px(CHANGE_TREE_ROW_HEIGHT))
                            .pl(px(6.0 + depth as f32 * CHANGE_TREE_INDENT))
                            .pr(px(8.0))
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .hover(|style| style.bg(crate::theme::ink(0.04)).text_color(theme.text))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_change_path(toggle_row.clone(), toggle_path.clone());
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .size(px(14.0))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_size(px(9.0))
                                    .text_color(theme.text_muted.opacity(0.7))
                                    .child(SharedString::from(if collapsed {
                                        "▸"
                                    } else {
                                        "▾"
                                    })),
                            )
                            .child(
                                crate::icons::icon(crate::icons::FOLDER)
                                    .size(px(14.0))
                                    .flex_none()
                                    .text_color(theme.text_muted),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .truncate()
                                    .font_family(theme.font_mono.clone())
                                    .child(name),
                            )
                            .into_any_element()
                    }
                    ChangeTreeRow::File {
                        file_index,
                        name,
                        depth,
                    } => {
                        let file = &diff.files[file_index];
                        let open_diff = diff.clone();
                        let path = file.path.clone();
                        div()
                            .id(SharedString::from(format!("{row_id}-file-{}", file.id)))
                            .h(px(CHANGE_TREE_ROW_HEIGHT))
                            .pl(px(6.0 + depth as f32 * CHANGE_TREE_INDENT))
                            .pr(px(8.0))
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .hover(|style| style.bg(crate::theme::ink(0.04)).text_color(theme.text))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(TranscriptEvent::OpenTurnDiff {
                                    diff: (*open_diff).clone(),
                                    file_path: Some(path.clone()),
                                });
                            }))
                            .child(div().size(px(14.0)).flex_none())
                            .child(
                                crate::icons::icon(crate::icons::FILE)
                                    .size(px(14.0))
                                    .flex_none()
                                    .text_color(theme.text_muted),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .truncate()
                                    .font_family(theme.font_mono.clone())
                                    .child(name),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .font_family(theme.font_mono.clone())
                                    .text_color(theme.diff_add)
                                    .child(SharedString::from(format!("+{}", file.additions))),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .font_family(theme.font_mono.clone())
                                    .text_color(theme.diff_del)
                                    .child(SharedString::from(format!("−{}", file.deletions))),
                            )
                            .into_any_element()
                    }
                }))
        });

        div()
            .mt(px(4.0))
            .p(px(6.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(theme.border.opacity(0.8))
            .bg(theme.surface_raised.opacity(0.45))
            .child(header)
            .children(files)
            .into_any_element()
    }

    fn render_tool_group(
        &mut self,
        row_id: &SharedString,
        tools: &Arc<Vec<ToolItem>>,
        active: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fold = self.folds.get(row_id).copied().unwrap_or_default();
        let open = fold.open.unwrap_or(false);
        let visible_tools = visible_tool_range(tools.len(), open, active);
        let target = chips_height(visible_tools.len());
        let animating = !active
            && fold.epoch > 0
            && fold
                .toggled_at
                .is_some_and(|at| at.elapsed() < FOLD_TWEEN_WINDOW);
        // Keep all chips mounted while an inactive group collapses so the
        // existing height tween clips its content instead of shrinking blank
        // space. Active preview toggles do not animate.
        let rendered_tools = if animating && fold.from > target {
            0..tools.len()
        } else {
            visible_tools.clone()
        };
        let summary = if active {
            tools.last().map_or_else(
                || tool_group_summary(tools),
                |tool| tool_activity(&tool.call, tool.resolved, tool.is_error),
            )
        } else {
            tool_group_summary(tools)
        };

        let toggle_id = row_id.clone();
        let tool_count = tools.len();
        // Header: a small chevron tile centered over the chips' guide rail,
        // then a quiet 12px summary.
        let header = div()
            .id(SharedString::from(format!("{row_id}-hdr")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(4.0))
            .h(px(26.0))
            .cursor_pointer()
            .text_size(px(12.0))
            // Quiet even when children failed: agents routinely have failed
            // probes mid-work, and a red HEADER read as "this whole step
            // broke" (user report). Failures still show on the individual
            // chips with a destructive tint and in the summary's
            // "· N failed" count.
            .text_color(theme.text_muted)
            .hover(|s| s.text_color(theme.text))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_fold(toggle_id.clone(), tool_count, active);
                cx.notify();
            }))
            .child(
                div()
                    .size(px(18.0))
                    .flex_none()
                    .rounded(px(5.0))
                    .bg(crate::theme::ink(0.06))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(10.0))
                    .text_color(theme.text_muted.opacity(0.7))
                    .child(SharedString::from(if open { "▾" } else { "▸" })),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(summary)),
            );

        let first_rendered = rendered_tools.start;
        let chips = div()
            .pt(px(CHIPS_TOP_PAD))
            .flex()
            .flex_col()
            .gap(px(CHIP_GAP))
            .children(
                tools[rendered_tools]
                    .iter()
                    .enumerate()
                    .map(|(offset, tool)| {
                        tool_chip(
                            tool,
                            theme,
                            SharedString::from(format!(
                                "{row_id}-tool-state-{}",
                                first_rendered + offset
                            )),
                            cx,
                        )
                    }),
            );

        // Fold body: 200ms committed-height tween on a USER toggle only — and
        // only within a short window of the click. Active-preview changes and
        // content growth never tween, and a SETTLED fold renders at its static
        // height: leaving the tween armed replayed it on every remount, which
        // in a virtualized list means every scroll-back-into-view (only `open`
        // toggles animate — composes with the stick spring).
        let body: AnyElement = if animating {
            let from = fold.from;
            div()
                .overflow_hidden()
                .child(chips)
                .with_animation(
                    SharedString::from(format!("{row_id}-fold{}", fold.epoch)),
                    RESIZE.animation(),
                    move |el, t| el.h(px(motion::lerp(from, target, t))),
                )
                .into_any_element()
        } else {
            div()
                .overflow_hidden()
                .h(px(target))
                .child(chips)
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .into_any_element()
    }
}

/// Convert projected file-mention ranges to Markdown code spans so they retain
/// their compact chip treatment inside an otherwise normal Markdown prompt.
fn user_markdown_source(text: &str, mentions: &[crate::composer::SentMentionSpan]) -> String {
    if mentions.is_empty() {
        return text.to_string();
    }
    let mut markdown = String::with_capacity(text.len() + mentions.len() * 2);
    let mut at = 0;
    for mention in mentions {
        markdown.push_str(&text[at..mention.range.start]);
        markdown.push_str(&markdown_code_span(&text[mention.range.clone()]));
        at = mention.range.end;
    }
    markdown.push_str(&text[at..]);
    markdown
}

fn markdown_code_span(text: &str) -> String {
    let mut longest = 0;
    let mut current = 0;
    for byte in text.bytes() {
        if byte == b'`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    let delimiter = "`".repeat(longest + 1);
    format!("{delimiter}{text}{delimiter}")
}

fn harness_label(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "Claude Code",
        HarnessId::Codex => "Codex",
        HarnessId::Pi => "Pi",
        HarnessId::Mock => "Mock",
    }
}

fn harness_switch(from: HarnessId, to: HarnessId, theme: &Theme) -> AnyElement {
    let identity = |harness| {
        let (icon, tint) = crate::pickers::harness_brand_icon(harness);
        div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .child(
                crate::icons::icon(icon)
                    .size(px(11.0))
                    .text_color(tint.unwrap_or_else(|| theme.text_muted.opacity(0.7))),
            )
            .child(harness_label(harness))
    };

    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(10.0))
        .child(div().h(px(1.0)).flex_1().bg(crate::theme::hairline(0.07)))
        .child(
            div()
                .h(px(24.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .rounded_full()
                .border_1()
                .border_color(crate::theme::hairline(0.08))
                .bg(crate::theme::ink(0.025))
                .px(px(9.0))
                .text_size(px(10.0))
                .text_color(theme.text_muted.opacity(0.72))
                .child("Switched")
                .child(identity(from))
                .child("→")
                .child(identity(to)),
        )
        .child(div().h(px(1.0)).flex_1().bg(crate::theme::hairline(0.07)))
        .into_any_element()
}

/// The transcript ErrorChip: a 34px row (`rounded-[10px] border
/// border-red-400/[0.16]
/// bg-red-400/[0.05] px-2 text-[12px]`) with a 20px red-washed tile holding a
/// 12px DangerTriangle (`bg-red-400/[0.12] text-red-300/80`), a medium
/// "Error" label, then the human message truncating at `text-foreground/80` —
/// a subtle red-tinted wash, never a bare red-stroke box.
fn error_chip(message: SharedString, theme: &Theme) -> AnyElement {
    let red_300 = theme.danger_muted; // tailwind red-300
    let danger = theme.danger; // red-400
    div()
        .py(px(4.0))
        .w_full()
        .child(
            div()
                .h(px(34.0))
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .rounded(px(10.0))
                .border_1()
                .border_color(danger.opacity(0.16))
                .bg(danger.opacity(0.05))
                .px(px(8.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .flex_none()
                        .size(px(20.0))
                        .rounded(px(6.0))
                        .bg(danger.opacity(0.12))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            crate::icons::icon(crate::icons::ALERT_TRIANGLE)
                                .size(px(12.0))
                                .text_color(red_300.opacity(0.8)),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(red_300.opacity(0.8))
                        .child(SharedString::from("Error")),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_color(theme.text.opacity(0.8))
                        .child(message),
                ),
        )
        .into_any_element()
}

/// A passive one-line chip marking a question the agent asked; interactive
/// controls live in the composer:
/// 34px row, `rounded-[10px] border-white/[0.08] bg-white/[0.045] px-2
/// text-[12px]`, a 20px `bg-white/[0.09]` icon tile with a 12px
/// ChatRoundLine, the medium "Question" label, then the truncating value —
/// the first question's header once resolved, "Awaiting your answer…" while
/// pending. Neutral tones throughout; resolution never recolors the chip.
fn input_chip(header: SharedString, resolved: bool, theme: &Theme) -> AnyElement {
    let value: SharedString = if resolved {
        header
    } else {
        "Awaiting your answer…".into()
    };
    div()
        .py(px(4.0))
        .w_full()
        .child(
            div()
                .h(px(34.0))
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .rounded(px(10.0))
                .border_1()
                .border_color(crate::theme::hairline(0.08))
                .bg(crate::theme::ink(0.045))
                .px(px(8.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .flex_none()
                        .size(px(20.0))
                        .rounded(px(6.0))
                        .bg(crate::theme::ink(0.09))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            crate::icons::icon(crate::icons::MESSAGE_CIRCLE)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Question")),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_color(theme.text.opacity(0.9))
                        .child(value),
                ),
        )
        .into_any_element()
}

/// The Tabler glyph for a tool call.
fn tool_icon_path(call: &ToolCall) -> &'static str {
    match call {
        ToolCall::Exec { .. } => crate::icons::TERMINAL_2,
        ToolCall::ReadFile { .. } | ToolCall::ApplyPatch { .. } => crate::icons::FILE,
        ToolCall::WriteFile { .. } => crate::icons::FILE_PLUS,
        ToolCall::EditFile { .. } => crate::icons::PENCIL,
        ToolCall::Search { .. } => crate::icons::SEARCH,
        ToolCall::Glob { .. } => crate::icons::FOLDERS,
        ToolCall::WebFetch { .. } | ToolCall::WebSearch { .. } => crate::icons::WORLD,
        ToolCall::Todo { .. } => crate::icons::LIST_CHECK,
        ToolCall::SpawnAgent { .. } => crate::icons::USER,
        ToolCall::Mcp { .. } | ToolCall::Unknown { .. } => crate::icons::APPS,
    }
}

/// One tool chip row: a guide rail on the left (continuous across stacked
/// chips; the rail spans the row's full height, threading the chips to their
/// group toggle, then the chip card.
fn tool_chip(
    tool: &ToolItem,
    theme: &Theme,
    activity_key: SharedString,
    cx: &mut Context<Transcript>,
) -> AnyElement {
    let (label, detail) = tool_chip_content(&tool.call);
    let tint = if tool.is_error {
        theme.danger
    } else if tool.resolved {
        theme.text_muted
    } else {
        theme.text
    };
    let lifecycle: AnyElement = if tool.is_error {
        crate::icons::icon(crate::icons::CIRCLE_X)
            .size(px(13.0))
            .text_color(theme.danger)
            .into_any_element()
    } else if tool.resolved {
        crate::icons::icon(crate::icons::CHECK)
            .size(px(13.0))
            .text_color(theme.success_muted.opacity(0.8))
            .into_any_element()
    } else {
        crate::loaders::activity_spinner(activity_key, theme, 12.0, cx.entity_id(), cx)
            .into_any_element()
    };
    div()
        .h(px(CHIP_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        // Guide rail: hairline centered under the header's chevron tile.
        .child(
            div()
                .ml(px(12.0))
                .h_full()
                .w(px(1.0))
                .flex_none()
                .bg(crate::theme::ink(0.08)),
        )
        .child(
            div()
                .ml(px(12.0))
                .h(px(CHIP_CARD_HEIGHT))
                .min_w_0()
                .flex_1()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .rounded(px(9.0))
                .border_1()
                .border_color(crate::theme::hairline(if tool.resolved {
                    0.07
                } else {
                    0.12
                }))
                .bg(crate::theme::ink(if tool.resolved { 0.03 } else { 0.055 }))
                .px(px(8.0))
                .text_size(px(12.0))
                .child(
                    // Icon tile (`size-[18px] rounded-[5px] bg-white/[0.08]`,
                    // icon size-3).
                    div()
                        .size(px(18.0))
                        .flex_none()
                        .rounded(px(5.0))
                        .bg(crate::theme::ink(0.08))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            crate::icons::icon(tool_icon_path(&tool.call))
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(tint)
                        .child(SharedString::from(label)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(if tool.is_error {
                            theme.danger
                        } else {
                            theme.text.opacity(0.85)
                        })
                        .child(SharedString::from(detail)),
                )
                .child(
                    div()
                        .size(px(18.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(lifecycle),
                ),
        )
        .into_any_element()
}

fn entry_fingerprint(entry: &SessionMessageEntry, pending: bool) -> u64 {
    let mut acc: Vec<u8> = Vec::with_capacity(entry.parts.len() * 8 + 16);
    acc.extend_from_slice(entry.id.as_bytes());
    acc.push(match entry.status {
        None => 0,
        Some(MessageStatus::Streaming) => 1,
        Some(MessageStatus::Complete) => 2,
        Some(MessageStatus::Aborted) => 3,
    });
    acc.push(pending as u8);
    for part in &entry.parts {
        acc.extend_from_slice(part.id().as_bytes());
        acc.extend_from_slice(&(part.byte_len() as u64).to_le_bytes());
        if let MessagePart::Tool {
            is_error, resolved, ..
        } = part
        {
            acc.push(*is_error as u8 | (*resolved as u8) << 1);
        }
        if let MessagePart::Input { resolved, .. } = part {
            acc.push(0x10 | *resolved as u8);
        }
    }
    fnv1a(&acc)
}

impl Render for Transcript {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Release gpui-side decoded copies of any images the attachment LRU
        // evicted since the last frame (no-op when nothing was evicted).
        crate::attachments::flush_evicted(Some(window), cx);
        // Spring driver: one on_next_frame callback at a time; each tick
        // notifies, which re-enters render and schedules the next frame until
        // the spring parks. Reduced motion never schedules (sync snaps).
        if self.pinned
            && !motion::reduced_motion(cx)
            && !self.spring_scheduled
            && self.spring_should_run()
        {
            self.spring_scheduled = true;
            let entity = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                entity
                    .update(cx, |this: &mut Transcript, cx| {
                        this.spring_scheduled = false;
                        this.step_spring(cx);
                    })
                    .ok();
            });
        }
        let rail = self.render_rail(cx);
        let history_loading = !self.state.read(cx).transcript_loading_pages.is_empty();
        let history_loading_label = self.turn_navigation_loading.map_or_else(
            || "Loading messages…".to_string(),
            |(target, total)| format!("Loading prompt {target} of {total}…"),
        );
        // The scroll-to-bottom pill is rendered by the SHELL (conversation
        // region overlay): it must float just above the composer and paint
        // OVER the bottom fade gradient, which is a later sibling of this
        // outlet — an overlay here would be tinted by the fade.
        let root = div()
            .relative()
            .size_full()
            .min_h_0()
            // FIRST child ⇒ paints first: clears the frame's markdown text-
            // selection registry before any row's text elements re-register
            // (document paint order = selection order; see markdown/render.rs).
            .child(crate::markdown::render::selection_frame_reset())
            .child(
                list(self.list.clone(), cx.processor(Self::render_row))
                    .size_full()
                    .with_sizing_behavior(gpui::ListSizingBehavior::Auto),
            )
            .child(rail)
            .when(history_loading, |el| {
                el.child(
                    div()
                        .absolute()
                        .top(px(12.0))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .rounded(px(999.0))
                                .bg(Theme::of(cx).surface_raised)
                                .px(px(12.0))
                                .py(px(6.0))
                                .text_size(px(11.0))
                                .text_color(Theme::of(cx).text_muted)
                                .child(SharedString::from(history_loading_label)),
                        ),
                )
            });
        // Full-size viewer for a clicked user-bubble thumbnail
        // (AttachmentPreviewDialog: bare lightbox, click closes).
        if let Some(preview) = self.attachment_preview.clone() {
            let weak = cx.weak_entity();
            return root.child(crate::attachments::lightbox(
                window.viewport_size(),
                &preview,
                move |_, cx| {
                    weak.update(cx, |this, cx| {
                        this.attachment_preview = None;
                        cx.notify();
                    })
                    .ok();
                },
            ));
        }
        root
    }
}

#[cfg(test)]
mod tests;
