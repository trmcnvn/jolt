//! The composer: a hand-rolled multiline text input (adapted from gpui's
//! `examples/input.rs`), the compact↔expanded flip, the Send/Steer/Stop morph,
//! optimistic send with failure recovery, per-chat drafts, and the question
//! wizard that replaces the composer while a run awaits input.
//!
//! Pure decision logic (flip, auto-grow math, button morph, wizard reducer,
//! pending-input detection) lives in free functions/structs with unit tests;
//! the gpui element only feeds them measurements.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyTooltip, App, BorderStyle, Bounds, ClipboardEntry, ClipboardItem, Context, CursorStyle,
    DispatchPhase, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, GlobalElementId, KeyBinding, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ObjectFit, PaintQuad, PathPromptOptions, Pixels, Point,
    ScrollWheelEvent, SharedString, Style, StyledImage as _, Subscription, Task, TextRun,
    TextStyle, UTF16Selection, UnderlineStyle, Window, WrappedLine, actions, div, fill, img, point,
    prelude::*, px, quad, relative, size,
};
use unicode_segmentation::UnicodeSegmentation;

use jolt_api::{
    CancelQueuedPrompt, CreateWorktree, ExtractQuestions, ListCommands, Mutate, QueueCommand,
    SearchFiles, call as call_api,
};
use jolt_proto::{
    AgentCommand, AgentCommandSource, ExtractedQuestion, FileSearchMatch, GoalStatus, RunRequest,
    SandboxLevel, UserInputAnswer, UserInputQuestion,
};
use jolt_rpc::RpcError;
use jolt_session_doc::{
    GoalOperation, MessagePart, MessageRole, MessageStatus, SessionCommandPayload,
    SessionMessageEntry,
};

use crate::attachments::{self, StagedAttachment};
use crate::loaders;
use crate::motion;
use crate::pickers::Pickers;
use crate::state::{AppState, Indicator, PENDING_SEND_TTL_MS};
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Constants + pure decision logic
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellCommand {
    command: String,
    exclude_from_context: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellScope {
    AgentContext,
    LocalOnly,
}

fn shell_scope(text: &str) -> Option<ShellScope> {
    let text = text.trim_start();
    if text.starts_with("!!!") {
        None
    } else if text.starts_with("!!") {
        Some(ShellScope::LocalOnly)
    } else if text.starts_with('!') {
        Some(ShellScope::AgentContext)
    } else {
        None
    }
}

fn shell_command(text: &str) -> Option<ShellCommand> {
    let text = text.trim();
    let (prefix, exclude_from_context) = match shell_scope(text)? {
        ShellScope::AgentContext => ("!", false),
        ShellScope::LocalOnly => ("!!", true),
    };
    let command = text.strip_prefix(prefix)?.trim();
    (!command.is_empty()).then(|| ShellCommand {
        command: command.to_string(),
        exclude_from_context,
    })
}

fn bash_pending_transcript(command: &str) -> String {
    let command = format!("$ {command}");
    let longest = command
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let delimiter = "`".repeat((longest + 1).max(3));
    format!("{delimiter}bash\n{command}\n{delimiter}\n\n_Output pending…_")
}

fn shell_mode_chip(scope: ShellScope, theme: &Theme) -> gpui::AnyElement {
    let (label, color) = match scope {
        ShellScope::AgentContext => ("Bash · Agent context", theme.accent),
        ShellScope::LocalOnly => ("Bash · Local only", theme.text_muted),
    };
    div()
        .id("composer-shell-mode")
        .h(px(28.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .rounded_full()
        .border_1()
        .border_color(color.opacity(0.22))
        .bg(color.opacity(0.08))
        .px(px(10.0))
        .text_size(px(12.0))
        .text_color(color)
        .child(
            crate::icons::icon(crate::icons::TERMINAL_2)
                .size(px(13.0))
                .text_color(color),
        )
        .child(label)
        .into_any_element()
}

/// Expanded-mode textarea vertical padding: 16px top + 4px bottom.
pub const TEXTAREA_PAD_V: f32 = 20.0;
/// The expanded textarea box, including padding, is clamped to 76–260px.
/// The 76px floor applies even when
/// empty — it's what makes the always-expanded new-chat composer tall.
pub const TEXTAREA_MIN: f32 = 76.0;
pub const TEXTAREA_MAX: f32 = 260.0;
/// Expanded actions row: 4px top padding + 32px picker chips + 10px bottom
/// padding.
pub const ACTIONS_ROW_HEIGHT: f32 = 46.0;
/// The pill's 1px hairline, top + bottom (`rounded-[26px] border`).
pub const PILL_BORDER_V: f32 = 2.0;
/// Expanded composer bounds, border-box: 76 + 46 + 2 = 124 when empty (the
/// new-chat canvas), 260 + 46 + 2 = 308 at the content cap.
pub const COMPOSER_MIN_HEIGHT: f32 = TEXTAREA_MIN + ACTIONS_ROW_HEIGHT + PILL_BORDER_V;
pub const COMPOSER_MAX_HEIGHT: f32 = TEXTAREA_MAX + ACTIONS_ROW_HEIGHT + PILL_BORDER_V;
/// Compact pill, border-box: one-line textarea `py-3` (24) + one 22.75px line
/// plus the 2px hairline = 49. The
/// compact cluster (`py-1.5` + h-8 = 44) is shorter, so the textarea wins.
pub const COMPACT_TOTAL_HEIGHT: f32 = 49.0;
/// Width reserved for the compact action cluster. Model/traits labels shrink
/// within this lane; context, attach, and send retain their fixed hit targets.
pub const COMPACT_ACTIONS_WIDTH: f32 = 280.0;
/// Below this pill input width the composer always expands.
pub const MIN_COMPACT_INPUT_WIDTH: f32 = 200.0;
/// Input text metrics: `text-[14px] leading-relaxed` = 14 × 1.625 = 22.75.
pub const INPUT_LINE_HEIGHT: f32 = 22.75;
pub const INPUT_TEXT_SIZE: f32 = 14.0;
/// Intermediate single-select questions auto-advance after this long.
pub const AUTO_ADVANCE_MS: u64 = 220;
/// Drag-selection autoscroll runs at the display-friendly 60fps cadence.
pub const DRAG_SCROLL_FRAME_MS: u64 = 16;

const WIZARD_OPTIONS_MAX_HEIGHT: f32 = 280.0;
const COMMAND_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const COMMAND_CACHE_FAILURE_RETRY: Duration = Duration::from_secs(15);
const COMMAND_CACHE_CAPACITY: usize = 16;

const DEFAULT_PLACEHOLDER: &str = "What do you want to work on?";
const BUSY_PLACEHOLDER: &str = if cfg!(target_os = "macos") {
    "Enter steers now · ⌘+Enter queues next"
} else {
    "Enter steers now · Ctrl+Enter queues next"
};

fn composer_placeholder(is_busy: bool) -> &'static str {
    if is_busy {
        BUSY_PLACEHOLDER
    } else {
        DEFAULT_PLACEHOLDER
    }
}

/// Hysteresis slack for the expanded→compact flip: once expanded, the composer
/// only collapses when the text is comfortably narrower than the compact
/// capacity — expanding and collapsing share no boundary, so a width right at
/// the flip threshold can't oscillate between the two layouts.
pub const COLLAPSE_HYSTERESIS: f32 = 32.0;
/// During an interactive window resize the current mode is frozen until the
/// measured widths have been stable this long.
pub const RESIZE_SETTLE_MS: u64 = 150;

/// Compact↔expanded flip with hysteresis. `capacity` is the *compact-mode*
/// input capacity (a layout-stable width: measured while compact, tracked by
/// container-width deltas while expanded — never the post-flip measured width,
/// which differs per mode and would feed back into the decision):
/// - a newline always expands;
/// - while `resizing`, the current mode is kept (no flip until sizes settle);
/// - a too-narrow pill (`capacity < MIN_COMPACT_INPUT_WIDTH`) always expands;
/// - compact expands only when `text_width > capacity`; expanded collapses
///   only when `text_width < capacity - COLLAPSE_HYSTERESIS`.
pub fn composer_flip(
    expanded: bool,
    text_width: f32,
    capacity: f32,
    has_newline: bool,
    resizing: bool,
) -> bool {
    if has_newline {
        return true;
    }
    if resizing {
        return expanded;
    }
    if capacity < MIN_COMPACT_INPUT_WIDTH {
        return true;
    }
    if expanded {
        text_width >= capacity - COLLAPSE_HYSTERESIS
    } else {
        text_width > capacity
    }
}

/// Caret blink half-period (standard textarea cadence: ~500ms on / 500ms off).
pub const CARET_BLINK_MS: u64 = 500;

/// Caret blink phase for a time since the last keystroke/caret move: solid
/// through the first half-period (typing bursts never blink — each keystroke
/// resets the phase), then alternating.
pub fn caret_visible(ms_since_activity: u64) -> bool {
    (ms_since_activity / CARET_BLINK_MS).is_multiple_of(2)
}

/// Auto-grow: content height for a wrapped-line count.
pub fn input_content_height(wrapped_lines: usize) -> f32 {
    wrapped_lines.max(1) as f32 * INPUT_LINE_HEIGHT
}

/// Total expanded composer height (border-box) for a content height: the
/// textarea box (content + vertical padding) clamps to 76–260, then the 46px
/// actions row and the hairline
/// ride on top. Range 124–308.
pub fn composer_total_height(content_height: f32) -> f32 {
    (content_height + TEXTAREA_PAD_V).clamp(TEXTAREA_MIN, TEXTAREA_MAX)
        + ACTIONS_ROW_HEIGHT
        + PILL_BORDER_V
}

fn input_max_scroll(content_height: f32, viewport_height: f32) -> f32 {
    (content_height - viewport_height).max(0.0)
}

/// Apply GPUI's wheel delta to a top-origin input offset. Positive deltas mean
/// scrolling toward the start, matching gpui's built-in list/div behavior.
fn input_scroll_offset(
    current: f32,
    delta_y: f32,
    content_height: f32,
    viewport_height: f32,
) -> f32 {
    (current - delta_y).clamp(0.0, input_max_scroll(content_height, viewport_height))
}

/// Minimally adjust the viewport so the caret row is fully visible.
fn input_scroll_offset_for_cursor(
    current: f32,
    cursor_top: f32,
    cursor_height: f32,
    content_height: f32,
    viewport_height: f32,
) -> f32 {
    let mut next = current;
    if cursor_top < next {
        next = cursor_top;
    } else if cursor_top + cursor_height > next + viewport_height {
        next = cursor_top + cursor_height - viewport_height;
    }
    next.clamp(0.0, input_max_scroll(content_height, viewport_height))
}

/// Per-frame drag-selection scroll. Distance increases speed, capped at one
/// text row per frame so crossing the input boundary never causes a jump.
fn input_drag_scroll_delta(
    pointer_y: f32,
    viewport_top: f32,
    viewport_bottom: f32,
    line_height: f32,
) -> f32 {
    let distance = if pointer_y < viewport_top {
        pointer_y - viewport_top
    } else if pointer_y > viewport_bottom {
        pointer_y - viewport_bottom
    } else {
        return 0.0;
    };
    distance.signum() * (distance.abs() * 0.2).clamp(1.0, line_height)
}

/// Staged-attachment strip metrics: wrapping 56px thumbnails with 8px gaps,
/// 16px horizontal padding, and 12px top padding.
pub const STRIP_THUMB: f32 = 56.0;
pub const STRIP_GAP: f32 = 8.0;
pub const STRIP_PAD_TOP: f32 = 12.0;
pub const STRIP_PAD_X: f32 = 16.0;

/// Height the wrap strip adds to the pill for `count` staged thumbnails at an
/// `inner_width` pill content width (0 when empty). Mirrors flex-wrap: as many
/// 56px thumbs per row as fit with 8px gaps inside the 16px side insets.
pub fn attachment_strip_height(count: usize, inner_width: f32) -> f32 {
    if count == 0 {
        return 0.0;
    }
    let usable = (inner_width - 2.0 * STRIP_PAD_X).max(STRIP_THUMB);
    let per_row = (((usable + STRIP_GAP) / (STRIP_THUMB + STRIP_GAP)).floor() as usize).max(1);
    let rows = count.div_ceil(per_row);
    STRIP_PAD_TOP + rows as f32 * STRIP_THUMB + (rows - 1) as f32 * STRIP_GAP
}

/// Compact↔expanded flip morph. One committed flip starts exactly one 180ms
/// ease-out morph ([`motion::COLLAPSE`], the same
/// manual-drive pattern as shell.rs `WidthTween` — never `with_animation`,
/// whose element-id keying replays tweens on remount, round-6 §1–3).
///
/// The morph animates the pill's COMMITTED height: the flip commits its final
/// layout immediately (the input entity never remounts — the caret survives,
/// exactly as before) while the pill clips toward the live target. The pill's
/// bottom edge is stationary on screen, so the controls stay pinned to it
/// (constant screen-y; see the anchoring helpers below) and only the text
/// glides with the sweeping top edge. [`composer_flip`]'s hysteresis already
/// guarantees no oscillation at the boundary, and [`flip_morph_step`] never
/// restarts a morph while the committed mode holds. Reduced motion snaps: no
/// morph is ever created.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlipMorph {
    /// Rendered height when the flip committed — the animation's start point.
    pub from: f32,
    /// Commit time in ms on the caller's monotonic clock.
    pub start_ms: f32,
}

impl FlipMorph {
    /// Raw timeline position 0..1 over [`motion::COLLAPSE`]'s 180ms.
    fn raw(&self, now_ms: f32) -> f32 {
        let total = motion::COLLAPSE.total().as_secs_f32() * 1000.0;
        ((now_ms - self.start_ms) / total).clamp(0.0, 1.0)
    }

    /// Eased progress 0..1 (ease-out) — also drives the actions fade.
    pub fn progress(&self, now_ms: f32) -> f32 {
        motion::COLLAPSE.progress(self.raw(now_ms))
    }

    pub fn done(&self, now_ms: f32) -> bool {
        self.raw(now_ms) >= 1.0
    }

    /// Committed-height evaluation: eased lerp from the flip-time height to
    /// the LIVE target (auto-grow may move the target mid-morph — the morph
    /// tracks it instead of finishing on a stale height).
    pub fn height(&self, target: f32, now_ms: f32) -> f32 {
        motion::lerp(self.from, target, self.progress(now_ms))
    }
}

// -- morph anchoring (round-9 follow-up) ------------------------------------
// The pill sits at the BOTTOM of the shell column: growing it moves its TOP
// edge; the bottom edge is stationary on screen. The first morph cut anchored
// the pill's inner content to the top, so the actions/cluster (laid out at
// the inner bottom) rode the animating height up and down. The controls are
// therefore pinned to the stationary bottom edge (absolute bottom row when
// expanded, a bottom-justified row when compact) and only the TEXT glides
// with the sweeping top edge. The helpers below are the pure math.

/// Send/attach center sits 27px above the pill's outer bottom in expanded
/// mode (`pb-2.5` 10 + half the 32px content zone + 1px hairline) but 24.5px
/// in compact (centered in the 47px row) — an inherent 2.5px delta between
/// the two SOURCE geometries. The morph glides it instead of snapping.
pub const CLUSTER_Y_DELTA: f32 = 2.5;

/// Expanded text top padding across the morph: starts at the compact resting
/// inset (12 ≈ `py-3`) and eases to `pt-4` (16) — the first line glides with
/// the rising top edge instead of jumping at the commit.
pub fn morph_text_pad(progress: f32) -> f32 {
    motion::lerp(12.0, 16.0, progress)
}

/// Collapse-morph text glide: the committed compact row is bottom-anchored
/// (text resting top = 36px above the pill's outer bottom: 49 − 1 hairline −
/// 12 centering inset), while at the commit instant the text sat 17px below
/// the expanded pill's top (1 hairline + 16 `pt-4`) — i.e. `from − 17` above
/// the bottom. The decaying relative offset walks it down smoothly.
pub fn collapse_text_glide(from: f32, progress: f32) -> f32 {
    (from - 53.0).max(0.0) * (1.0 - progress)
}

/// The decaying [`CLUSTER_Y_DELTA`] offset for the in-flight morph.
/// The whole control cluster — chips AND attach/send — rides the stationary
/// bottom anchor at FULL alpha throughout (round-9 follow-up: any fade on the
/// picker chips read as flicker; their screen position is near-stationary
/// across the flip, so nothing needs to be hidden).
pub fn morph_cluster_dy(progress: f32) -> f32 {
    CLUSTER_Y_DELTA * (1.0 - progress)
}

/// Session and route changes snap the composer, matching the header inset
/// tween's zero-motion route behavior. The
/// nav-driven flip doesn't commit on the first render after a switch (the
/// draft swap has to be laid out and re-measured first), so a plain reset at
/// the nav instant leaks: `last_rendered_height` is repopulated before the
/// flip lands and the session change morphs 49↔124. Instead, every flip
/// committed within this wall-clock window of a navigation snaps. User-driven
/// flips need typing and can't land this fast after a switch.
pub const ROUTE_SNAP_MS: u64 = 250;

/// Advance the flip morph across one render pass. While the committed mode
/// holds, the morph is kept (a finished one clears) — same-mode renders can
/// NEVER restart the animation. A committed mode change starts one morph from
/// the last rendered height, which mid-flight is the CURRENT animated height,
/// so a reverse flip hands off seamlessly instead of popping to an endpoint.
/// Reduced motion (or a first paint with no measured height yet) snaps, and
/// `route_snap` (a session/route change within [`ROUTE_SNAP_MS`]) both blocks
/// arming AND kills anything in flight — navigation never animates the pill.
pub fn flip_morph_step(
    morph: Option<FlipMorph>,
    mode_changed: bool,
    last_height: f32,
    now_ms: f32,
    reduced_motion: bool,
    route_snap: bool,
) -> Option<FlipMorph> {
    if route_snap {
        return None;
    }
    if !mode_changed {
        return morph.filter(|m| !m.done(now_ms));
    }
    if reduced_motion || last_height <= 0.0 {
        return None;
    }
    Some(FlipMorph {
        from: last_height,
        start_ms: now_ms,
    })
}

/// What the send button is right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendButtonMode {
    /// No live run: plain send.
    Send,
    /// Live steerable run with text typed: "Send (steers the current run)".
    Steer,
    /// Live run, nothing typed: red stop square.
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendIntent {
    Run,
    Steer,
    Queue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SubmissionOrigin {
    Editor,
    GeneratedReview { review_id: String },
}

impl SubmissionOrigin {
    fn uses_editor_state(&self) -> bool {
        matches!(self, Self::Editor)
    }
}

pub fn send_button_mode(run_live: bool, has_text: bool) -> SendButtonMode {
    match (run_live, has_text) {
        (false, _) => SendButtonMode::Send,
        (true, true) => SendButtonMode::Steer,
        (true, false) => SendButtonMode::Stop,
    }
}

/// Find the unresolved input request the panel should serve, if any: an
/// unresolved input part on the LAST assistant entry — regardless of the
/// entry's run status. The question stays answerable until the user actually
/// answers it (user requirement): a run that died under its question (engine
/// restart reaping it) leaves an aborted entry whose answer the engine
/// delivers as a resumed turn (`RespondInput`'s dead-run fallback). A newer
/// assistant entry supersedes an unanswered question. Assistant-entry-scoped,
/// not last-entry: a steer prompt sent while the agent waits appends a USER
/// entry after the streaming assistant entry, and a last-entry-only read made
/// the question panel vanish exactly when the user typed. Read the live
/// assistant fold, rebuilt from replay even after the run died.
pub fn pending_input_request(
    transcript: &[SessionMessageEntry],
) -> Option<(String, Vec<UserInputQuestion>)> {
    transcript
        .iter()
        .rev()
        .find(|entry| entry.role == MessageRole::Assistant)
        .and_then(|entry| {
            entry.parts.iter().find_map(|part| match part {
                MessagePart::Input {
                    request_id,
                    questions,
                    resolved: false,
                    ..
                } => Some((request_id.clone(), questions.clone())),
                _ => None,
            })
        })
}

/// Whether the transcript shows `request_id` explicitly resolved (here or on
/// another device) — the wizard latch's release condition.
pub fn input_request_resolved(transcript: &[SessionMessageEntry], request_id: &str) -> bool {
    transcript.iter().any(|entry| {
        entry.parts.iter().any(|part| {
            matches!(
                part,
                MessagePart::Input {
                    request_id: rid,
                    resolved: true,
                    ..
                } if rid == request_id
            )
        })
    })
}

// ---------------------------------------------------------------------------
// Question wizard (pure reducer)
// ---------------------------------------------------------------------------

/// Reducer outcome of a wizard interaction.
#[derive(Debug, Clone, PartialEq)]
pub enum WizardStep {
    Stay,
    /// An intermediate single-select landed — advance after [`AUTO_ADVANCE_MS`].
    AutoAdvance,
    /// All pages answered — submit these answers.
    Done(Vec<UserInputAnswer>),
}

/// Paged question state ("1/3"): intermediate single-select pages auto-advance;
/// final, multi-select, and typed answers advance explicitly. Number keys 1-9
/// select and Back pages back.
#[derive(Debug, Clone)]
pub struct Wizard {
    pub request_id: String,
    pub questions: Vec<UserInputQuestion>,
    pub page: usize,
    picked: Vec<Vec<usize>>,
    typed: Vec<String>,
}

impl Wizard {
    pub fn new(request_id: String, questions: Vec<UserInputQuestion>) -> Self {
        let n = questions.len();
        Self {
            request_id,
            questions,
            page: 0,
            picked: vec![Vec::new(); n],
            typed: vec![String::new(); n],
        }
    }

    pub fn counter(&self) -> String {
        format!("{}/{}", self.page + 1, self.questions.len().max(1))
    }

    pub fn current(&self) -> Option<&UserInputQuestion> {
        self.questions.get(self.page)
    }

    pub fn is_picked(&self, option_ix: usize) -> bool {
        self.picked
            .get(self.page)
            .is_some_and(|p| p.contains(&option_ix))
    }

    /// Whether the current page has any picked option.
    pub fn page_has_pick(&self) -> bool {
        self.picked.get(self.page).is_some_and(|p| !p.is_empty())
    }

    /// Click/tap an option.
    pub fn select(&mut self, option_ix: usize) -> WizardStep {
        let Some(question) = self.questions.get(self.page) else {
            return WizardStep::Stay;
        };
        if option_ix >= question.options.len() {
            return WizardStep::Stay;
        }
        let multi = question.multi_select;
        let Some(picked) = self.picked.get_mut(self.page) else {
            return WizardStep::Stay;
        };
        if multi {
            match picked.iter().position(|&p| p == option_ix) {
                Some(at) => {
                    picked.remove(at);
                }
                None => picked.push(option_ix),
            }
            WizardStep::Stay
        } else if self.page + 1 >= self.questions.len() {
            *picked = vec![option_ix];
            // The final choice is a reviewable selection, not an immediate
            // approval. The user must explicitly submit it.
            WizardStep::Stay
        } else {
            *picked = vec![option_ix];
            WizardStep::AutoAdvance
        }
    }

    /// Number key 1-9.
    pub fn press_number(&mut self, number: usize) -> WizardStep {
        if number == 0 {
            return WizardStep::Stay;
        }
        self.select(number - 1)
    }

    pub fn set_typed(&mut self, text: String) {
        if let Some(slot) = self.typed.get_mut(self.page) {
            *slot = text;
        }
    }

    pub fn current_typed(&self) -> &str {
        self.typed.get(self.page).map_or("", String::as_str)
    }

    /// Explicit submit / auto-advance landing.
    pub fn advance(&mut self) -> WizardStep {
        if self.page + 1 < self.questions.len() {
            self.page += 1;
            WizardStep::Stay
        } else {
            WizardStep::Done(self.answers())
        }
    }

    /// Page back; false when already on the first page.
    pub fn back(&mut self) -> bool {
        if self.page > 0 {
            self.page -= 1;
            true
        } else {
            false
        }
    }

    /// Answers per question: free text overrides picked labels.
    pub fn answers(&self) -> Vec<UserInputAnswer> {
        self.questions
            .iter()
            .enumerate()
            .map(|(ix, q)| {
                let typed = self.typed.get(ix).map(|s| s.trim()).unwrap_or("");
                let labels = if !typed.is_empty() {
                    vec![typed.to_string()]
                } else {
                    self.picked
                        .get(ix)
                        .map(|picked| {
                            picked
                                .iter()
                                .filter_map(|&p| q.options.get(p).cloned())
                                .collect()
                        })
                        .unwrap_or_default()
                };
                UserInputAnswer {
                    question_id: q.id.clone(),
                    labels,
                }
            })
            .collect()
    }
}

/// The local `/answer`-style flow: extraction runs first, then free-text pages.
#[derive(Debug, Clone)]
enum ExtractedAnswerState {
    Extracting { source_message_id: String },
    Answering(ExtractedWizard),
}

#[derive(Debug, Clone)]
struct ExtractedAnswerFlow {
    chat_id: String,
    state: ExtractedAnswerState,
}

#[derive(Debug, Clone)]
struct ExtractedWizard {
    questions: Vec<ExtractedQuestion>,
    answers: Vec<String>,
    page: usize,
}

impl ExtractedWizard {
    fn new(questions: Vec<ExtractedQuestion>) -> Self {
        let count = questions.len();
        Self {
            questions,
            answers: vec![String::new(); count],
            page: 0,
        }
    }

    fn current(&self) -> Option<&ExtractedQuestion> {
        self.questions.get(self.page)
    }

    fn counter(&self) -> String {
        format!("{}/{}", self.page + 1, self.questions.len().max(1))
    }

    fn save(&mut self, answer: String) {
        if let Some(slot) = self.answers.get_mut(self.page) {
            *slot = answer;
        }
    }

    fn advance(&mut self) -> bool {
        if self.page + 1 < self.questions.len() {
            self.page += 1;
            false
        } else {
            true
        }
    }

    fn back(&mut self) -> bool {
        if self.page == 0 {
            false
        } else {
            self.page -= 1;
            true
        }
    }

    fn current_answer(&self) -> &str {
        self.answers.get(self.page).map_or("", String::as_str)
    }

    fn compiled_message(&self) -> String {
        let mut lines = vec!["I answered your questions in the following way:".to_string()];
        for (question, answer) in self.questions.iter().zip(&self.answers) {
            lines.push(String::new());
            lines.push(format!("Q: {}", question.question));
            if let Some(context) = &question.context {
                lines.push(format!("> {context}"));
            }
            let answer = answer.trim();
            lines.push(format!(
                "A: {}",
                if answer.is_empty() {
                    "(no answer)"
                } else {
                    answer
                }
            ));
        }
        lines.join("\n")
    }
}

/// Latest completed assistant text entry eligible for extraction.
fn latest_answerable_message(transcript: &[SessionMessageEntry]) -> Option<(String, String)> {
    transcript.iter().rev().find_map(|entry| {
        if entry.role != MessageRole::Assistant
            || entry.status != Some(jolt_session_doc::MessageStatus::Complete)
        {
            return None;
        }
        let text = entry
            .parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::Text { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        (!text.is_empty()).then(|| (entry.id.clone(), text))
    })
}

actions!(
    composer,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        Home,
        End,
        SelectHome,
        SelectEnd,
        DocStart,
        DocEnd,
        SelectDocStart,
        SelectDocEnd,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        DeleteWordLeft,
        DeleteWordRight,
        DeleteToLineStart,
        DeleteToLineEnd,
        ClearSelection,
        Copy,
        Cut,
        Paste,
        Newline,
        Submit,
        QueueSubmit,
        Undo,
        Redo,
        MentionTab,
        MentionEscape,
    ]
);

mod completion;
mod goals;
mod input;
mod queue;

use input::*;
pub use input::{ComposerInput, ComposerInputEvent, SentMentionSpan, init, sent_mention_display};

// ---------------------------------------------------------------------------
// Composer wrapper
// ---------------------------------------------------------------------------

/// Events the shell listens for.
#[derive(Debug, Clone)]
pub enum ComposerEvent {
    /// A prompt was sent (optimistically) — re-engage the transcript pin.
    Sent { chat_id: String, new_thread: bool },
    /// A generated review message reached (or failed at) the durable command RPC.
    GeneratedReviewFinished {
        review_id: String,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MentionToken {
    range: Range<usize>,
    query: String,
}

/// The `@` must begin a token. This intentionally excludes `name@example.com`
/// and ordinary words while allowing punctuation such as `(@src`.
fn mention_token(text: &str, cursor: usize) -> Option<MentionToken> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let token_start = text[..cursor]
        .char_indices()
        .rev()
        .find_map(|(at, ch)| ch.is_whitespace().then_some(at + ch.len_utf8()))
        .unwrap_or(0);
    let relative_at = text[token_start..cursor].rfind('@')?;
    let at = token_start + relative_at;
    let valid_boundary = at == 0
        || text[..at]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_whitespace() || matches!(ch, '(' | '[' | '{'));
    if text[at + 1..cursor].contains('@') || !valid_boundary {
        return None;
    }
    let end = text[cursor..]
        .char_indices()
        .find_map(|(at, ch)| ch.is_whitespace().then_some(cursor + at))
        .unwrap_or(text.len());
    Some(MentionToken {
        range: at..end,
        query: text[at + 1..cursor].to_string(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SlashCommandToken {
    range: Range<usize>,
    query: String,
}

/// Slash completion is intentionally limited to the first message token. A
/// slash elsewhere is prose, a path, or Markdown and keeps normal editing.
fn slash_command_token(text: &str, cursor: usize) -> Option<SlashCommandToken> {
    if !text.starts_with('/') || cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let end = text
        .char_indices()
        .find_map(|(at, ch)| ch.is_whitespace().then_some(at))
        .unwrap_or(text.len());
    if cursor > end {
        return None;
    }
    Some(SlashCommandToken {
        range: 0..end,
        query: text[1..cursor].to_string(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CommandCacheKey {
    harness: jolt_proto::HarnessId,
    target_device: String,
    cwd: String,
    model_options: String,
}

#[derive(Debug, Clone)]
struct CommandCacheEntry {
    catalog: Vec<AgentCommand>,
    fetched_at: Option<Instant>,
    failed_at: Option<Instant>,
    error: Option<SharedString>,
    last_used: Instant,
}

impl CommandCacheEntry {
    fn empty(now: Instant) -> Self {
        Self {
            catalog: Vec::new(),
            fetched_at: None,
            failed_at: None,
            error: None,
            last_used: now,
        }
    }
}

fn prune_command_cache(cache: &mut HashMap<CommandCacheKey, CommandCacheEntry>) {
    while cache.len() > COMMAND_CACHE_CAPACITY {
        let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        cache.remove(&oldest);
    }
}

#[derive(Debug, Clone, Default)]
struct SlashCommandState {
    token: Option<SlashCommandToken>,
    results: Vec<AgentCommand>,
    active: Option<usize>,
    request: u64,
    loading: bool,
    error: Option<SharedString>,
    notice: Option<SharedString>,
    cache_key: Option<CommandCacheKey>,
    dismissed: Option<(Range<usize>, String)>,
}

fn command_model_options_key(options: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut entries: Vec<_> = options.iter().collect();
    entries.sort_by_key(|(key, _)| *key);
    serde_json::to_string(&entries).unwrap_or_default()
}

fn command_cache_should_fetch(
    fetched_at: Option<Instant>,
    failed_at: Option<Instant>,
    now: Instant,
) -> bool {
    let stale = fetched_at.is_some_and(|at| now.saturating_duration_since(at) >= COMMAND_CACHE_TTL);
    let failed_recently =
        failed_at.is_some_and(|at| now.saturating_duration_since(at) < COMMAND_CACHE_FAILURE_RETRY);
    (fetched_at.is_none() || stale) && !failed_recently
}

const ANSWER_QUESTIONS_COMMAND: &str = "answer";
const BRO_COMMAND: &str = "bro";
const BRO_PROMPT: &str = "Restate your last message. Stop using jargon and speak coherently. State it more simply and concisely, like one human talking to another.";

fn is_goal_command(text: &str) -> bool {
    text.trim() == "/goal"
}

fn filtered_commands(catalog: &[AgentCommand], query: &str) -> Vec<AgentCommand> {
    let query = query.to_lowercase();
    let mut commands: Vec<_> = catalog
        .iter()
        .filter(|command| {
            command.source == AgentCommandSource::Jolt
                && (query.is_empty()
                    || command.name.to_lowercase().contains(&query)
                    || command
                        .description
                        .as_deref()
                        .is_some_and(|description| description.to_lowercase().contains(&query)))
        })
        .cloned()
        .collect();
    commands.sort_by_key(|command| {
        let name = command.name.to_lowercase();
        (!name.starts_with(&query), name)
    });
    commands
}

fn is_answer_questions_command(text: &str) -> bool {
    text.trim().strip_prefix('/') == Some(ANSWER_QUESTIONS_COMMAND)
}

fn is_bro_command(text: &str) -> bool {
    text.trim().strip_prefix('/') == Some(BRO_COMMAND)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageHistoryDirection {
    /// Move toward the start of the transcript.
    Older,
    /// Move toward the draft after the newest user message.
    Newer,
}

fn can_navigate_message_history(current: Option<usize>, composer_text: &str) -> bool {
    current.is_some() || composer_text.is_empty()
}

fn message_history_position(
    current: Option<usize>,
    message_count: usize,
    direction: MessageHistoryDirection,
) -> Option<usize> {
    if message_count == 0 {
        return None;
    }
    let current = current.map(|position| position.min(message_count - 1));
    match direction {
        MessageHistoryDirection::Older => Some(
            current
                .map_or(0, |position| position.saturating_add(1))
                .min(message_count - 1),
        ),
        MessageHistoryDirection::Newer => current.and_then(|position| position.checked_sub(1)),
    }
}

fn user_message_text(entry: &SessionMessageEntry) -> Option<String> {
    if entry.role != MessageRole::User {
        return None;
    }
    let raw = entry
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let text = attachments::parse_user_message_images(&raw).text;
    (!text.is_empty()).then_some(text)
}

fn user_message_history(
    transcript: &[SessionMessageEntry],
    pending_echoes: &[SessionMessageEntry],
) -> Vec<String> {
    transcript
        .iter()
        .chain(pending_echoes)
        .filter_map(user_message_text)
        .collect()
}

fn message_history_text(history: &[String], position: Option<usize>, draft: &str) -> String {
    position
        .and_then(|position| history.iter().rev().nth(position))
        .cloned()
        .unwrap_or_else(|| draft.to_string())
}

#[derive(Debug, Clone)]
struct BroRun {
    source_message_id: String,
    saw_live_run: bool,
}

#[derive(Debug, Clone, Default)]
struct FileMentionState {
    token: Option<MentionToken>,
    results: Vec<FileSearchMatch>,
    active: Option<usize>,
    request: u64,
    loading: bool,
    /// Why the last search failed, for the popup. A failure MUST NOT render
    /// as "No matching files": cross-device searches fail for reasons the
    /// user can act on (host daemon too old for `SearchFiles`, device
    /// offline), and the empty state hid them (user report).
    error: Option<SharedString>,
    /// Full token text, not just the cursor-relative query: moving within a
    /// dismissed token keeps it closed, while any edit re-enables completion.
    dismissed: Option<(Range<usize>, String)>,
}

fn mention_response_is_current(state: &FileMentionState, request: u64) -> bool {
    state.request == request && state.token.is_some()
}

/// A failed file search, translated for the popup. `UnknownMethod` is the
/// version-skew case: `SearchFiles` shipped after v0.1.9, so a session hosted
/// by a device on an older daemon answers "unknown method" while the same
/// search works for local sessions.
fn mention_error_message(err: &RpcError) -> SharedString {
    match err {
        RpcError::UnknownMethod(_) => {
            "The thread's device runs an older Jolt version — update it to search its files".into()
        }
        RpcError::Transport(_) | RpcError::Closed => "The thread's device is unreachable".into(),
        RpcError::BadParams(_) | RpcError::Failed(_) => "File search failed".into(),
    }
}

struct GoalDialog {
    objective: Entity<ComposerInput>,
    budget: Entity<ComposerInput>,
    goal_id: Option<String>,
    expected_revision: Option<u64>,
    tokens_used: u64,
    _objective_events: Subscription,
}

/// The checkout/ref toolbar has its own invalidation boundary. Composer edits
/// redraw the pill at typing cadence; the toolbar only changes when picker or
/// app state changes, so rebuilding its menus on every keystroke is wasted.
struct PickerFooter {
    pickers: Entity<Pickers>,
    _observe: Subscription,
}

/// Mode-specific sizing boundary for the shared picker state. Expanded rows
/// fill their available slot; compact rows must measure at intrinsic width.
struct PickerControls {
    pickers: Entity<Pickers>,
    fill_width: bool,
    _observe: Subscription,
}

impl PickerControls {
    fn new(pickers: Entity<Pickers>, fill_width: bool, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&pickers, |_, _, cx| cx.notify());
        Self {
            pickers,
            fill_width,
            _observe: observe,
        }
    }
}

impl Render for PickerControls {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let fill_width = self.fill_width;
        self.pickers.update(cx, |pickers, cx| {
            pickers.render_controls(fill_width, window, cx)
        })
    }
}

/// The context-window wheel has its own render boundary so it can sit beside
/// the picker entity without changing that entity's intrinsic width.
struct PickerUsage {
    pickers: Entity<Pickers>,
    _observe: Subscription,
}

impl PickerUsage {
    fn new(pickers: Entity<Pickers>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&pickers, |_, _, cx| cx.notify());
        Self {
            pickers,
            _observe: observe,
        }
    }
}

impl Render for PickerUsage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.pickers
            .update(cx, |pickers, cx| pickers.render_usage(cx))
    }
}

impl PickerFooter {
    fn new(pickers: Entity<Pickers>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&pickers, |_, _, cx| cx.notify());
        Self {
            pickers,
            _observe: observe,
        }
    }
}

impl Render for PickerFooter {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.pickers
            .update(cx, |pickers, cx| pickers.render_footer(cx))
            .unwrap_or_else(|| gpui::Empty.into_any_element())
    }
}

pub struct Composer {
    state: Entity<AppState>,
    input: Entity<ComposerInput>,
    /// Composer actions row: repo/branch/harness-model/traits (§1.7).
    pickers: Entity<Pickers>,
    compact_picker_controls: Entity<PickerControls>,
    expanded_picker_controls: Entity<PickerControls>,
    /// Context-window wheel rendered immediately after the model/thinking picker.
    picker_usage: Entity<PickerUsage>,
    picker_footer: Entity<PickerFooter>,
    /// Draft text per chat key ("" = new-chat canvas), surviving navigation.
    drafts: HashMap<String, String>,
    /// Offset from the newest sent user message; `None` is the draft below
    /// the newest message.
    message_history_position: Option<usize>,
    /// Text expected while a recalled message is untouched. Any edit leaves
    /// history navigation and turns the recalled prompt into a fresh draft.
    message_history_text: Option<String>,
    /// Unsent text captured before entering history, restored at the bottom.
    message_history_draft: Option<String>,
    /// Staged-but-unsent attachments per chat key. Navigating away and back
    /// restores them; they remain memory-only.
    attachments: HashMap<String, Vec<StagedAttachment>>,
    /// The staged attachment being viewed full-size (click a thumbnail).
    preview: Option<attachments::PreviewImage>,
    /// In-flight file-picker prompt (paperclip).
    picker_task: Option<Task<()>>,
    mention_task: Option<Task<()>>,
    mention: FileMentionState,
    command_task: Option<Task<()>>,
    command: SlashCommandState,
    command_cache: HashMap<CommandCacheKey, CommandCacheEntry>,
    command_scroll: gpui::ScrollHandle,
    current_key: String,
    goal_expanded: bool,
    goal_dialog: Option<GoalDialog>,
    sending: bool,
    failure: Option<SharedString>,
    wizard: Option<Wizard>,
    /// User-invoked extraction and free-text answer flow.
    extracted_answers: Option<ExtractedAnswerFlow>,
    /// In-progress extracted answers survive session navigation like drafts.
    extracted_answer_stash: HashMap<String, ExtractedAnswerFlow>,
    extraction_task: Option<Task<()>>,
    extraction_notice: Option<SharedString>,
    /// Hidden `/bro` control turns, retained across conversation navigation.
    bro_runs: HashMap<String, BroRun>,
    wizard_focus: FocusHandle,
    wizard_scroll: gpui::ScrollHandle,
    wizard_options_scroll: gpui::ScrollHandle,
    /// Requests already answered locally (suppresses the panel until the doc
    /// frame marks them resolved).
    answered_requests: HashSet<String>,
    advance_task: Option<Task<()>>,
    send_task: Option<Task<()>>,
    /// Serializes detached Cmd/Ctrl+Enter sends so attachment upload latency
    /// cannot reorder queue entries.
    queue_send_lock: Arc<tokio::sync::Mutex<()>>,
    // -- compact/expanded flip state (hysteresis; see `composer_flip`) --
    /// Current layout mode (persisted across frames — never derived fresh).
    expanded_mode: bool,
    /// `layout_epoch` of the measurement that caused the last flip: the flip is
    /// re-evaluated only after the input has been laid out in the new mode, so
    /// at most one flip can happen per layout pass.
    flip_epoch: u64,
    /// Compact-mode input capacity, learned while compact (layout-stable).
    compact_capacity: f32,
    /// Input width first measured after expanding — container-width deltas
    /// while expanded shift `compact_capacity` by the same amount.
    expanded_anchor: f32,
    /// Last input width seen in the current mode (resize detection).
    last_seen_width: f32,
    /// Set while an interactive resize is in flight; mode is frozen until
    /// widths have settled for [`RESIZE_SETTLE_MS`].
    width_changed_at: Option<Instant>,
    settle_task: Option<Task<()>>,
    /// In-flight compact↔expanded morph (one per committed flip; manual
    /// drive — see [`FlipMorph`]).
    flip_morph: Option<FlipMorph>,
    /// Pill height actually rendered last frame — a committed flip morphs
    /// from here, so mid-flight reversals hand off without a jump.
    last_rendered_height: f32,
    /// Monotonic clock anchor for the morph timeline.
    morph_clock: Instant,
    /// Set on every session/route change: flips committed before this instant
    /// SNAP instead of morphing (see [`ROUTE_SNAP_MS`]).
    route_snap_until: Option<Instant>,
    _observe: Subscription,
    _input_events: Subscription,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            let mut input = ComposerInput::new(DEFAULT_PLACEHOLDER, cx);
            input.enable_prompt_typography();
            input.enable_mentions();
            input.enable_message_history();
            input.enable_context_menu();
            input
        });
        let pickers = cx.new(|cx| Pickers::new(state.clone(), cx));
        let compact_picker_controls = cx.new(|cx| PickerControls::new(pickers.clone(), false, cx));
        let expanded_picker_controls = cx.new(|cx| PickerControls::new(pickers.clone(), true, cx));
        let picker_usage = cx.new(|cx| PickerUsage::new(pickers.clone(), cx));
        let picker_footer = cx.new(|cx| PickerFooter::new(pickers.clone(), cx));
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.on_state_changed(cx));
        let input_events = cx.subscribe(&input, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted => this.on_submit(cx),
            ComposerInputEvent::QueueSubmitted => this.on_queue_submit(cx),
            ComposerInputEvent::Edited => {
                this.leave_message_history_after_edit(cx);
                this.on_input_edited(cx);
            }
            ComposerInputEvent::CursorMoved => this.on_input_edited(cx),
            ComposerInputEvent::ViewportChanged => cx.notify(),
            ComposerInputEvent::MessageHistoryNavigate(direction) => {
                this.navigate_message_history(*direction, cx)
            }
            ComposerInputEvent::MentionNavigate(delta) => this.move_mention(*delta, cx),
            ComposerInputEvent::MentionAccept => this.accept_mention(cx),
            ComposerInputEvent::MentionDismiss => this.dismiss_mention(cx),
            ComposerInputEvent::PastedImages(images) => {
                let staged = images
                    .iter()
                    .map(|image| attachments::stage_clipboard_image(image.clone()))
                    .collect();
                this.add_staged(staged, cx);
            }
            ComposerInputEvent::PastedPaths(paths) => this.add_paths(paths.clone(), cx),
        });
        let current_key = state.read(cx).selected_chat.clone().unwrap_or_default();
        let mut composer = Self {
            state,
            input,
            pickers,
            compact_picker_controls,
            expanded_picker_controls,
            picker_usage,
            picker_footer,
            drafts: HashMap::new(),
            message_history_position: None,
            message_history_text: None,
            message_history_draft: None,
            attachments: HashMap::new(),
            preview: None,
            picker_task: None,
            mention_task: None,
            mention: FileMentionState::default(),
            command_task: None,
            command: SlashCommandState::default(),
            command_cache: HashMap::new(),
            command_scroll: gpui::ScrollHandle::new(),
            current_key,
            goal_expanded: false,
            goal_dialog: None,
            sending: false,
            failure: None,
            wizard: None,
            extracted_answers: None,
            extracted_answer_stash: HashMap::new(),
            extraction_task: None,
            extraction_notice: None,
            bro_runs: HashMap::new(),
            wizard_focus: cx.focus_handle(),
            wizard_scroll: gpui::ScrollHandle::new(),
            wizard_options_scroll: gpui::ScrollHandle::new(),
            answered_requests: HashSet::new(),
            advance_task: None,
            send_task: None,
            queue_send_lock: Arc::new(tokio::sync::Mutex::new(())),
            expanded_mode: false,
            flip_epoch: 0,
            compact_capacity: 0.0,
            expanded_anchor: 0.0,
            last_seen_width: 0.0,
            width_changed_at: None,
            settle_task: None,
            flip_morph: None,
            last_rendered_height: 0.0,
            morph_clock: Instant::now(),
            route_snap_until: None,
            _observe: observe,
            _input_events: input_events,
        };
        // Dev knob: pre-stage attachments (drop/paste can't be synthesized on
        // a rig) — `JOLT_ATTACH=/path/a.png[,/path/b.png]`, and
        // `JOLT_ATTACH_PREVIEW=1` boots with the first one's lightbox open.
        if let Ok(spec) = std::env::var("JOLT_ATTACH") {
            let staged: Vec<StagedAttachment> = spec
                .split(',')
                .filter(|s| !s.trim().is_empty())
                .filter_map(|path| {
                    match attachments::stage_file(std::path::Path::new(path.trim())) {
                        Ok(att) => Some(att),
                        Err(err) => {
                            tracing::warn!(%path, error = %err, "JOLT_ATTACH stage failed");
                            None
                        }
                    }
                })
                .collect();
            if std::env::var("JOLT_ATTACH_PREVIEW").is_ok_and(|v| v == "1")
                && let Some(first) = staged.first()
            {
                composer.preview = Some(attachments::PreviewImage {
                    name: first.name.clone().into(),
                    image: first.image.clone(),
                });
            }
            if !staged.is_empty() {
                composer
                    .attachments
                    .entry(composer.current_key.clone())
                    .or_default()
                    .extend(staged);
            }
        }
        composer.sync_default_placeholder(cx);
        composer
    }

    /// Capture-knob passthrough (`JOLT_OPEN_DIALOG=model`): open the
    /// combined harness/model menu.
    pub fn debug_open_model_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pickers
            .update(cx, |pickers, cx| pickers.open_model_menu(window, cx));
    }

    pub fn is_sending(&self) -> bool {
        self.sending
    }

    /// Whether the composer already renders progress for its active workflow.
    /// Shell chrome uses this to avoid duplicating status feedback; future
    /// composer-owned workflows opt in here without coupling the shell to them.
    pub fn has_inline_progress(&self) -> bool {
        self.bro_active()
            || matches!(
                self.extracted_answers.as_ref().map(|flow| &flow.state),
                Some(ExtractedAnswerState::Extracting { .. })
            )
    }

    // ---- attachment staging ----

    /// Staged attachments for the chat the composer is showing.
    /// Ask the active harness to restate its latest response in plain language.
    /// The control prompt reaches the harness but never becomes a user bubble.
    fn start_bro(&mut self, cx: &mut Context<Self>) {
        self.extraction_notice = None;
        if !self.staged().is_empty() {
            self.extraction_notice = Some("Remove attachments before using /bro.".into());
            cx.notify();
            return;
        }
        let (engine, chat_id, cwd, source_message_id) = {
            let state = self.state.read(cx);
            let Some(engine) = state.engine().cloned() else {
                self.failure = Some("Engine not connected".into());
                cx.notify();
                return;
            };
            let Some(chat) = state.selected_chat_row() else {
                self.extraction_notice = Some("Select a thread first.".into());
                cx.notify();
                return;
            };
            let Some((source_message_id, _)) = latest_answerable_message(&state.transcript) else {
                self.extraction_notice =
                    Some("There is no completed assistant response to restate.".into());
                cx.notify();
                return;
            };
            (
                engine,
                chat.id.clone(),
                chat.cwd.clone().unwrap_or_else(|| ".".into()),
                source_message_id,
            )
        };
        let resolved = self.pickers.read(cx).resolved(cx);
        self.bro_runs.insert(
            chat_id.clone(),
            BroRun {
                source_message_id: source_message_id.clone(),
                saw_live_run: false,
            },
        );
        self.failure = None;
        self.sending = true;
        cx.emit(ComposerEvent::Sent {
            chat_id: chat_id.clone(),
            new_thread: false,
        });
        cx.notify();

        let command = SessionCommandPayload::HiddenPrompt {
            request: RunRequest {
                prompt: BRO_PROMPT.into(),
                harness: resolved.harness,
                model: resolved.model.clone(),
                reasoning: resolved.reasoning,
                model_options: resolved.model_options.clone(),
                cwd,
                sandbox: SandboxLevel::WorkspaceWrite,
                auto_approve: false,
                resume: None,
                attachments: Vec::new(),
            },
        };
        self.send_task = Some(cx.spawn(async move |this, cx| {
            let result = async {
                call_api(
                    engine.client(),
                    &QueueCommand {
                        chat_id: chat_id.clone(),
                        command,
                        target_device_id: None,
                    },
                )
                .await
                .map_err(|error| format!("Send failed: {error}"))?;
                Ok::<_, String>(())
            }
            .await;
            this.update(cx, |composer, cx| {
                composer.sending = false;
                if let Err(message) = result {
                    if composer
                        .bro_runs
                        .get(&chat_id)
                        .is_some_and(|run| run.source_message_id == source_message_id)
                    {
                        composer.bro_runs.remove(&chat_id);
                    }
                    composer.failure = Some(message.into());
                }
                composer.sync_default_placeholder(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Start the native equivalent of the local Pi `/answer` extension.
    fn start_answer_questions(&mut self, cx: &mut Context<Self>) {
        self.extraction_notice = None;
        if self.sending
            || self.run_live(cx)
            || self.wizard.is_some()
            || self.extracted_answers.is_some()
        {
            self.extraction_notice = Some("Wait for the current interaction to finish.".into());
            cx.notify();
            return;
        }
        if !self.input.read(cx).text().trim().is_empty() || !self.staged().is_empty() {
            self.extraction_notice =
                Some("Send or clear the current draft before answering questions.".into());
            cx.notify();
            return;
        }
        let (engine, chat_id, host_device_id, source_message_id) = {
            let state = self.state.read(cx);
            let Some(engine) = state.engine().cloned() else {
                self.failure = Some("Engine not connected".into());
                cx.notify();
                return;
            };
            let Some(chat) = state.selected_chat_row() else {
                self.extraction_notice = Some("Select a thread first.".into());
                cx.notify();
                return;
            };
            let Some((source_message_id, _)) = latest_answerable_message(&state.transcript) else {
                self.extraction_notice =
                    Some("There is no completed assistant response to inspect.".into());
                cx.notify();
                return;
            };
            (
                engine,
                chat.id.clone(),
                chat.device_id.clone(),
                source_message_id,
            )
        };

        self.reset_mention(None, cx);
        self.input.update(cx, |input, cx| {
            input.set_text("", cx);
            input.set_placeholder("Preparing questions…", cx);
        });
        self.extracted_answers = Some(ExtractedAnswerFlow {
            chat_id: chat_id.clone(),
            state: ExtractedAnswerState::Extracting {
                source_message_id: source_message_id.clone(),
            },
        });
        let request = ExtractQuestions {
            chat_id,
            source_message_id,
            target_device_id: Some(host_device_id),
        };
        self.extraction_task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(engine.client(), &request).await;
            this.update(cx, |composer, cx| {
                composer.extraction_task = None;
                let expected =
                    composer
                        .extracted_answers
                        .as_ref()
                        .and_then(|flow| match &flow.state {
                            ExtractedAnswerState::Extracting { source_message_id } => {
                                Some((flow.chat_id.as_str(), source_message_id.as_str()))
                            }
                            ExtractedAnswerState::Answering(_) => None,
                        });
                let Some((expected_chat, expected_source)) = expected else {
                    return;
                };
                if composer.current_key != expected_chat {
                    composer.close_extracted_answers(cx);
                    return;
                }
                match result {
                    Ok(result) if result.source_message_id == expected_source => {
                        let still_latest =
                            latest_answerable_message(&composer.state.read(cx).transcript)
                                .is_some_and(|(id, _)| id == result.source_message_id);
                        if !still_latest {
                            composer.close_extracted_answers(cx);
                            composer.extraction_notice =
                                Some("A newer assistant response arrived. Try again.".into());
                        } else if result.questions.is_empty() {
                            composer.close_extracted_answers(cx);
                            composer.extraction_notice =
                                Some("No questions requiring an answer were found.".into());
                        } else {
                            if let Some(flow) = composer.extracted_answers.as_mut() {
                                flow.state = ExtractedAnswerState::Answering(ExtractedWizard::new(
                                    result.questions,
                                ));
                            }
                            composer.wizard_scroll.set_offset(point(px(0.0), px(0.0)));
                            composer.input.update(cx, |input, cx| {
                                input.set_text("", cx);
                                input.set_placeholder("Type your answer…", cx);
                            });
                        }
                    }
                    Ok(_) => {
                        composer.close_extracted_answers(cx);
                        composer.failure = Some("Question extraction became stale.".into());
                    }
                    Err(error) => {
                        composer.close_extracted_answers(cx);
                        composer.failure =
                            Some(format!("Question extraction failed: {error}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn close_extracted_answers(&mut self, cx: &mut Context<Self>) {
        self.extracted_answers = None;
        self.extraction_task = None;
        self.input.update(cx, |input, cx| input.set_text("", cx));
        self.sync_default_placeholder(cx);
    }

    fn extracted_back(&mut self, cx: &mut Context<Self>) {
        let current = self.input.read(cx).text().to_string();
        let previous = self.extracted_answers.as_mut().and_then(|flow| {
            let ExtractedAnswerState::Answering(wizard) = &mut flow.state else {
                return None;
            };
            wizard.save(current);
            wizard.back().then(|| wizard.current_answer().to_string())
        });
        if let Some(previous) = previous {
            self.wizard_scroll.set_offset(point(px(0.0), px(0.0)));
            self.input
                .update(cx, |input, cx| input.set_text(previous, cx));
            cx.notify();
        }
    }

    fn extracted_advance(&mut self, cx: &mut Context<Self>) {
        let current = self.input.read(cx).text().to_string();
        let outcome = self.extracted_answers.as_mut().and_then(|flow| {
            let ExtractedAnswerState::Answering(wizard) = &mut flow.state else {
                return None;
            };
            wizard.save(current);
            if wizard.advance() {
                Some(Ok(wizard.compiled_message()))
            } else {
                Some(Err(wizard.current_answer().to_string()))
            }
        });
        match outcome {
            Some(Ok(message)) => {
                self.close_extracted_answers(cx);
                self.send(message, SendIntent::Run, SubmissionOrigin::Editor, cx);
            }
            Some(Err(next)) => {
                self.wizard_scroll.set_offset(point(px(0.0), px(0.0)));
                self.input.update(cx, |input, cx| input.set_text(next, cx));
                cx.notify();
            }
            None => {}
        }
    }

    fn on_state_changed(&mut self, cx: &mut Context<Self>) {
        let (key, pending) = {
            let s = self.state.read(cx);
            (
                s.selected_chat.clone().unwrap_or_default(),
                pending_input_request(&s.transcript),
            )
        };

        // A real agent input request takes priority over the optional prose
        // extraction flow. Unlike route navigation, this cancels rather than
        // stashes because the live run is blocked on its own question.
        if pending.is_some() && self.extracted_answers.is_some() {
            self.close_extracted_answers(cx);
        }
        if pending.is_some() {
            self.bro_runs.remove(&key);
        }

        // Draft swap on chat navigation — the input entity itself survives.
        if key != self.current_key {
            if let Some(mut flow) = self.extracted_answers.take() {
                match &mut flow.state {
                    ExtractedAnswerState::Answering(wizard) => {
                        wizard.save(self.input.read(cx).text().to_string());
                        self.extracted_answer_stash
                            .insert(self.current_key.clone(), flow);
                    }
                    ExtractedAnswerState::Extracting { .. } => {
                        self.extraction_task = None;
                    }
                }
                self.input.update(cx, |input, cx| input.set_text("", cx));
            }
            let old_text = self
                .message_history_draft
                .take()
                .unwrap_or_else(|| self.input.read(cx).text().to_string());
            if old_text.is_empty() {
                self.drafts.remove(&self.current_key);
            } else {
                self.drafts.insert(self.current_key.clone(), old_text);
            }
            let draft = self.drafts.get(&key).cloned().unwrap_or_default();
            self.current_key = key;
            self.message_history_position = None;
            self.message_history_text = None;
            self.message_history_draft = None;
            self.failure = None;
            self.extraction_notice = None;
            self.wizard = None;
            // Attachments stay stashed under their chat key (the map swap IS
            // the navigation); only the transient chrome resets.
            self.preview = None;
            self.reset_mention(None, cx);
            // Route changes snap (round 5/6): a mode difference between the
            self.reset_command(cx);
            // old and new session's composer must not glide across
            // navigation. Killing the in-flight morph here isn't enough —
            // the nav-driven flip only commits AFTER the swapped draft has
            // been re-measured, one or two renders later, so the whole
            // window snaps (see ROUTE_SNAP_MS).
            self.flip_morph = None;
            self.last_rendered_height = 0.0;
            self.route_snap_until = Some(Instant::now() + Duration::from_millis(ROUTE_SNAP_MS));
            self.input.update(cx, |input, cx| input.set_text(draft, cx));
            if pending.is_none()
                && let Some(flow) = self.extracted_answer_stash.remove(&self.current_key)
            {
                let answer = match &flow.state {
                    ExtractedAnswerState::Answering(wizard) => wizard.current_answer().to_string(),
                    ExtractedAnswerState::Extracting { .. } => String::new(),
                };
                self.extracted_answers = Some(flow);
                self.wizard_scroll.set_offset(point(px(0.0), px(0.0)));
                self.input.update(cx, |input, cx| {
                    input.set_text(answer, cx);
                    input.set_placeholder("Type your answer…", cx);
                });
            }
        }

        // A hidden control turn stays in the composer until its replacement
        // response completes (or its live run settles without one). Tracking
        // both signals avoids flicker around the Working status transition.
        let (latest_assistant_id, live_runs) = {
            let state = self.state.read(cx);
            let now = chrono::Utc::now();
            (
                latest_answerable_message(&state.transcript).map(|(id, _)| id),
                self.bro_runs
                    .keys()
                    .map(|chat_id| {
                        let live = matches!(
                            state.indicator_for(chat_id, now),
                            Indicator::Working | Indicator::AwaitingInput
                        );
                        (chat_id.clone(), live)
                    })
                    .collect::<Vec<_>>(),
            )
        };
        let mut finished = Vec::new();
        for (chat_id, live) in live_runs {
            let Some(run) = self.bro_runs.get_mut(&chat_id) else {
                continue;
            };
            run.saw_live_run |= live;
            let response_arrived = chat_id == self.current_key
                && latest_assistant_id
                    .as_deref()
                    .is_some_and(|id| id != run.source_message_id);
            if response_arrived || (run.saw_live_run && !live) {
                finished.push(chat_id);
            }
        }
        for chat_id in finished {
            self.bro_runs.remove(&chat_id);
        }

        // Question panel lifecycle (wizard state cached per request id).
        match pending {
            Some((request_id, questions)) if !self.answered_requests.contains(&request_id) => {
                let same = self
                    .wizard
                    .as_ref()
                    .is_some_and(|w| w.request_id == request_id);
                if !same {
                    self.reset_mention(None, cx);
                    self.wizard = Some(Wizard::new(request_id, questions));
                    self.wizard_scroll.set_offset(point(px(0.0), px(0.0)));
                    self.wizard_options_scroll
                        .set_offset(point(px(0.0), px(0.0)));
                    self.advance_task = None;
                    // The shared input becomes the panel's free-text override.
                    self.input.update(cx, |input, cx| {
                        input.set_placeholder("Type your own answer, or pick an option above", cx)
                    });
                }
            }
            _ => {
                if let Some(wizard) = self.wizard.as_ref() {
                    // Keep the panel latched through a transient fold/sync blip
                    // or a steer appended behind the
                    // streaming entry — must not unmount the panel and lose
                    // the user's picks. Release only on explicit resolution
                    // (here or on another device) or when a NON-EMPTY
                    // transcript shows the question superseded (a newer
                    // assistant entry took over). Never on run death: the
                    // question stays answerable until answered — the engine
                    // delivers a dead run's answer as a resumed turn.
                    let transcript = self.state.read(cx).transcript.clone();
                    let released = input_request_resolved(&transcript, &wizard.request_id)
                        || (!transcript.is_empty()
                            && !self.answered_requests.contains(&wizard.request_id));
                    if released {
                        self.wizard = None;
                        self.advance_task = None;
                    }
                }
            }
        }
        self.sync_default_placeholder(cx);
        cx.notify();
    }

    fn sync_default_placeholder(&self, cx: &mut Context<Self>) {
        if self.wizard.is_some() || self.extracted_answers.is_some() || self.bro_active() {
            return;
        }
        let placeholder = composer_placeholder(self.sending || self.run_live(cx));
        self.input
            .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
    }

    fn bro_active(&self) -> bool {
        self.bro_runs.contains_key(&self.current_key)
    }

    fn run_live(&self, cx: &App) -> bool {
        let s = self.state.read(cx);
        let Some(chat_id) = s.selected_chat.as_deref() else {
            return false;
        };
        matches!(
            s.indicator_for(chat_id, chrono::Utc::now()),
            Indicator::Working | Indicator::AwaitingInput
        )
    }

    fn button_mode(&self, cx: &App) -> SendButtonMode {
        // A staged image counts as content: image-only sends are legal
        // (the prompt body becomes "See the attached image(s).").
        let has_text = !self.input.read(cx).text().trim().is_empty() || !self.staged().is_empty();
        send_button_mode(self.run_live(cx), has_text)
    }

    /// Copy an active text selection, or clear the current session's composer.
    pub fn clear_input(&mut self, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            // Clear input and Copy share mod-c by default. The app-level clear
            // action wins dispatch, so preserve normal copy behavior here.
            if !input.copy_selected_text(cx) {
                input.set_text("", cx);
            }
        });
    }

    fn on_submit(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.extracted_answers.as_ref().map(|flow| &flow.state),
            Some(ExtractedAnswerState::Answering(_))
        ) {
            self.extracted_advance(cx);
            return;
        }
        if self.wizard.is_some() {
            // Enter inside the panel's free-text input submits the page.
            self.wizard_advance(cx);
            return;
        }
        let text = self.input.read(cx).text().trim().to_string();
        if is_goal_command(&text) {
            self.open_goal_dialog(cx);
            return;
        }
        match self.button_mode(cx) {
            SendButtonMode::Stop => self.interrupt(cx),
            _ if text.is_empty() && self.staged().is_empty() => {}
            SendButtonMode::Send if is_answer_questions_command(&text) => {
                self.reset_command(cx);
                self.input.update(cx, |input, cx| input.set_text("", cx));
                self.drafts.remove(&self.current_key);
                self.start_answer_questions(cx);
            }
            SendButtonMode::Send if is_bro_command(&text) => {
                self.reset_command(cx);
                self.input.update(cx, |input, cx| input.set_text("", cx));
                self.drafts.remove(&self.current_key);
                self.start_bro(cx);
            }
            SendButtonMode::Send => self.send(text, SendIntent::Run, SubmissionOrigin::Editor, cx),
            SendButtonMode::Steer => {
                self.send(text, SendIntent::Steer, SubmissionOrigin::Editor, cx)
            }
        }
    }

    fn on_queue_submit(&mut self, cx: &mut Context<Self>) {
        if self.wizard.is_some()
            || self.extracted_answers.is_some()
            || !self.run_live(cx)
            || shell_scope(self.input.read(cx).text()).is_some()
        {
            self.on_submit(cx);
            return;
        }
        let text = self.input.read(cx).text().trim().to_string();
        if text.is_empty() && self.staged().is_empty() {
            return;
        }
        self.send(text, SendIntent::Queue, SubmissionOrigin::Editor, cx);
    }

    /// Submit generated review feedback through the ordinary user-message path
    /// without reading or mutating the visible composer draft or attachments.
    pub fn submit_generated_review(
        &mut self,
        review_id: String,
        chat_id: String,
        text: String,
        cx: &mut Context<Self>,
    ) {
        if self.state.read(cx).selected_chat.as_deref() != Some(chat_id.as_str()) {
            cx.emit(ComposerEvent::GeneratedReviewFinished {
                review_id,
                error: Some("The reviewed chat is no longer selected".into()),
            });
            return;
        }
        if self.state.read(cx).engine().is_none() {
            cx.emit(ComposerEvent::GeneratedReviewFinished {
                review_id,
                error: Some("Engine not connected".into()),
            });
            return;
        }
        let intent = if self.run_live(cx) {
            SendIntent::Steer
        } else {
            SendIntent::Run
        };
        self.send(
            text,
            intent,
            SubmissionOrigin::GeneratedReview { review_id },
            cx,
        );
    }

    /// Queue a Run, Steer, or deferred-turn doc command. Agent prompts get an
    /// optimistic echo; direct shell output appears when execution completes.
    /// New chats thread the picked config in: worktree creation (when the isolated toggle
    /// is on), `Mutate createChat` with the `ChatConfig` + cwd, and the model /
    /// reasoning / options on the Run request itself (§1.7).
    fn send(
        &mut self,
        text: String,
        intent: SendIntent,
        origin: SubmissionOrigin,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.failure = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        // Chat id: existing selection, or client-minted for the new-chat canvas
        // (the chat then appears from the doc host once the doc materializes).
        let (chat_id, is_new) = match self.state.read(cx).selected_chat.clone() {
            Some(id) => (id, false),
            None => (uuid::Uuid::new_v4().to_string(), true),
        };
        // Where the new session runs (Current checkout / reuse an existing
        // worktree / fresh worktree off the picked base) — resolved NOW so
        // the async block needs no picker access.
        let plan = self.pickers.read(cx).checkout_plan();
        // Fully-resolved model/reasoning/options — concrete values (chat config
        // or defaults), so the engine never has to guess a "default".
        let resolved = self.pickers.read(cx).resolved(cx);
        let scope = shell_scope(&text);
        let shell = shell_command(&text);
        if scope.is_some() && shell.is_none() {
            self.failure = Some("Enter a Bash command after ! or !!".into());
            cx.notify();
            return;
        }
        if shell.is_some() && !self.staged().is_empty() {
            self.failure = Some("Send or remove attachments before running a shell command".into());
            cx.notify();
            return;
        }
        let is_shell = shell.is_some();
        let editor_submission = origin.uses_editor_state();
        let generated_review_id = match &origin {
            SubmissionOrigin::Editor => None,
            SubmissionOrigin::GeneratedReview { review_id } => Some(review_id.clone()),
        };
        let steer_cmd = intent == SendIntent::Steer && !is_new && !is_shell;
        let queue_cmd = intent == SendIntent::Queue && !is_new && !is_shell;
        let existing_cwd = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.cwd.clone());
        // The SPACE fixes the new chat's device + base folder — this is the
        // behavioral core of spaces: sessions are minted onto the space's
        // device, not necessarily this one.
        let space = self.state.read(cx).selected_space_row().cloned();
        if is_new && space.is_none() {
            self.failure = Some("Add a space first".into());
            cx.notify();
            return;
        }
        let local_device_id = self.state.read(cx).local_device_id.clone();
        let device_id = if is_new {
            space
                .as_ref()
                .map(|s| s.device_id.clone())
                .unwrap_or_else(|| "local".to_string())
        } else {
            self.state
                .read(cx)
                .selected_chat_row()
                .map(|c| c.device_id.clone())
                .or_else(|| local_device_id.clone())
                .unwrap_or_else(|| "local".to_string())
        };
        // Uploads/read-backs target the chat's HOST device (forwardable RPCs);
        // for a new chat that's the space's device (None when it's local).
        let host_device_id = if is_new {
            space
                .as_ref()
                .map(|s| s.device_id.clone())
                .filter(|id| local_device_id.as_deref() != Some(id.as_str()))
        } else {
            self.state
                .read(cx)
                .selected_chat_row()
                .map(|c| c.device_id.clone())
        };
        let space_id = space.as_ref().map(|s| s.id.clone());
        let space_path = space.as_ref().map(|s| s.path.clone());
        let space_remote = space
            .as_ref()
            .is_some_and(|s| local_device_id.as_deref() != Some(s.device_id.as_str()));
        // Snapshot and clear now: the strip empties the instant you hit send;
        // a failure hands the files
        // back into the chat's stash.
        let staged = if is_shell || !editor_submission {
            Vec::new()
        } else {
            self.attachments
                .remove(&self.current_key)
                .unwrap_or_default()
        };
        if !is_shell && editor_submission {
            self.preview = None;
        }
        let message_id = uuid::Uuid::new_v4().to_string();
        let sent_at = chrono::Utc::now();
        let created_at = sent_at.timestamp_millis();

        // Image-only sends echo the same body `with_uploaded_attachments` will use, so
        // the bubble never renders empty (refs are upserted in post-upload).
        let echo_text = if text.is_empty() && !staged.is_empty() {
            attachments::ATTACHMENT_ONLY_TEXT.to_string()
        } else {
            text.clone()
        };

        // Optimistic echo (client-minted id doubles as the persisted message id,
        // so the doc frame dedups it away). Shell commands appear immediately
        // as streaming system entries while their output is still pending.
        let echo = match &shell {
            Some(shell) => SessionMessageEntry {
                id: message_id.clone(),
                role: MessageRole::System,
                parts: vec![MessagePart::Text {
                    id: "t0".into(),
                    text: bash_pending_transcript(&shell.command),
                }],
                created_at,
                device_id: "local".into(),
                status: Some(MessageStatus::Streaming),
                continuation_of: None,
            },
            None => SessionMessageEntry {
                id: message_id.clone(),
                role: MessageRole::User,
                parts: vec![MessagePart::Text {
                    id: "t0".into(),
                    text: echo_text.clone(),
                }],
                created_at,
                device_id: "local".into(),
                status: None,
                continuation_of: None,
            },
        };
        if !queue_cmd {
            self.state.update(cx, |s, cx| {
                if is_new {
                    s.select_chat(Some(chat_id.clone()), cx);
                }
                s.push_echo(&chat_id, echo);
                s.begin_pending_send(&chat_id, &message_id, sent_at);
                cx.notify();
            });
            let expiry_chat_id = chat_id.clone();
            let expiry_message_id = message_id.clone();
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(PENDING_SEND_TTL_MS as u64))
                    .await;
                this.update(cx, |composer, cx| {
                    composer.state.update(cx, |state, cx| {
                        state.end_pending_send(&expiry_chat_id, &expiry_message_id);
                        cx.notify();
                    });
                })
                .ok();
            })
            .detach();
        }

        if editor_submission {
            self.input.update(cx, |input, cx| input.set_text("", cx));
            self.drafts.remove(&self.current_key);
        }
        self.failure = None;
        self.sending = true;
        self.sync_default_placeholder(cx);
        cx.emit(ComposerEvent::Sent {
            chat_id: chat_id.clone(),
            new_thread: is_new,
        });
        cx.notify();

        let restore_text = editor_submission.then(|| text.clone());
        let err_chat_id = chat_id.clone();
        let err_message_id = message_id.clone();
        let queue_send_lock = self.queue_send_lock.clone();
        let send_task = cx.spawn(async move |this, cx| {
            let _queue_order = if queue_cmd {
                Some(queue_send_lock.lock().await)
            } else {
                None
            };
            let result: Result<(), String> = async {
                // Resolve the working directory: existing chats keep theirs;
                // new chats run per the checkout plan (t3code env-mode): the
                // space's folder as-is, an EXISTING worktree of the picked ref
                // (a plain cwd override — multiple sessions share one
                // worktree), or a fresh isolated worktree created off the
                // picked base ref (CreateWorktree on send, targeted at the
                // space's device; the RPC relay-forwards).
                let mut cwd = if is_new {
                    space_path.clone()
                } else {
                    existing_cwd
                }
                .unwrap_or_else(|| ".".to_string());
                let mut worktree_cwd: Option<String> = None;
                // The picked ref rides createChat so the session footer names
                // it from the first frame (it read "Select ref" until the
                // host's diff reconciler got around to stamping the branch).
                let mut chat_branch: Option<String> = None;
                if is_new {
                    match &plan {
                        crate::pickers::CheckoutPlan::CurrentCheckout { branch } => {
                            chat_branch = branch.clone();
                        }
                        crate::pickers::CheckoutPlan::ReuseWorktree { path, branch } => {
                            cwd = path.clone();
                            worktree_cwd = Some(path.clone());
                            chat_branch = Some(branch.clone());
                        }
                        crate::pickers::CheckoutPlan::NewWorktree { base } => {
                            chat_branch = base.clone();
                            if let (Some(repo_path), Some(base)) = (&space_path, base) {
                                let worktree = call_api(
                                    engine.client(),
                                    &CreateWorktree {
                                        repo_path: repo_path.clone(),
                                        branch: base.clone(),
                                        target_device_id: space_remote
                                            .then(|| device_id.clone()),
                                    },
                                )
                                .await
                                .map_err(|e| format!("Worktree failed: {e}"))?;
                                cwd = worktree.path.clone();
                                worktree_cwd = Some(worktree.path);
                                chat_branch = Some(worktree.branch);
                            }
                        }
                    }
                }

                // Best-effort Mutate createChat with the picked config: the
                // engine resolves device + cwd from the SPACE row (idempotent;
                // the doc host would materialize the chat on first command
                // anyway, so failures are non-fatal).
                if is_new && let Some(space_id) = &space_id {
                    let mutate = Mutate::CreateChat {
                        chat_id: chat_id.clone(),
                        space_id: space_id.clone(),
                        config: resolved.chat_config(),
                        branch: chat_branch.clone(),
                        cwd: worktree_cwd.clone(),
                    };
                    if let Err(err) = call_api(engine.client(), &mutate).await {
                        tracing::warn!(error = %err, "CreateChat mutate unavailable; doc host will materialize the chat");
                    }
                }

                // Stage every attachment on the host device (sequential — the
                // chunks share one channel), then thread the refs into the
                // prompt text (`with_uploaded_attachments`, the persisted transport)
                // and the paths onto the Run request (inline image blocks).
                let mut content = text.clone();
                let mut uploaded_attachments = Vec::new();
                if !staged.is_empty() {
                    for att in &staged {
                        match attachments::upload_attachment(
                            &engine,
                            cx.background_executor(),
                            host_device_id.as_deref(),
                            &chat_id,
                            att,
                        )
                        .await
                        {
                            Ok(upload) => uploaded_attachments.push(upload),
                            Err(err) => {
                                tracing::warn!(name = %att.name, error = %err, "attachment upload failed");
                                return Err(
                                    "Couldn't upload the attachment — the device may be offline."
                                        .to_string(),
                                );
                            }
                        }
                    }
                    // Seed the transcript cache from local bytes so the sent
                    // bubble's thumbnails never round-trip.
                    let seed_device = host_device_id.clone().unwrap_or_else(|| device_id.clone());
                    for (upload, att) in uploaded_attachments.iter().zip(&staged) {
                        attachments::seed_attachment(
                            &seed_device,
                            &upload.path,
                            &att.name,
                            att.image.clone(),
                        );
                        if seed_device != device_id {
                            attachments::seed_attachment(
                                &device_id,
                                &upload.path,
                                &att.name,
                                att.image.clone(),
                            );
                        }
                    }
                    content = attachments::with_uploaded_attachments(&text, &uploaded_attachments);
                    if !queue_cmd {
                        // Refresh the echo in place with the attachment refs
                        // (same id, same clock — the bubble grows its thumbnails
                        // without flickering).
                        let refreshed = SessionMessageEntry {
                            id: message_id.clone(),
                            role: jolt_session_doc::MessageRole::User,
                            parts: vec![MessagePart::Text {
                                id: "t0".into(),
                                text: content.clone(),
                            }],
                            created_at,
                            device_id: "local".into(),
                            status: None,
                            continuation_of: None,
                        };
                        let echo_chat_id = chat_id.clone();
                        this.update(cx, |composer, cx| {
                            composer.state.update(cx, |s, cx| {
                                s.remove_echo(&echo_chat_id, &message_id);
                                s.push_echo(&echo_chat_id, refreshed);
                                cx.notify();
                            });
                        })
                        .ok();
                    }
                }

                let attachment_paths = uploaded_attachments
                    .into_iter()
                    .map(|upload| upload.path)
                    .collect();
                let command = if let Some(shell) = &shell {
                    SessionCommandPayload::Bash {
                        command: shell.command.clone(),
                        exclude_from_context: shell.exclude_from_context,
                        cwd: cwd.clone(),
                        message_id: message_id.clone(),
                    }
                } else if steer_cmd {
                    SessionCommandPayload::Steer {
                        prompt: content.clone(),
                        message_id: Some(message_id.clone()),
                    }
                } else {
                    let request = RunRequest {
                        prompt: content.clone(),
                        harness: resolved.harness,
                        model: resolved.model.clone(),
                        reasoning: resolved.reasoning,
                        model_options: resolved.model_options.clone(),
                        cwd,
                        sandbox: SandboxLevel::WorkspaceWrite,
                        auto_approve: false,
                        resume: None,
                        attachments: attachment_paths,
                    };
                    if queue_cmd {
                        SessionCommandPayload::Queue {
                            request,
                            message_id: message_id.clone(),
                        }
                    } else {
                        SessionCommandPayload::Run {
                            request,
                            message_id: message_id.clone(),
                        }
                    }
                };
                call_api(
                    engine.client(),
                    &QueueCommand {
                        chat_id,
                        command,
                        target_device_id: None,
                    },
                )
                .await
                .map_err(|e| format!("Send failed: {e}"))?;
                Ok(())
            }
            .await;
            this.update(cx, |composer, cx| {
                composer.sending = false;
                composer.sync_default_placeholder(cx);
                let generated_error = result.as_ref().err().cloned();
                if let Err(message) = result {
                    // Failure: red banner, echo removed, prompt back in the
                    // draft, staged files back in the chat's stash.
                    composer.failure = Some(message.into());
                    composer.state.update(cx, |s, cx| {
                        s.remove_echo(&err_chat_id, &err_message_id);
                        s.end_pending_send(&err_chat_id, &err_message_id);
                        cx.notify();
                    });
                    if let Some(restore_text) = restore_text {
                        composer
                            .input
                            .update(cx, |input, cx| input.set_text(restore_text, cx));
                    }
                    if !staged.is_empty() {
                        // Merge by id (stashAttachments): files the user staged
                        // while the send was in flight survive the hand-back.
                        let slot = composer.attachments.entry(err_chat_id.clone()).or_default();
                        let mut merged = staged.clone();
                        merged.extend(
                            slot.drain(..)
                                .filter(|e| !staged.iter().any(|f| f.id == e.id)),
                        );
                        *slot = merged;
                    }
                }
                if let Some(review_id) = generated_review_id {
                    cx.emit(ComposerEvent::GeneratedReviewFinished {
                        review_id,
                        error: generated_error,
                    });
                }
                cx.notify();
            })
            .ok();
        });
        if queue_cmd {
            // Queueing is intentionally multi-shot: submitting another item
            // must not cancel an upload/RPC still finishing for the previous one.
            send_task.detach();
        } else {
            self.send_task = Some(send_task);
        }
    }

    // ---- wizard glue ----

    fn wizard_select(&mut self, option_ix: usize, cx: &mut Context<Self>) {
        let Some(wizard) = self.wizard.as_mut() else {
            return;
        };
        let step = wizard.select(option_ix);
        let has_pick = wizard.page_has_pick();
        self.input.update(cx, |input, cx| {
            input.set_placeholder(
                if has_pick {
                    "Type your own answer, or leave this blank to use the selected option"
                } else {
                    "Type your own answer, or pick an option above"
                },
                cx,
            )
        });
        match step {
            WizardStep::AutoAdvance => self.schedule_auto_advance(cx),
            WizardStep::Done(answers) => self.wizard_finish(answers, cx),
            WizardStep::Stay => {}
        }
        cx.notify();
    }

    fn schedule_auto_advance(&mut self, cx: &mut Context<Self>) {
        self.advance_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(AUTO_ADVANCE_MS))
                .await;
            this.update(cx, |composer, cx| composer.wizard_advance(cx))
                .ok();
        }));
    }

    fn wizard_advance(&mut self, cx: &mut Context<Self>) {
        let current = self.input.read(cx).text().to_string();
        let outcome = {
            let Some(wizard) = self.wizard.as_mut() else {
                return;
            };
            if current.trim().is_empty() && !wizard.page_has_pick() {
                return;
            }
            wizard.set_typed(current);
            match wizard.advance() {
                WizardStep::Done(answers) => Ok(answers),
                WizardStep::Stay | WizardStep::AutoAdvance => {
                    Err((wizard.current_typed().to_string(), wizard.page_has_pick()))
                }
            }
        };
        match outcome {
            Ok(answers) => self.wizard_finish(answers, cx),
            Err((next, has_pick)) => {
                self.wizard_scroll.set_offset(point(px(0.0), px(0.0)));
                self.wizard_options_scroll
                    .set_offset(point(px(0.0), px(0.0)));
                self.input.update(cx, |input, cx| {
                    input.set_text(next, cx);
                    input.set_placeholder(
                        if has_pick {
                            "Type your own answer, or leave this blank to use the selected option"
                        } else {
                            "Type your own answer, or pick an option above"
                        },
                        cx,
                    );
                });
                cx.notify();
            }
        }
    }

    fn wizard_back(&mut self, cx: &mut Context<Self>) {
        let current = self.input.read(cx).text().to_string();
        let previous = self.wizard.as_mut().and_then(|wizard| {
            wizard.set_typed(current);
            wizard
                .back()
                .then(|| (wizard.current_typed().to_string(), wizard.page_has_pick()))
        });
        let Some((previous, has_pick)) = previous else {
            return;
        };
        self.advance_task = None;
        self.wizard_scroll.set_offset(point(px(0.0), px(0.0)));
        self.wizard_options_scroll
            .set_offset(point(px(0.0), px(0.0)));
        self.input.update(cx, |input, cx| {
            input.set_text(previous, cx);
            input.set_placeholder(
                if has_pick {
                    "Type your own answer, or leave this blank to use the selected option"
                } else {
                    "Type your own answer, or pick an option above"
                },
                cx,
            );
        });
        cx.notify();
    }

    /// Submit RespondInput and retire the panel.
    fn wizard_finish(&mut self, answers: Vec<UserInputAnswer>, cx: &mut Context<Self>) {
        let Some(wizard) = self.wizard.take() else {
            return;
        };
        self.advance_task = None;
        self.answered_requests.insert(wizard.request_id.clone());
        self.input.update(cx, |input, cx| input.set_text("", cx));
        // The panel borrowed the composer input; hand back its identity.
        self.sync_default_placeholder(cx);
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let request_id = wizard.request_id.clone();
        let request = QueueCommand {
            chat_id,
            command: SessionCommandPayload::RespondInput {
                request_id: request_id.clone(),
                answers,
            },
            target_device_id: None,
        };
        self.send_task = Some(cx.spawn(async move |this, cx| {
            let result = call_api(engine.client(), &request).await;
            if let Err(err) = result {
                this.update(cx, |composer, cx| {
                    composer.failure = Some(format!("Answer failed: {err}").into());
                    // The answer never left this device — put the panel back.
                    composer.answered_requests.remove(&request_id);
                    cx.notify();
                })
                .ok();
                return;
            }
            // Safety net against a dead-looking session: the command queued,
            // but the host may still REJECT it (e.g. the run's resolver is
            // gone). If the very same request is still the live pending input
            // once the host has had ample time to execute and the resolved
            // flag to sync back, the answer demonstrably didn't take —
            // un-hide the panel instead of leaving the question unanswerable.
            cx.background_executor().timer(Duration::from_secs(2)).await;
            this.update(cx, |composer, cx| {
                let transcript = composer.state.read(cx).transcript.clone();
                let still_pending = pending_input_request(&transcript)
                    .is_some_and(|(pending_id, _)| pending_id == request_id);
                if still_pending && composer.answered_requests.remove(&request_id) {
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn on_wizard_key(&mut self, event: &KeyDownEvent, window: &Window, cx: &mut Context<Self>) {
        // Keys bubbling out of the free-text input must not double-handle:
        // digits select options only while the input is empty, and Enter is the
        // input's own Submit action when it has focus.
        let input_focused = self.input.read(cx).focus_handle.is_focused(window);
        let input_empty = self.input.read(cx).is_empty();
        let key = event.keystroke.key.as_str();
        if let Ok(digit) = key.parse::<usize>()
            && (1..=9).contains(&digit)
        {
            if !input_focused || input_empty {
                self.wizard_select(digit - 1, cx);
                // Consumed as a selection: stop the platform from also
                // inserting the digit into the focused free-text input.
                cx.stop_propagation();
            }
        } else if key == "enter" {
            if !input_focused {
                self.wizard_advance(cx);
                cx.stop_propagation();
            }
        } else if key == "escape" && (!input_focused || input_empty) {
            self.wizard_back(cx);
            cx.stop_propagation();
        }
    }

    // ---- render pieces ----

    fn render_bro_loader(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::of(cx).clone();
        div()
            .id("bro-loader")
            .rounded(px(26.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.input_bg)
            .shadow_lg()
            .px(px(18.0))
            .py(px(16.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .child(loaders::activity_spinner(
                "bro-spinner",
                &theme,
                16.0,
                cx.entity_id(),
                cx,
            ))
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from("Rephrasing nonsense lines for clarity…")),
            )
            .child(
                crate::popover::btn_ghost(&theme, "Cancel", "bro-cancel")
                    .id("bro-cancel")
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_bro(cx))),
            )
            .into_any_element()
    }

    fn render_extraction_loader(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::of(cx).clone();
        div()
            .id("question-extraction-loader")
            .rounded(px(26.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.input_bg)
            .shadow_lg()
            .px(px(18.0))
            .py(px(16.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .child(loaders::activity_spinner(
                "question-extraction-spinner",
                &theme,
                16.0,
                cx.entity_id(),
                cx,
            ))
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from("Finding questions…")),
            )
            .child(
                crate::popover::btn_ghost(&theme, "Cancel", "question-extraction-cancel")
                    .id("question-extraction-cancel")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.close_extracted_answers(cx);
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn render_extracted_wizard(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(wizard) = self
            .extracted_answers
            .as_ref()
            .and_then(|flow| match &flow.state {
                ExtractedAnswerState::Answering(wizard) => Some(wizard.clone()),
                ExtractedAnswerState::Extracting { .. } => None,
            })
        else {
            return gpui::Empty.into_any_element();
        };
        let Some(question) = wizard.current().cloned() else {
            return gpui::Empty.into_any_element();
        };
        let last = wizard.page + 1 >= wizard.questions.len();
        let left = if wizard.page > 0 {
            crate::popover::btn_ghost(&theme, "Back", "extracted-question-back")
                .id("extracted-question-back")
                .on_click(cx.listener(|this, _, _, cx| this.extracted_back(cx)))
                .into_any_element()
        } else {
            crate::popover::btn_ghost(&theme, "Cancel", "extracted-question-cancel")
                .id("extracted-question-cancel")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.close_extracted_answers(cx);
                    cx.notify();
                }))
                .into_any_element()
        };

        div()
            .id("extracted-question-panel")
            .track_focus(&self.wizard_focus)
            .rounded(px(26.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.input_bg)
            .shadow_lg()
            .flex()
            .flex_col()
            .min_h_0()
            .max_h(relative(1.0))
            .overflow_hidden()
            .child(
                div()
                    .flex_none()
                    .px(px(16.0))
                    .pt(px(16.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .text_size(px(10.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from(crate::popover::tracked_upper(
                                "Questions",
                            ))),
                    )
                    .child(
                        div()
                            .h(px(20.0))
                            .px(px(6.0))
                            .flex()
                            .items_center()
                            .rounded(px(6.0))
                            .bg(crate::theme::ink(0.06))
                            .text_size(px(10.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from(wizard.counter())),
                    ),
            )
            .child(
                div()
                    .id("extracted-question-content")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.wizard_scroll)
                    .px(px(16.0))
                    .pt(px(6.0))
                    .pb(px(12.0))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(14.0))
                            .line_height(px(20.0))
                            .font_weight(gpui::FontWeight::NORMAL)
                            .text_color(theme.text)
                            .child(SharedString::from(question.question)),
                    )
                    .when_some(question.context, |el, context| {
                        el.child(
                            div()
                                .mt(px(8.0))
                                .rounded(px(10.0))
                                .bg(crate::theme::ink(0.035))
                                .px(px(10.0))
                                .py(px(8.0))
                                .text_size(px(12.0))
                                .line_height(px(17.0))
                                .text_color(theme.text_muted.opacity(0.8))
                                .child(SharedString::from(context)),
                        )
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .mx(px(16.0))
                    .border_t_1()
                    .border_color(crate::theme::hairline(0.06))
                    .pt(px(12.0))
                    .pb(px(4.0))
                    .px(px(4.0))
                    .child(self.input.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .px(px(16.0))
                    .pb(px(16.0))
                    .pt(px(4.0))
                    .child(left)
                    .child(
                        crate::popover::btn_primary(&theme, if last { "Submit" } else { "Next" })
                            .id("extracted-question-submit")
                            .px(px(16.0))
                            .on_click(cx.listener(|this, _, _, cx| this.extracted_advance(cx))),
                    ),
            )
            .into_any_element()
    }

    /// The agent-asked-a-question panel, rendered in place of the composer with
    /// the same floating-pill chrome (`rounded-[26px]
    /// border-white/[0.08] bg-white/[0.03] shadow-xl`), uppercase header +
    /// "1/3" counter chip, option rows with number kbd chips, a free-text
    /// override over a hairline, and Back / Next-Submit footer.
    fn render_wizard(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(wizard) = self.wizard.clone() else {
            return gpui::Empty.into_any_element();
        };
        let counter = wizard.counter();
        let Some(question) = wizard.current().cloned() else {
            return gpui::Empty.into_any_element();
        };
        let page = wizard.page;
        let last = page + 1 >= wizard.questions.len();
        let typed_empty = self.input.read(cx).is_empty();
        let can_advance = wizard.page_has_pick() || !typed_empty;
        let has_options = !question.options.is_empty();

        let options = question.options.iter().enumerate().map(|(ix, label)| {
            // Selection reads on the row only while no typed override exists;
            // typed answers win.
            let picked = wizard.is_picked(ix) && typed_empty;
            div()
                .id(("wizard-option", ix))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .px(px(14.0))
                .py(px(10.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(if picked {
                    crate::theme::ink(0.16)
                } else {
                    gpui::transparent_black()
                })
                // Option rows use the standard color transition.
                .bg(if picked {
                    crate::theme::ink(0.09)
                } else {
                    motion::hover_blend(
                        &format!("wizard-option-{ix}"),
                        crate::theme::ink(0.025),
                        crate::theme::ink(0.06),
                    )
                })
                .on_hover(motion::hover_listener(format!("wizard-option-{ix}")))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.wizard_select(ix, cx)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(13.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(if picked {
                            theme.text
                        } else {
                            theme.text.opacity(0.9)
                        })
                        .child(SharedString::from(label.clone())),
                )
                .when(ix < 9, |el| {
                    el.child(
                        // Number kbd chip: `size-[22px] rounded-md text-[11px]`.
                        div()
                            .flex_none()
                            .size(px(22.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .bg(if picked {
                                crate::theme::ink(0.16)
                            } else {
                                crate::theme::ink(0.05)
                            })
                            .text_size(px(11.0))
                            .text_color(if picked {
                                theme.text
                            } else {
                                theme.text_muted.opacity(0.6)
                            })
                            .child(SharedString::from(format!("{}", ix + 1))),
                    )
                })
        });

        div()
            .id("question-panel")
            .track_focus(&self.wizard_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_wizard_key(event, window, cx)
            }))
            .rounded(px(26.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.input_bg)
            .shadow_lg()
            .flex()
            .flex_col()
            .min_h_0()
            .max_h(relative(1.0))
            .overflow_hidden()
            // The request identity and progress stay visible while the body
            // scrolls, so a long approval still has clear context.
            .child(
                div()
                    .flex_none()
                    .px(px(16.0))
                    .pt(px(16.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .text_size(px(10.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from(crate::popover::tracked_upper(
                                &question.header,
                            ))),
                    )
                    .when(wizard.questions.len() > 1, |el| {
                        el.child(
                            div()
                                .h(px(20.0))
                                .px(px(6.0))
                                .flex()
                                .items_center()
                                .rounded(px(6.0))
                                .bg(crate::theme::ink(0.06))
                                .text_size(px(10.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text_muted.opacity(0.6))
                                .child(SharedString::from(counter)),
                        )
                    }),
            )
            .child(
                div()
                    .id("question-panel-content")
                    .flex_1()
                    // Keep a short two-line prompt fully visible. Tight window
                    // layouts should scroll only genuinely long questions.
                    .min_h(px(46.0))
                    .overflow_y_scroll()
                    .track_scroll(&self.wizard_scroll)
                    .px(px(16.0))
                    .pt(px(6.0))
                    .pb(px(if has_options { 0.0 } else { 12.0 }))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(14.0))
                            .line_height(px(20.0))
                            .font_weight(gpui::FontWeight::NORMAL)
                            .text_color(theme.text)
                            .child(SharedString::from(question.question.clone())),
                    ),
            )
            // Choices stay pinned with the answer controls while only a long
            // question scrolls. Exceptionally large choice sets get their own
            // bounded scroll region instead of growing the panel off-screen.
            .when(has_options, |panel| {
                panel.child(
                    div()
                        .id("question-panel-options")
                        .flex_none()
                        .max_h(px(WIZARD_OPTIONS_MAX_HEIGHT))
                        .overflow_y_scroll()
                        .track_scroll(&self.wizard_options_scroll)
                        .px(px(16.0))
                        .pb(px(8.0))
                        .when(question.multi_select, |el| {
                            el.child(
                                div()
                                    .mt(px(4.0))
                                    .text_size(px(12.0))
                                    .text_color(theme.text_muted.opacity(0.65))
                                    .child(SharedString::from("Select one or more options.")),
                            )
                        })
                        .child(
                            div()
                                .mt(px(8.0))
                                .flex()
                                .flex_col()
                                .gap(px(4.0))
                                .children(options),
                        ),
                )
            })
            // Choices, the custom answer, and actions stay reachable while a
            // long prompt scrolls above them.
            .child(
                div()
                    .flex_none()
                    .mx(px(16.0))
                    .border_t_1()
                    .border_color(crate::theme::hairline(0.06))
                    .pt(px(12.0))
                    .pb(px(4.0))
                    .px(px(4.0))
                    .child(self.input.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .px(px(16.0))
                    .pb(px(16.0))
                    .pt(px(4.0))
                    .child(if page > 0 {
                        crate::popover::btn_ghost(&theme, "Back", "wizard-back")
                            .id("wizard-back")
                            .on_click(cx.listener(|this, _, _, cx| this.wizard_back(cx)))
                            .into_any_element()
                    } else {
                        div().flex_1().into_any_element()
                    })
                    .child(
                        crate::popover::btn_primary(&theme, if last { "Submit" } else { "Next" })
                            .id("wizard-submit")
                            .px(px(16.0))
                            .when(!can_advance, |el| el.opacity(0.4).cursor_default())
                            .when(can_advance, |el| {
                                el.on_click(cx.listener(|this, _, _, cx| this.wizard_advance(cx)))
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_send_button(
        &mut self,
        mode: SendButtonMode,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = Theme::of(cx);
        // A 28px filled circle: up-arrow to send/steer, or a dark rounded
        // square on the same light circle to stop.
        match mode {
            SendButtonMode::Stop => div()
                .id("composer-stop")
                .debug_selector(|| "composer-send-bounds".into())
                .size(px(28.0))
                .flex_none()
                .rounded_full()
                .bg(theme.text)
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.opacity(0.85))
                .on_click(cx.listener(|this, _, _, cx| this.interrupt(cx)))
                .child(div().size(px(11.0)).rounded(px(3.0)).bg(theme.bg))
                .into_any_element(),
            SendButtonMode::Send | SendButtonMode::Steer => div()
                .id("composer-send")
                .debug_selector(|| "composer-send-bounds".into())
                .size(px(28.0))
                .flex_none()
                .rounded_full()
                .bg(theme.text)
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.opacity(0.85))
                .on_click(cx.listener(|this, _, _, cx| this.on_submit(cx)))
                .child(
                    crate::icons::icon(crate::icons::ARROW_UP)
                        .size(px(14.0))
                        .text_color(theme.bg),
                )
                .into_any_element(),
        }
    }
}

/// Focus lands on the prompt input (window-level focus fallbacks — e.g. after
/// the focused terminal panel is hidden — route here).
impl Focusable for Composer {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let wizard_active =
            self.wizard.is_some() || self.extracted_answers.is_some() || self.bro_active();
        if self.mention.token.is_some()
            && (wizard_active || !self.input.focus_handle(cx).is_focused(window))
        {
            self.reset_mention(None, cx);
        }
        let mode = self.button_mode(cx);
        let (text_width, has_newline, content_height, last_width, epoch, shell_scope) = {
            let input = self.input.read(cx);
            (
                input.measured_text_width(),
                input.has_newline(),
                input.measured_content_height(),
                input.last_width,
                input.layout_epoch,
                shell_scope(input.text()),
            )
        };
        let now = Instant::now();
        // Only measurements taken *after* the last flip may drive the next one
        // (at most one flip per layout pass — a flip invalidates the widths).
        let measured_since_flip = epoch > self.flip_epoch && last_width > 0.0;
        if measured_since_flip {
            // A same-mode width change is an interactive window/pane resize:
            // freeze the mode until sizes settle for RESIZE_SETTLE_MS.
            if self.last_seen_width > 0.0 && (last_width - self.last_seen_width).abs() > 0.5 {
                self.width_changed_at = Some(now);
            }
            self.last_seen_width = last_width;
            if self.expanded_mode {
                if self.expanded_anchor <= 0.0 {
                    self.expanded_anchor = last_width;
                }
            } else {
                // The compact pill's content box is the layout-stable capacity
                // both thresholds measure against.
                self.compact_capacity = last_width - 8.0;
            }
        }
        let resizing = self
            .width_changed_at
            .is_some_and(|t| now.duration_since(t) < Duration::from_millis(RESIZE_SETTLE_MS));
        if resizing && self.settle_task.is_none() {
            // Re-evaluate once the settle window has passed.
            self.settle_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(RESIZE_SETTLE_MS + 20))
                    .await;
                this.update(cx, |composer, cx| {
                    composer.settle_task = None;
                    cx.notify();
                })
                .ok();
            }));
        }
        // Layout-stable compact capacity: measured directly while compact;
        // while expanded, the learned value shifted by any container resize
        // (the expanded input width tracks the container 1:1).
        let capacity = if !self.expanded_mode {
            if last_width > 0.0 {
                last_width - 8.0
            } else {
                f32::MAX // before first measure default to compact
            }
        } else if self.compact_capacity > 0.0 {
            if self.expanded_anchor > 0.0 && last_width > 0.0 {
                self.compact_capacity + (last_width - self.expanded_anchor)
            } else {
                self.compact_capacity
            }
        } else {
            f32::MAX
        };
        let next = composer_flip(
            self.expanded_mode,
            text_width,
            capacity,
            has_newline,
            resizing,
        );
        let committed_flip = next != self.expanded_mode && measured_since_flip;
        if committed_flip {
            self.expanded_mode = next;
            self.flip_epoch = epoch;
            self.expanded_anchor = 0.0;
            // The mode change moves the input width; don't read that jump as
            // an interactive resize.
            self.last_seen_width = 0.0;
        }
        // New chats render expanded regardless of `expanded_mode` (see below),
        // so a mode flip there changes nothing visible — never morph it.
        let new_chat = self.state.read(cx).selected_chat.is_none();
        // Morph clock in ms; dividing by the measurement knob stretches the
        // timeline exactly like shell.rs eval_tween's scaled duration.
        let now_ms = self.morph_clock.elapsed().as_secs_f32() * 1000.0 / motion::speed_scale();
        let route_snap = self
            .route_snap_until
            .is_some_and(|until| Instant::now() < until);
        self.flip_morph = flip_morph_step(
            self.flip_morph,
            committed_flip && !new_chat,
            self.last_rendered_height,
            now_ms,
            motion::reduced_motion(cx),
            route_snap,
        );
        let expanded = self.expanded_mode;

        let failure = self.failure.clone();
        let extraction_notice = self.extraction_notice.clone();
        // Centered full-width composer column capped at 768px.
        let container = div()
            .w_full()
            .max_w(px(768.0))
            .mx_auto()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_SM))
            .px(px(Theme::SPACE_LG))
            .pb(px(Theme::SPACE_LG))
            .when_some(failure, |el, message| {
                // Failure notice matching the transcript ErrorChip palette:
                // compact type, rounded border, 14px DangerTriangle, and a
                // subtle tinted wash instead of a bare red
                // stroke. Amber for the offline-ish case (engine not
                // connected), red for send/run failures. Click dismisses.
                let offline = message.as_ref() == "Engine not connected";
                let (border_c, wash, text_c) = if offline {
                    let amber = theme.warning; // amber-400
                    let amber_200 = theme.warning_muted;
                    (
                        amber.opacity(0.16),
                        amber.opacity(0.05),
                        amber_200.opacity(0.9),
                    )
                } else {
                    let danger = theme.danger; // red-400
                    let red_300 = theme.danger_muted;
                    (
                        danger.opacity(0.16),
                        danger.opacity(0.05),
                        red_300.opacity(0.9),
                    )
                };
                el.child(
                    div()
                        .id("composer-failure")
                        .mx(px(4.0))
                        .mt(px(6.0))
                        .flex()
                        .items_start()
                        .gap(px(8.0))
                        .rounded(px(12.0))
                        .border_1()
                        .border_color(border_c)
                        .bg(wash)
                        .px(px(12.0))
                        .py(px(8.0))
                        .text_size(px(12.0))
                        .line_height(px(16.0))
                        .text_color(text_c)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.failure = None;
                            cx.notify();
                        }))
                        .child(
                            crate::icons::icon(crate::icons::ALERT_TRIANGLE)
                                .size(px(14.0))
                                .mt(px(2.0))
                                .text_color(text_c),
                        )
                        .child(div().min_w_0().child(message)),
                )
            })
            .when_some(extraction_notice, |el, message| {
                el.child(
                    div()
                        .id("question-extraction-notice")
                        .mx(px(4.0))
                        .mt(px(6.0))
                        .rounded(px(12.0))
                        .border_1()
                        .border_color(theme.border)
                        .bg(crate::theme::ink(0.035))
                        .px(px(12.0))
                        .py(px(8.0))
                        .text_size(px(12.0))
                        .line_height(px(16.0))
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.extraction_notice = None;
                            cx.notify();
                        }))
                        .child(message),
                )
            });

        if self.bro_active() {
            let loader = self.render_bro_loader(cx);
            return container.child(motion::fade_quick("bro-loader", div().child(loader)));
        }
        if matches!(
            self.extracted_answers.as_ref().map(|flow| &flow.state),
            Some(ExtractedAnswerState::Extracting { .. })
        ) {
            let loader = self.render_extraction_loader(cx);
            return container.child(motion::fade_quick(
                "question-extraction-loader",
                div().child(loader),
            ));
        }
        if matches!(
            self.extracted_answers.as_ref().map(|flow| &flow.state),
            Some(ExtractedAnswerState::Answering(_))
        ) {
            let wizard = self.render_extracted_wizard(cx);
            return container
                .min_h_0()
                .max_h(relative(1.0))
                .overflow_hidden()
                .child(motion::fade_quick(
                    "extracted-question-wizard",
                    div()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .flex()
                        .flex_col()
                        .child(wizard),
                ));
        }
        if self.wizard.is_some() {
            let wizard = self.render_wizard(cx);
            return container
                .min_h_0()
                .max_h(relative(1.0))
                .overflow_hidden()
                .child(motion::fade_quick(
                    "composer-wizard",
                    div()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .flex()
                        .flex_col()
                        .child(wizard),
                ));
        }

        // New chats always use the expanded layout because the repo and branch
        // pickers need the full-width actions row.
        let expanded = expanded || new_chat;

        // Committed-height morph: the layout below is already the NEW mode's;
        // only the pill's height (and the entrance fade/text glide driven by
        // `morph_t`) animates. Steady state renders exactly the target.
        // Staged attachments add the wrap strip's height to the pill in BOTH
        // modes because the attachment strip sits above the input row.
        let staged_count = self.staged().len();
        let strip_width_hint = if last_width > 0.0 { last_width } else { 720.0 };
        let strip_h = attachment_strip_height(staged_count, strip_width_hint);
        let compact_total_height = theme.font_sizes.prompt_line_height() + 26.0;
        let base_height = if expanded {
            composer_total_height(content_height)
        } else {
            compact_total_height
        };
        let target_height = base_height + strip_h;
        let (pill_height, morph_t, morphing) = match self.flip_morph {
            Some(m) if !m.done(now_ms) => {
                (m.height(target_height, now_ms), m.progress(now_ms), true)
            }
            _ => (target_height, 1.0, false),
        };
        if !morphing {
            self.flip_morph = None;
        } else {
            // Manual tween drive: keep frames coming (shell.rs motion_active).
            window.request_animation_frame();
        }
        self.last_rendered_height = pill_height;

        let send_button = self.render_send_button(mode, cx);
        // Attach button opens the native multi-image picker; paste and drop
        // feed the same strip. Its spacing comes exclusively from the shared
        // control-row gap, just like every other composer action.
        let attach = div()
            .id("composer-attach")
            .debug_selector(|| "composer-attach-bounds".into())
            .size(px(28.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            // Use the standard hover color transition.
            .bg(motion::hover_blend(
                "composer-attach",
                gpui::transparent_black(),
                crate::theme::ink(0.10),
            ))
            .on_hover(motion::hover_listener("composer-attach"))
            .on_click(cx.listener(|this, _, _, cx| this.open_file_picker(cx)))
            .child(
                crate::icons::icon(crate::icons::PAPERCLIP)
                    .size(px(16.0))
                    .text_color(theme.text_muted),
            );
        // The staged-thumbnail strip sits above the input in both modes.
        let strip = self.render_attachment_strip(&theme, cx);

        // The pill chrome uses a 26px radius, hairline, faint wash, and large
        // shadow, never a solid grey box. Picker chips,
        // attach, and the send circle all live INSIDE the pill.
        let pill_bg = theme.input_bg;
        let pill = div()
            .debug_selector(|| "composer-pill-bounds".into())
            .rounded(px(26.0))
            .bg(pill_bg)
            .border_1()
            .border_color(theme.border)
            .shadow_lg();
        // The pill's bottom edge is stationary on screen (the composer sits at
        // the bottom of the shell column; growth moves the TOP edge), so the
        // controls pin to the bottom and only the text glides with the reveal
        // (round-9 follow-up: the send/attach/chips must not ride the height,
        // and none of them fade — the full cluster stays visible throughout).
        let cluster_dy = morph_cluster_dy(morph_t);
        let body = if expanded {
            // Expanded: textarea on top (`px-4 pb-1 pt-4`), actions row
            // (`px-2 pb-2.5 pt-1`, h-8 chips → 46px) ABSOLUTE at the pill's
            // stationary bottom — constant screen-y through the morph, with
            // the 2.5px compact↔expanded centering delta gliding out. The
            // text container is laid out at TARGET size (committed layout
            // never reflows mid-tween — the caret can't jump); its top pad
            // eases 12→16 so the first line glides from its compact resting
            // place. The whole control cluster stays at full alpha — chips,
            // attach and send are all (near-)stationary on the bottom anchor.
            let text_pt = morph_text_pad(morph_t);
            pill.h(px(pill_height))
                .overflow_hidden()
                .relative()
                .flex()
                .flex_col()
                .children(strip)
                .child(
                    div()
                        .h(px(
                            (base_height - PILL_BORDER_V - ACTIONS_ROW_HEIGHT).max(0.0)
                        ))
                        .px(px(16.0))
                        .pt(px(text_pt))
                        .pb(px(4.0))
                        .child(self.render_input_with_completion(&theme, cx)),
                )
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom(px(-cluster_dy))
                        .h(px(ACTIONS_ROW_HEIGHT))
                        .flex()
                        .flex_row()
                        .items_center()
                        // Shared cluster metrics: internal gaps and the 8px
                        // right inset match compact mode.
                        .gap(px(Theme::SPACE_XS))
                        .px(px(8.0))
                        .pt(px(4.0))
                        .pb(px(10.0))
                        .child(if let Some(scope) = shell_scope {
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(shell_mode_chip(scope, &theme))
                        } else {
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(self.expanded_picker_controls.clone())
                        })
                        .child(self.picker_usage.clone())
                        .child(attach)
                        .child(send_button),
                )
        } else {
            // Compact pill: input and the actions cluster on one 47px line
            // (`py-3 pl-4 pr-2` textarea, `gap-2 py-1.5 pl-1 pr-2` cluster;
            // the 22.75px line centers to the same 12px inset as `py-3`).
            // The row is BOTTOM-justified: during the collapse morph the pill
            // top sweeps down over a stationary row, the text walks down from
            // its expanded resting place via a decaying relative offset, and
            // the whole inline cluster (chips + attach/send) holds its spot at
            // full alpha (2.5px centering delta gliding in).
            let text_glide = match self.flip_morph {
                Some(m) if morphing => collapse_text_glide(m.from, morph_t),
                _ => 0.0,
            };
            pill.h(px(pill_height))
                .overflow_hidden()
                .flex()
                .flex_col()
                .justify_end()
                .children(strip)
                .child(
                    div()
                        .h(px(compact_total_height - PILL_BORDER_V))
                        .flex()
                        .flex_row()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .pl(px(16.0))
                                .pr(px(8.0))
                                .relative()
                                .top(px(-text_glide))
                                .child(self.render_input_with_completion(&theme, cx)),
                        )
                        .child(
                            div()
                                .debug_selector(|| "composer-compact-actions-bounds".into())
                                .w(px(COMPACT_ACTIONS_WIDTH))
                                .flex_none()
                                .flex()
                                .flex_row()
                                .items_center()
                                // Shared cluster metrics with internals and
                                // right inset identical to expanded.
                                .gap(px(Theme::SPACE_XS))
                                .pl(px(4.0))
                                .pr(px(8.0))
                                .relative()
                                .top(px(-cluster_dy))
                                .child(if let Some(scope) = shell_scope {
                                    div().flex_none().child(shell_mode_chip(scope, &theme))
                                } else {
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(self.compact_picker_controls.clone())
                                })
                                .child(self.picker_usage.clone())
                                .child(attach)
                                .child(send_button),
                        ),
                )
        };
        // The file dropzone lives in the shell (the whole conversation column,
        // not just the pill — shell.rs `chat-dropzone`); drops land back here
        // via `add_paths`.
        let container = match self.render_goal(cx) {
            Some(goal) => container.child(goal),
            None => container,
        };
        let container = match self.render_queue(cx) {
            Some(queue) => container.child(queue),
            None => container,
        };
        let container = container.child(motion::fade_quick("composer-input", body));
        // Ref/workspace toolbar under the pill (t3code BranchToolbar): the
        // checkout-kind selector + ref picker for new sessions, read-only
        // labels once the session exists. Its entity boundary keeps ordinary
        // composer edits from rebuilding the toolbar and its menus.
        let has_footer = self
            .state
            .read(cx)
            .selected_space_row()
            .is_some_and(|space| space.git_detected);
        let container = container.when(has_footer, |container| {
            container.child(self.picker_footer.clone())
        });
        // Full-size preview of a staged thumbnail (AttachmentPreviewDialog).
        if let Some(preview) = self.preview.clone() {
            let weak = cx.weak_entity();
            return container.child(attachments::lightbox(
                window.viewport_size(),
                &preview,
                move |_, cx| {
                    weak.update(cx, |this, cx| {
                        this.preview = None;
                        cx.notify();
                    })
                    .ok();
                },
            ));
        }
        match self.render_goal_dialog(window.viewport_size(), cx) {
            Some(dialog) => container.child(dialog),
            None => container,
        }
    }
}

fn selected_copy_text(
    content: &str,
    selected_range: &Range<usize>,
    transcript_selection: Option<String>,
) -> Option<String> {
    if selected_range.is_empty() {
        transcript_selection
    } else {
        Some(content[selected_range.clone()].to_string())
    }
}

#[cfg(test)]
mod tests;
