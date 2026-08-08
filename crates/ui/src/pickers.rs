//! Composer pickers: RepoPicker (recents + search +
//! in-app folder browser + clone/create), BranchPicker (search + isolated-
//! worktree toggle), HarnessModelPicker (harness rail + model list, harness
//! locked once the chat exists), TraitsPicker (reasoning ladder + advertised
//! model options; trigger shows the non-default summary "High · 1M · Fast").
//!
//! All selections accumulate into a [`DraftConfig`] the composer threads into
//! the Run command and the `Mutate createChat` call on first send.
//!
//! Pure logic (repo ordering, folder-browser navigation, traits summary) lives
//! in free functions with unit tests; RPC results land in [`Loadable`] slots
//! rendered as skeletons / inline errors with Retry.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable as _, KeyDownEvent, PathBuilder,
    Render, SharedString, Subscription, Task, Window, canvas, div, point, prelude::*, px,
};

use jolt_api::{
    GetCheckoutReview, HarnessDescriptor, ListModels, ListRefs, Mutate, SwitchRef, call as call_api,
};
use jolt_proto::{
    ChatConfig, CheckoutReview, FolderListing, HarnessId, Model, ReasoningLevel, RepoRef,
    SandboxLevel, Space, UsageSummary,
};

mod checkout;

/// Display cap for the ref list (t3code shows pages of 100 with a status
/// footer; a flat cap + "Showing X of Y refs" reads the same without
/// pagination plumbing).
const MAX_REF_ROWS: usize = 300;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::motion;
use crate::popover::{self, Loadable, MenuKey};
use crate::settings::composer::ComposerDefaults;
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Draft config (what the pickers accumulate)
// ---------------------------------------------------------------------------

/// Everything a new chat is configured with before the first send. The folder
/// and device come from the selected SPACE — the draft only carries the VCS
/// extras (ref + checkout kind) and the run config.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DraftConfig {
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    /// option id → choice id (only non-defaults are meaningful).
    pub model_options: serde_json::Map<String, serde_json::Value>,
    /// The picked ref (base branch in NewWorktree mode; a worktree's branch
    /// when reusing one). `None` = the repo's current branch.
    pub branch: Option<String>,
    /// Backend revision corresponding to `branch`; differs for JJ working-copy
    /// labels (the change id displays, `@` executes).
    pub revision: Option<String>,
    /// Where the new session runs (the t3code env-mode).
    pub checkout: CheckoutKind,
}

/// Where a new session runs (t3code's env-mode: `local | worktree`). "Current
/// worktree" is NOT a third mode — it's `Local` when the picked ref is already
/// materialized as a worktree (the session reuses that checkout's path).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CheckoutKind {
    /// The space's own folder — or the picked ref's existing worktree.
    #[default]
    Local,
    /// A fresh isolated worktree created off the picked base ref on send.
    NewWorktree,
}

/// The resolved on-send checkout action (composer consumes this — see
/// [`Pickers::checkout_plan`]).
#[derive(Debug, Clone, PartialEq)]
pub enum CheckoutPlan {
    /// Run in the space folder as-is. `branch` is the checkout's branch (the
    /// picked or current ref), carried onto `createChat` so the session names
    /// it from the first frame; `None` = refs never loaded.
    CurrentCheckout { branch: Option<String> },
    /// Reuse the picked ref's existing checkout (a cwd override; no VCS mutation).
    ReuseWorktree { path: String, branch: String },
    /// `CreateWorktree` off `base` on send (jolt mints a `jolt/<name>`
    /// branch). `base: None` = refs never loaded — send falls back to the
    /// space folder rather than failing.
    NewWorktree { base: Option<String> },
}

/// The fully-resolved run configuration the composer sends: concrete harness,
/// model and reasoning (never a "default" passthrough once the catalog is
/// loaded), plus the explicit non-default option picks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedRunConfig {
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    pub model_options: serde_json::Map<String, serde_json::Value>,
}

impl ResolvedRunConfig {
    /// The `ChatConfig` recorded on `Mutate createChat` (needs a known harness).
    pub fn chat_config(&self) -> Option<ChatConfig> {
        Some(ChatConfig {
            harness: self.harness?,
            model: self.model.clone(),
            reasoning: self.reasoning,
            model_options: self.model_options.clone(),
            sandbox: SandboxLevel::WorkspaceWrite,
        })
    }
}

// ---------------------------------------------------------------------------
// Pure: default resolution (no "Default" placeholders — a concrete pick always)
// ---------------------------------------------------------------------------

/// The harness's default model is the first catalog row; both curated catalogs
/// lead with the flagship.
pub fn default_model(models: &[Model]) -> Option<&Model> {
    models.first()
}

/// A model's default reasoning: High when available, then Medium, then the
/// ladder's first entry. `None` only for ladder-less models.
pub fn default_reasoning(ladder: &[ReasoningLevel]) -> Option<ReasoningLevel> {
    // The recommended default is High (user-corrected — not X-High globally);
    // fall to Medium then the ladder's first entry for shorter ladders.
    if ladder.contains(&ReasoningLevel::High) {
        return Some(ReasoningLevel::High);
    }
    if ladder.contains(&ReasoningLevel::Medium) {
        return Some(ReasoningLevel::Medium);
    }
    ladder.first().copied()
}

/// Clamp a picked/remembered level to what the model actually offers: keep it
/// when the ladder lists it; otherwise fall back to the model's default and
/// never retain a stale or foreign level.
pub fn clamp_reasoning(
    level: Option<ReasoningLevel>,
    ladder: &[ReasoningLevel],
) -> Option<ReasoningLevel> {
    match level {
        Some(level) if ladder.contains(&level) => Some(level),
        _ => default_reasoning(ladder),
    }
}

// ---------------------------------------------------------------------------
// Pure: labels + traits summary
// ---------------------------------------------------------------------------

pub fn reasoning_label(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal => "Minimal",
        ReasoningLevel::Low => "Low",
        ReasoningLevel::Medium => "Medium",
        ReasoningLevel::High => "High",
        ReasoningLevel::XHigh => "X-High",
        ReasoningLevel::Max => "Max",
        ReasoningLevel::Ultra => "Ultra",
        ReasoningLevel::Ultracode => "Ultracode",
        ReasoningLevel::Ultrathink => "Ultrathink",
    }
}

/// The TraitsPicker trigger summary: non-default reasoning + non-default model
/// option choices, joined with " · " (jolt: "High · 1M · Fast"). `None` when
/// everything is at its default.
pub fn traits_summary(
    model: Option<&Model>,
    reasoning: Option<ReasoningLevel>,
    selections: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(level) = reasoning {
        parts.push(reasoning_label(level).to_string());
    }
    if let Some(model) = model {
        for option in &model.options {
            let Some(choice_id) = selections.get(&option.id).and_then(|v| v.as_str()) else {
                continue;
            };
            if choice_id == option.default_choice {
                continue;
            }
            if let Some(choice) = option.choices.iter().find(|c| c.id == choice_id) {
                parts.push(choice.label.clone());
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

// ---------------------------------------------------------------------------
// Pure: folder-browser navigation (used by the shell's add-space flow)
// ---------------------------------------------------------------------------

/// Parent of an absolute path; `None` at the filesystem root.
pub fn parent_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None; // was "/" (or empty)
    }
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(at) => Some(trimmed[..at].to_string()),
        None => None,
    }
}

/// Join a listing path and an entry name.
pub fn child_path(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// Breadcrumb segments for a path: `(label, full path)`, root first.
pub fn breadcrumbs(path: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![("/".to_string(), "/".to_string())];
    let mut acc = String::new();
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        acc.push('/');
        acc.push_str(segment);
        out.push((segment.to_string(), acc.clone()));
    }
    out
}

/// Directory rows of a listing (files never render in the browser).
pub fn browser_rows(listing: &FolderListing) -> Vec<&jolt_proto::FolderEntry> {
    listing.entries.iter().filter(|e| e.is_dir).collect()
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

/// Which picker popover is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Branch,
    /// The checkout-kind dropdown in the composer footer (Current
    /// checkout/worktree | New worktree).
    Checkout,
    HarnessModel,
    Traits,
    /// New-session target space. Existing sessions remain pinned to theirs.
    Space,
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextPressure {
    Normal,
    Warning,
    Danger,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReviewLookup {
    chat_id: String,
    cwd: String,
    branch: Option<String>,
    activity_at: Option<i64>,
    device_id: String,
}

fn context_pressure(usage: Option<&UsageSummary>) -> ContextPressure {
    match usage.and_then(UsageSummary::context_fraction) {
        Some(fraction) if fraction >= 0.9 => ContextPressure::Danger,
        Some(fraction) if fraction >= 0.7 => ContextPressure::Warning,
        _ => ContextPressure::Normal,
    }
}

fn search_active_index(query: &str, selected_index: usize) -> usize {
    if query.trim().is_empty() {
        selected_index
    } else {
        0
    }
}

fn compact_decimal(value: f64) -> String {
    let formatted = format!("{value:.1}");
    formatted
        .strip_suffix(".0")
        .unwrap_or(&formatted)
        .to_string()
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{}m", compact_decimal(tokens as f64 / 1_000_000.0))
    } else if tokens >= 1_000 {
        format!("{}k", compact_decimal(tokens as f64 / 1_000.0))
    } else {
        tokens.to_string()
    }
}

fn format_context(usage: &UsageSummary) -> String {
    match (usage.context_tokens, usage.context_window) {
        (Some(tokens), Some(window)) if window != 0 => format!(
            "{}% · {}/{}",
            compact_decimal(tokens as f64 / window as f64 * 100.0),
            format_tokens(tokens),
            format_tokens(window)
        ),
        (Some(tokens), _) => format!("{} used", format_tokens(tokens)),
        _ => "Unavailable".into(),
    }
}

fn usage_stat(label: &'static str, value: String, theme: &Theme) -> gpui::Div {
    div()
        .min_w_0()
        .flex_1()
        .child(
            div()
                .text_size(px(10.0))
                .text_color(theme.text_muted)
                .child(label),
        )
        .child(
            div()
                .mt(px(3.0))
                .truncate()
                .text_size(px(12.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(value),
        )
}

fn usage_progress_ring(
    fraction: Option<f64>,
    progress_color: gpui::Hsla,
    track_color: gpui::Hsla,
) -> impl IntoElement {
    let progress = fraction.unwrap_or_default().clamp(0.0, 1.0) as f32;
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let center_x = bounds.origin.x + bounds.size.width / 2.0;
            let center_y = bounds.origin.y + bounds.size.height / 2.0;
            let radius = px(6.0);
            let stroke = px(2.0);

            let mut track = PathBuilder::stroke(stroke);
            track.move_to(point(center_x + radius, center_y));
            track.arc_to(
                point(radius, radius),
                px(0.0),
                false,
                true,
                point(center_x - radius, center_y),
            );
            track.arc_to(
                point(radius, radius),
                px(0.0),
                false,
                true,
                point(center_x + radius, center_y),
            );
            track.close();
            if let Ok(path) = track.build() {
                window.paint_path(path, track_color);
            }

            if progress == 0.0 {
                return;
            }
            let mut arc = PathBuilder::stroke(stroke);
            if progress >= 0.999 {
                arc.move_to(point(center_x + radius, center_y));
                arc.arc_to(
                    point(radius, radius),
                    px(0.0),
                    false,
                    true,
                    point(center_x - radius, center_y),
                );
                arc.arc_to(
                    point(radius, radius),
                    px(0.0),
                    false,
                    true,
                    point(center_x + radius, center_y),
                );
                arc.close();
            } else {
                arc.move_to(point(center_x, center_y - radius));
                let angle = -std::f32::consts::FRAC_PI_2 + progress * std::f32::consts::TAU;
                arc.arc_to(
                    point(radius, radius),
                    px(0.0),
                    progress > 0.5,
                    true,
                    point(
                        center_x + radius * angle.cos(),
                        center_y + radius * angle.sin(),
                    ),
                );
            }
            if let Ok(path) = arc.build() {
                window.paint_path(path, progress_color);
            }
        },
    )
    .size(px(16.0))
}

struct UsagePopover {
    usage: Option<UsageSummary>,
}

impl Render for UsagePopover {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        let card = div()
            .w(px(300.0))
            .p(px(14.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_raised)
            .shadow_md()
            .text_size(px(11.0));
        let Some(usage) = self.usage.as_ref().filter(|usage| usage.calls != 0) else {
            return card
                .child(
                    div()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child("Context window"),
                )
                .child(
                    div()
                        .mt(px(5.0))
                        .text_color(theme.text_muted)
                        .child("Available after the first response"),
                );
        };

        let fraction = usage.context_fraction().map(|value| value.clamp(0.0, 1.0));
        let progress_color = match context_pressure(Some(usage)) {
            ContextPressure::Normal => theme.text_muted.opacity(0.8),
            ContextPressure::Warning => theme.warning,
            ContextPressure::Danger => theme.danger,
        };
        let cost = usage
            .cost_usd
            .map(|cost| format!("${cost:.2}"))
            .unwrap_or_else(|| "Unavailable".into());
        let model = usage
            .model
            .as_deref()
            .unwrap_or("Unknown model")
            .to_string();

        card.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.0))
                .child(
                    div()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child("Context window"),
                )
                .child(
                    div()
                        .text_color(theme.text_muted)
                        .child(format_context(usage)),
                ),
        )
        .child(
            div()
                .mt(px(9.0))
                .h(px(5.0))
                .w_full()
                .overflow_hidden()
                .rounded_full()
                .bg(theme.element_hover)
                .child(
                    div()
                        .h_full()
                        .w(gpui::relative(fraction.unwrap_or_default() as f32))
                        .rounded_full()
                        .bg(progress_color),
                ),
        )
        .child(
            div()
                .mt(px(13.0))
                .flex()
                .items_center()
                .justify_between()
                .child(div().text_color(theme.text_muted).child("Total processed"))
                .child(
                    div()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(format_tokens(usage.total_tokens())),
                ),
        )
        .child(div().my(px(12.0)).h(px(1.0)).bg(theme.border))
        .child(
            div()
                .flex()
                .gap(px(18.0))
                .child(usage_stat(
                    "Prompt",
                    format_tokens(usage.prompt_tokens()),
                    theme,
                ))
                .child(usage_stat(
                    "Output",
                    format_tokens(usage.output_tokens),
                    theme,
                )),
        )
        .child(
            div()
                .mt(px(10.0))
                .flex()
                .gap(px(18.0))
                .child(usage_stat(
                    "Cache read",
                    format_tokens(usage.cache_read_input_tokens),
                    theme,
                ))
                .child(usage_stat(
                    "Cache write",
                    format_tokens(usage.cache_write_input_tokens),
                    theme,
                )),
        )
        .child(
            div()
                .mt(px(10.0))
                .flex()
                .gap(px(18.0))
                .child(usage_stat("Estimated API equivalent", cost, theme))
                .child(usage_stat("Model", model, theme)),
        )
    }
}

pub struct Pickers {
    state: Entity<AppState>,
    config: DraftConfig,
    /// Sticky last-used picks (jolt `jolt.composer.defaults:v1`): seeds the
    /// new-chat chips and is rewritten on every new-chat pick.
    defaults: ComposerDefaults,
    /// Where [`Self::defaults`] persists (`{data_dir}/composer-defaults.json`);
    /// `None` before bootstrap stamps the state (writes are skipped).
    data_dir: Option<PathBuf>,
    /// Selection the draft picks belong to — switching chats drops them so a
    /// pick made in one chat never leaks into another.
    draft_owner: Option<String>,
    /// Space the branch draft/cache belong to (see the state observer).
    space_owner: Option<String>,
    open: Option<PickerKind>,
    harnesses: Loadable<Vec<HarnessDescriptor>>,
    models: HashMap<HarnessId, Loadable<Vec<Model>>>,
    refs: Loadable<Vec<RepoRef>>,
    /// Space id the `refs` slot belongs to (invalidated on space change).
    refs_space: Option<String>,
    /// Invalidates every space/device-derived async response.
    catalog_generation: u64,
    /// Highlighted row in the open list (keyboard nav).
    active: usize,
    /// Models-list scroll — keyboard nav keeps the highlighted row in view
    /// (`scroll_to_item`; the add-space palette standard).
    model_scroll: gpui::ScrollHandle,
    /// Shared search / URL / name input, reused across popovers.
    search: Entity<ComposerInput>,
    focus: FocusHandle,
    /// Re-open suppression after outside-click dismissal (the dismiss and the
    /// trigger click would otherwise toggle twice).
    suppressed: Option<(PickerKind, Instant)>,
    /// `JOLT_OPEN_PICKER` boot: keep claiming focus until it sticks, so
    /// keyboard nav drives the data-side-opened popover (headless rigs have
    /// no synthetic pointer, but synthetic keys do arrive).
    boot_focus_pending: bool,
    load_task: Option<Task<()>>,
    /// Own slot: the refs load runs concurrently with the eager
    /// harness/model loads — sharing `load_task` would abort one mid-flight.
    refs_task: Option<Task<()>>,
    checkout_review: Option<CheckoutReview>,
    review_lookup: Option<ReviewLookup>,
    review_loaded: bool,
    review_task: Option<Task<()>>,
    /// In-flight mid-session `SwitchRef` (the ref being switched to).
    switching: Option<String>,
    switch_task: Option<Task<()>>,
    /// Last mid-session switch failure (shown in the ref popover).
    switch_error: Option<String>,
    mutate_task: Option<Task<()>>,
    _search_events: Subscription,
    _state_observe: Subscription,
}

impl Pickers {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| ComposerInput::new("Search…", cx));
        let search_events = cx.subscribe(&search, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Edited => {
                // Clearing the shared input on open emits this after `toggle`;
                // an empty filter must retain the picker's selected-row cursor.
                let selected_index = this
                    .open
                    .map(|kind| this.initial_active_index(kind, cx))
                    .unwrap_or(0);
                this.active = search_active_index(this.search.read(cx).text(), selected_index);
                cx.notify();
            }
            ComposerInputEvent::Submitted | ComposerInputEvent::QueueSubmitted => {
                this.on_search_submit(cx)
            }
            // Pasted images/files don't apply to a search box.
            ComposerInputEvent::PastedImages(_)
            | ComposerInputEvent::PastedPaths(_)
            | ComposerInputEvent::CursorMoved
            | ComposerInputEvent::ViewportChanged
            | ComposerInputEvent::MessageHistoryNavigate(_)
            | ComposerInputEvent::MentionNavigate(_)
            | ComposerInputEvent::MentionAccept
            | ComposerInputEvent::MentionDismiss => {}
        });
        // Chat selection / config changes must re-render the chips (child views
        // only re-render on their own notify). A selection change also drops
        // the draft picks — they belonged to the previous chat/new-chat canvas.
        let state_observe = cx.observe(&state, |this: &mut Self, state, cx| {
            let selected = state.read(cx).selected_chat.clone();
            if selected != this.draft_owner {
                this.draft_owner = selected;
                this.config.harness = None;
                this.config.model = None;
                this.config.reasoning = None;
                this.config.model_options.clear();
                this.switch_error = None;
                this.invalidate_checkout_review();
            }
            // A space switch invalidates the branch draft + cache — the folder
            // (and possibly the device) changed under them.
            let space = state.read(cx).selected_space.clone();
            if space != this.space_owner {
                this.space_owner = space;
                this.config.branch = None;
                this.config.revision = None;
                this.config.checkout = CheckoutKind::default();
                this.refs = Loadable::Idle;
                this.refs_space = None;
                this.refs_task = None;
                this.load_task = None;
                this.catalog_generation = this.catalog_generation.wrapping_add(1);
                // Catalogs are per-DEVICE (fetched from the space's host):
                // a space switch may land on another device, so refetch.
                this.harnesses = Loadable::Idle;
                this.models.clear();
            }
            cx.notify();
        });
        // Dev/testing knob: `JOLT_OPEN_PICKER=model|traits|repo|branch` boots
        // with that popover open — synthetic input can't reach the app on
        // headless compositors, so captures need a data-side path.
        let open = match std::env::var("JOLT_OPEN_PICKER").ok().as_deref() {
            Some("model") => Some(PickerKind::HarnessModel),
            Some("traits") => Some(PickerKind::HarnessModel),
            Some("branch") => Some(PickerKind::Branch),
            Some("checkout") => Some(PickerKind::Checkout),
            _ => None,
        };
        // Sticky last-used picks: loaded synchronously so the very first frame
        // shows the remembered harness/model/reasoning, never a placeholder.
        let data_dir = state.read(cx).data_dir.clone();
        let defaults = data_dir
            .as_deref()
            .map(ComposerDefaults::load)
            .unwrap_or_default();
        let draft_owner = state.read(cx).selected_chat.clone();
        let space_owner = state.read(cx).selected_space.clone();
        Self {
            state,
            space_owner,
            config: DraftConfig::default(),
            defaults,
            data_dir,
            draft_owner,
            open,
            harnesses: Loadable::Idle,
            models: HashMap::new(),
            refs: Loadable::Idle,
            refs_space: None,
            catalog_generation: 0,
            active: 0,
            model_scroll: gpui::ScrollHandle::new(),
            search,
            focus: cx.focus_handle(),
            suppressed: None,
            boot_focus_pending: open.is_some(),
            load_task: None,
            refs_task: None,
            checkout_review: None,
            review_lookup: None,
            review_loaded: false,
            review_task: None,
            switching: None,
            switch_task: None,
            switch_error: None,
            mutate_task: None,
            _search_events: search_events,
            _state_observe: state_observe,
        }
    }

    /// Persist the sticky defaults (best-effort; picks are rare and tiny).
    fn save_defaults(&self) {
        if let Some(dir) = self.data_dir.as_deref()
            && let Err(err) = self.defaults.save(dir)
        {
            tracing::warn!(error = %err, "composer-defaults save failed");
        }
    }

    pub fn draft(&self) -> &DraftConfig {
        &self.config
    }

    /// Harness is locked once the chat exists.
    fn harness_locked(&self, _cx: &App) -> bool {
        false
    }

    fn engine(&self, cx: &App) -> Option<EngineHandle> {
        self.state.read(cx).engine().cloned()
    }

    /// The selected space's device when it differs from the connected
    /// engine's own — harness/model catalogs come from the device that RUNS
    /// the agents (the CLIs live there; the viewer may have neither claude
    /// nor codex installed — user report: "can't load codex models/traits
    /// anywhere" from a Mac without codex).
    fn space_target(&self, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        let device = state.selected_space_row()?.device_id.clone();
        (state.local_device_id.as_deref() != Some(device.as_str())).then_some(device)
    }

    /// Effective harness: picked, or the chat's config, or the first listed.
    fn effective_harness(&self, cx: &App) -> Option<HarnessId> {
        if let Some(harness) = self.config.harness {
            return Some(harness);
        }
        if let Some(config) = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            return Some(config.harness);
        }
        // New-chat canvas: the remembered last-used harness (sticky defaults),
        // when the loaded catalog still offers it.
        if let Some(harness) = self.defaults.harness {
            let offered = match self.harnesses.ready() {
                Some(list) => visible_harnesses(list).iter().any(|d| d.id == harness),
                None => true, // catalog not loaded yet — trust the memory
            };
            if offered {
                return Some(harness);
            }
        }
        // Fall back to the first VISIBLE harness: the registry lists the mock
        // harness first, and resolving chips against it would boot the
        // new-chat canvas onto "Mock" instead of Claude Code + its default
        // model (it stays available under `JOLT_HARNESS=mock`).
        self.harnesses
            .ready()
            .and_then(|list| visible_harnesses(list).first().map(|d| d.id))
    }

    /// Effective model id: the draft pick, the selected chat's config, or (on
    /// the new-chat canvas) the remembered last-used model for the harness.
    fn effective_model_id<'a>(&'a self, cx: &'a App) -> Option<&'a str> {
        if let Some(id) = self.config.model.as_deref() {
            return Some(id);
        }
        if let Some(chat) = self.state.read(cx).selected_chat_row() {
            return chat.config.as_ref().and_then(|c| c.model.as_deref());
        }
        let harness = self.effective_harness(cx)?;
        self.defaults.model_for(harness).map(|m| m.id.as_str())
    }

    /// Effective reasoning — always concrete once the model is known: the
    /// draft pick / chat config / remembered default, clamped to the selected
    /// model's ladder, falling back to the model's default level.
    fn effective_reasoning(&self, cx: &App) -> Option<ReasoningLevel> {
        let explicit = self.config.reasoning.or_else(|| {
            match self.state.read(cx).selected_chat_row() {
                Some(chat) => chat.config.as_ref().and_then(|c| c.reasoning),
                // New chat: the remembered last-used level.
                None => self.defaults.reasoning,
            }
        });
        if self.selected_model(cx).is_none() {
            // Catalog not loaded yet: show the explicit value as-is (nothing
            // to clamp against); it resolves to a concrete level on load.
            return explicit;
        }
        clamp_reasoning(explicit, &self.trait_ladder(cx))
    }

    /// The selected model — concrete from the moment the list loads: the
    /// effective id when the list still offers it, else the harness default
    /// (first row). Never `None` with a non-empty catalog.
    fn selected_model<'a>(&'a self, cx: &'a App) -> Option<&'a Model> {
        let harness = self.effective_harness(cx)?;
        let models = self.models.get(&harness)?.ready()?;
        match self.effective_model_id(cx) {
            Some(id) => models
                .iter()
                .find(|m| m.id == id)
                .or_else(|| default_model(models)),
            None => default_model(models),
        }
    }

    fn selected_model_index(&self, cx: &App) -> usize {
        let Some(selected) = self.selected_model(cx) else {
            return 0;
        };
        let Some(models) = self
            .effective_harness(cx)
            .and_then(|harness| self.models.get(&harness))
            .and_then(Loadable::ready)
        else {
            return 0;
        };
        models
            .iter()
            .position(|model| model.id == selected.id)
            .unwrap_or(0)
    }

    /// The explicit (non-default) option picks: the chat's persisted
    /// selections for existing chats, the draft's for the new-chat canvas.
    fn explicit_options(&self, cx: &App) -> serde_json::Map<String, serde_json::Value> {
        match self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            Some(config) => config.model_options.clone(),
            None => self.config.model_options.clone(),
        }
    }

    /// The fully-resolved config the composer threads into the Run request and
    /// `Mutate createChat`: concrete model + reasoning whenever the catalog is
    /// loaded (no "engine picks a default" passthrough).
    pub fn resolved(&self, cx: &App) -> ResolvedRunConfig {
        ResolvedRunConfig {
            harness: self.effective_harness(cx),
            model: self
                .selected_model(cx)
                .map(|m| m.id.clone())
                // Catalog not loaded (offline): still send the id we know.
                .or_else(|| self.effective_model_id(cx).map(str::to_string)),
            reasoning: self.effective_reasoning(cx),
            model_options: self.explicit_options(cx),
        }
    }

    // ---- open/close ----

    fn close(&mut self, cx: &mut Context<Self>) {
        if let Some(kind) = self.open.take() {
            self.suppressed = Some((kind, Instant::now()));
        }
        cx.notify();
    }

    /// Capture knob (`JOLT_OPEN_DIALOG=model`): open the combined
    /// harness/model menu programmatically.
    pub fn open_model_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open != Some(PickerKind::HarnessModel) {
            self.toggle(PickerKind::HarnessModel, window, cx);
        }
    }

    fn initial_active_index(&self, kind: PickerKind, cx: &App) -> usize {
        match kind {
            PickerKind::Checkout => match self.config.checkout {
                CheckoutKind::Local => 0,
                CheckoutKind::NewWorktree => 1,
            },
            PickerKind::Branch => self.selected_ref_index(cx),
            PickerKind::HarnessModel | PickerKind::Traits => self.selected_model_index(cx),
            PickerKind::Space => self.selected_space_index(cx),
            PickerKind::Usage => 0,
        }
    }

    fn toggle(&mut self, kind: PickerKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.open == Some(kind) {
            self.open = None;
            cx.notify();
            return;
        }
        // A just-dismissed popover's trigger click must not instantly reopen.
        if let Some((suppressed, at)) = self.suppressed.take()
            && suppressed == kind
            && at.elapsed() < Duration::from_millis(400)
        {
            cx.notify();
            return;
        }
        self.open = Some(kind);
        self.search.update(cx, |input, cx| {
            input.set_placeholder("Search…", cx);
            input.set_text("", cx);
        });
        // The keyboard-nav highlight starts ON the selected row — row 0
        // otherwise reads as a second active row (user report).
        self.active = self.initial_active_index(kind, cx);
        if kind == PickerKind::HarnessModel {
            self.model_scroll.scroll_to_item(self.active);
        }
        // Searchable pickers focus the filter input (it sits inside the frame,
        // so the frame's key handler still sees arrows/Enter); the rest focus
        // the frame itself for pure keyboard nav.
        match kind {
            PickerKind::Branch => {
                self.switch_error = None; // stale mid-session failures don't linger
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search refs…", cx);
                });
                window.focus(&handle, cx);
            }
            PickerKind::Space => {
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search spaces…", cx);
                });
                window.focus(&handle, cx);
            }
            PickerKind::Usage => {}
            _ => window.focus(&self.focus, cx),
        }
        match kind {
            // Force: the checkout state moves under us (a send mints a
            // worktree+branch, terminals switch refs) — every open
            // revalidates, keeping stale rows visible until fresh ones land.
            PickerKind::Branch | PickerKind::Checkout => {
                self.ensure_refs(true, cx);
                self.ensure_checkout_review(true, cx);
            }
            PickerKind::HarnessModel | PickerKind::Traits => {
                self.ensure_harnesses(cx);
                if let Some(harness) = self.effective_harness(cx) {
                    self.ensure_models(harness, cx);
                }
            }
            PickerKind::Space | PickerKind::Usage => {}
        }
        cx.notify();
    }

    // ---- loads ----

    fn ensure_harnesses(&mut self, cx: &mut Context<Self>) {
        // Only load from Idle: `render` re-runs this every frame, so an Error
        // that could re-trigger a load would flip back to Loading before the
        // retry row ever painted (and spam the engine). Retry resets to Idle.
        if !matches!(self.harnesses, Loadable::Idle) {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let target = self.space_target(cx);
        let generation = self.catalog_generation;
        self.harnesses = Loadable::Loading;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = jolt_api::call(
                engine.client(),
                &jolt_api::ListHarnesses {
                    target_device_id: target,
                },
            )
            .await;
            this.update(cx, |pickers, cx| {
                if pickers.catalog_generation != generation {
                    return;
                }
                pickers.harnesses = match result {
                    Ok(list) => Loadable::Ready(list),
                    Err(err) => Loadable::Error(err.to_string()),
                };
                if let Some(harness) = pickers.effective_harness(cx) {
                    pickers.ensure_models(harness, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn ensure_models(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        // Absent or Idle only — same render-loop hazard as `ensure_harnesses`;
        // the retry row clears the map to re-arm.
        if self
            .models
            .get(&harness)
            .is_some_and(|slot| !matches!(slot, Loadable::Idle))
        {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let target = self.space_target(cx);
        let generation = self.catalog_generation;
        self.models.insert(harness, Loadable::Loading);
        cx.spawn(async move |this, cx| {
            let result = call_api(
                engine.client(),
                &ListModels {
                    harness,
                    target_device_id: target,
                },
            )
            .await;
            this.update(cx, |pickers, cx| {
                if pickers.catalog_generation != generation {
                    return;
                }
                let loaded = match result {
                    Ok(models) => Loadable::Ready(models),
                    Err(err) => Loadable::Error(err.to_string()),
                };
                if let Loadable::Ready(models) = &loaded {
                    let fresh = pickers
                        .defaults
                        .remember_labels(models.iter().map(|m| (m.id.as_str(), m.label.as_str())));
                    if fresh {
                        pickers.save_defaults();
                    }
                }
                pickers.models.insert(harness, loaded);
                if pickers.open == Some(PickerKind::HarnessModel) {
                    pickers.active = pickers.selected_model_index(cx);
                    pickers.model_scroll.scroll_to_item(pickers.active);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn pick_harness(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        if self.effective_harness(cx) != Some(harness) {
            if self.state.read(cx).selected_chat.is_some() {
                self.update_chat_config(cx, |config| {
                    config.harness = harness;
                    config.model = None;
                    config.reasoning = None;
                    config.model_options.clear();
                });
            } else {
                // The remembered model for this harness takes over via the
                // defaults fallback; a foreign pick must not linger.
                self.config.harness = Some(harness);
                self.config.model = None;
                self.config.reasoning = None;
                self.config.model_options.clear();
            }
        }
        self.defaults.harness = Some(harness);
        self.save_defaults();
        self.model_scroll.set_offset(gpui::Point::default());
        self.ensure_models(harness, cx);
        cx.notify();
    }

    fn pick_model(&mut self, model_id: String, cx: &mut Context<Self>) {
        self.open = None;
        if self.state.read(cx).selected_chat.is_some() {
            // Existing chat: persist to the chat row (Mutate setChatConfig) —
            // survives restarts and syncs; next runs in this chat use it.
            self.update_chat_config(cx, move |config| config.model = Some(model_id));
        } else {
            // New chat: draft pick + sticky last-used memory for this harness.
            self.config.model = Some(model_id.clone());
            if let Some(harness) = self.effective_harness(cx) {
                let label = self
                    .models
                    .get(&harness)
                    .and_then(|l| l.ready())
                    .and_then(|models| models.iter().find(|m| m.id == model_id))
                    .map(|m| m.label.clone())
                    .unwrap_or_else(|| model_id.clone());
                self.defaults.remember_model(harness, model_id, label);
                self.save_defaults();
            }
        }
        cx.notify();
    }

    fn pick_reasoning(&mut self, level: ReasoningLevel, cx: &mut Context<Self>) {
        // Always a concrete selection (no toggle-back-to-default).
        if self.state.read(cx).selected_chat.is_some() {
            self.update_chat_config(cx, move |config| config.reasoning = Some(level));
        } else {
            self.config.reasoning = Some(level);
            self.defaults.reasoning = Some(level);
            self.save_defaults();
        }
        cx.notify();
    }

    fn pick_option(
        &mut self,
        option_id: String,
        choice_id: String,
        default: bool,
        cx: &mut Context<Self>,
    ) {
        if self.state.read(cx).selected_chat.is_some() {
            self.update_chat_config(cx, move |config| {
                if default {
                    config.model_options.remove(&option_id);
                } else {
                    config
                        .model_options
                        .insert(option_id, serde_json::Value::String(choice_id));
                }
            });
        } else if default {
            self.config.model_options.remove(&option_id);
        } else {
            self.config
                .model_options
                .insert(option_id, serde_json::Value::String(choice_id));
        }
        cx.notify();
    }

    /// Apply `change` to the selected chat's effective config and persist it:
    /// optimistic row stamp (chips update on click) + `Mutate setChatConfig`
    /// (LWW workspace write — restarts and other devices see it). The written
    /// row always carries the CONCRETE resolved model/reasoning, with the
    /// reasoning re-clamped to the (possibly just-changed) model's ladder.
    fn update_chat_config(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut ChatConfig)) {
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let resolved = self.resolved(cx);
        let Some(mut config) = resolved.chat_config() else {
            return; // harness unknown (catalog + chat row both missing) — nothing safe to write
        };
        // Preserve fields the pickers don't own.
        if let Some(existing) = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            config.sandbox = existing.sandbox;
        }
        change(&mut config);
        // Reasoning must stay concrete for whatever model the row now names —
        // same ladder resolution as [`Self::trait_ladder`] (model levels, else
        // the harness's advertised ladder).
        if let Some(models) = self.models.get(&config.harness).and_then(|l| l.ready()) {
            let mut ladder = config
                .model
                .as_deref()
                .and_then(|id| models.iter().find(|m| m.id == id))
                .map(|m| m.reasoning_levels.clone())
                .unwrap_or_default();
            if ladder.is_empty()
                && let Some(descriptor) = self
                    .harnesses
                    .ready()
                    .and_then(|list| list.iter().find(|d| d.id == config.harness))
            {
                ladder = descriptor.reasoning_levels.clone();
            }
            if !ladder.is_empty() {
                config.reasoning = clamp_reasoning(config.reasoning, &ladder);
            }
        }
        self.state.update(cx, |state, cx| {
            state.apply_chat_config(&chat_id, config.clone());
            cx.notify();
        });
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.mutate_task = Some(cx.spawn(async move |_, _| {
            let request = Mutate::SetChatConfig { chat_id, config };
            if let Err(err) = call_api(engine.client(), &request).await {
                tracing::warn!(error = %err, "setChatConfig mutate failed");
            }
        }));
    }

    // ---- keyboard ----

    /// The traits popover's reasoning ladder (model levels, falling back to
    /// the harness's advertised ladder) — shared by render and keyboard nav.
    fn trait_ladder(&self, cx: &App) -> Vec<ReasoningLevel> {
        let Some(model) = self.selected_model(cx) else {
            return Vec::new();
        };
        if !model.reasoning_levels.is_empty() {
            return model.reasoning_levels.clone();
        }
        self.effective_harness(cx)
            .and_then(|h| {
                self.harnesses
                    .ready()
                    .and_then(|list| list.iter().find(|d| d.id == h))
                    .map(|d| d.reasoning_levels.clone())
            })
            .unwrap_or_default()
    }

    /// The viewed harness's model list, when loaded (keyboard nav rows).
    fn model_rows_len(&self, cx: &App) -> usize {
        self.effective_harness(cx)
            .and_then(|h| self.models.get(&h))
            .and_then(|l| l.ready())
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Enter on the harness/model popover: pick the highlighted model.
    fn activate_model_row(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self
            .effective_harness(cx)
            .and_then(|h| self.models.get(&h))
            .and_then(|l| l.ready())
            .and_then(|m| m.get(self.active))
            .map(|m| m.id.clone())
        else {
            return;
        };
        self.pick_model(id, cx);
    }

    fn filtered_space_rows(&self, cx: &App) -> Vec<Space> {
        let query = self.search.read(cx).text().to_string();
        let state = self.state.read(cx);
        let spaces = state.spaces_sorted();
        let names: Vec<String> = spaces
            .iter()
            .map(|space| space.display_name().to_string())
            .collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|index| spaces[index].clone())
            .collect()
    }

    fn selected_space_index(&self, cx: &App) -> usize {
        let state = self.state.read(cx);
        state
            .selected_space
            .as_deref()
            .and_then(|id| {
                state
                    .spaces_sorted()
                    .iter()
                    .position(|space| space.id == id)
            })
            .unwrap_or(0)
    }

    fn pick_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.state
            .update(cx, |state, cx| state.select_space(Some(space_id), cx));
        self.close(cx);
    }

    fn render_space_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let rows = self.filtered_space_rows(cx);
        let (selected, details): (Option<String>, Vec<(String, bool)>) = {
            let state = self.state.read(cx);
            (
                state.selected_space.clone(),
                rows.iter()
                    .map(|space| state.space_device_tag(space, chrono::Utc::now()))
                    .collect(),
            )
        };
        let active = self.active;
        let body: AnyElement = if rows.is_empty() {
            div()
                .p(px(Theme::SPACE_SM))
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child("No spaces match.")
                .into_any_element()
        } else {
            div()
                .id("space-picker-list")
                .flex()
                .flex_col()
                .gap(px(2.0))
                .max_h(px(224.0))
                .overflow_y_scroll()
                .children(rows.into_iter().zip(details).enumerate().map(
                    |(index, (space, (device, offline)))| {
                        let pick_id = space.id.clone();
                        let is_selected = selected.as_deref() == Some(space.id.as_str());
                        popover::menu_row_nav(
                            &theme,
                            is_selected,
                            index == active,
                            format!("space-picker-row-{index}"),
                        )
                        .id(("space-picker-row", index))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pick_space(pick_id.clone(), cx);
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(space.display_name().to_string())),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(10.0))
                                .text_color(if offline {
                                    theme.warning.opacity(0.8)
                                } else {
                                    theme.text_muted.opacity(0.45)
                                })
                                .child(SharedString::from(device)),
                        )
                    },
                ))
                .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .child(self.search_box(&theme))
            .child(body)
            .into_any_element()
    }

    fn on_search_submit(&mut self, cx: &mut Context<Self>) {
        if self.open == Some(PickerKind::Branch)
            && let Some(row) = self.filtered_ref_rows(cx).into_iter().nth(self.active)
        {
            self.pick_ref(row, cx);
        } else if self.open == Some(PickerKind::Space)
            && let Some(space) = self.filtered_space_rows(cx).into_iter().nth(self.active)
        {
            self.pick_space(space.id, cx);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &Window, cx: &mut Context<Self>) {
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        let search_focused = self.search.read(cx).focus_handle(cx).is_focused(window);
        match key {
            MenuKey::Escape => {
                self.open = None;
                cx.notify();
            }
            MenuKey::Up | MenuKey::Down => {
                let delta = if key == MenuKey::Up { -1 } else { 1 };
                let count = match self.open {
                    Some(PickerKind::Branch) => self.filtered_ref_rows(cx).len().min(MAX_REF_ROWS),
                    Some(PickerKind::Checkout) => 2,
                    // Keyboard nav walks the MODEL list only; the traits
                    // chips below (reasoning ladder, model options) are
                    // mouse-only.
                    Some(PickerKind::HarnessModel) => self.model_rows_len(cx),
                    Some(PickerKind::Space) => self.filtered_space_rows(cx).len(),
                    Some(PickerKind::Traits | PickerKind::Usage) => 0,
                    None => 0,
                };
                self.active = popover::menu_step(Some(self.active), count, delta).unwrap_or(0);
                // Keep the highlighted MODEL row in view (the rows are the
                // scroll container's direct children, so indices map 1:1);
                // the traits chips below live in the pinned tray and never
                // need scrolling into view.
                if self.open == Some(PickerKind::HarnessModel)
                    && self.active < self.model_rows_len(cx)
                {
                    self.model_scroll.scroll_to_item(self.active);
                }
                cx.notify();
            }
            MenuKey::Enter if !search_focused => {
                if self.open == Some(PickerKind::HarnessModel) {
                    self.activate_model_row(cx);
                } else if self.open == Some(PickerKind::Checkout) {
                    let kind = if self.active == 0 {
                        CheckoutKind::Local
                    } else {
                        CheckoutKind::NewWorktree
                    };
                    self.pick_checkout(kind, cx);
                } else {
                    self.on_search_submit(cx);
                }
            }
            _ => {}
        }
    }

    // ---- render ----

    fn trigger_chip(
        &self,
        kind: PickerKind,
        label: SharedString,
        set: bool,
        chip_icon: Option<(&'static str, Option<gpui::Hsla>)>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id: &'static str = match kind {
            PickerKind::Branch => "picker-branch",
            PickerKind::Checkout => "picker-checkout",
            PickerKind::HarnessModel => "picker-model",
            PickerKind::Traits => "picker-traits",
            PickerKind::Space => "picker-space",
            PickerKind::Usage => "composer-usage",
        };
        let open = self.open == Some(kind);
        // The adjacent model/traits pair uses a 4px inner edge inset so the
        // visible content gap matches the rest of the 4px action rhythm. The
        // model keeps its 10px leading inset around the brand mark.
        let (pad_left, pad_right) = match kind {
            PickerKind::HarnessModel => (10.0, Theme::SPACE_XS),
            PickerKind::Traits => (Theme::SPACE_XS, Theme::SPACE_XS),
            _ => (10.0, 10.0),
        };
        // Ghost pill: 32px high with 12px medium muted text, 16px icons, and
        // hover/open wash; no border or caret.
        div()
            .id(id)
            .debug_selector(move || format!("{id}-bounds"))
            .h(px(32.0))
            .min_w_0()
            .max_w(px(208.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .pl(px(pad_left))
            .pr(px(pad_right))
            .rounded(px(8.0))
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            // The wash and text brighten over 150ms.
            .text_color(motion::hover_blend(
                id,
                if set {
                    theme.text.opacity(0.9)
                } else {
                    theme.text_muted
                },
                theme.text,
            ))
            .bg(if open {
                theme.element_hover
            } else {
                motion::hover_blend(id, gpui::transparent_black(), theme.element_hover)
            })
            .on_hover(motion::hover_listener(id))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| this.toggle(kind, window, cx)))
            .when_some(chip_icon, |el, (path, tint)| {
                el.child(
                    crate::icons::icon(path)
                        .size(px(16.0))
                        .text_color(tint.unwrap_or(theme.text_muted)),
                )
            })
            .child(
                div()
                    .debug_selector(move || format!("{id}-label-bounds"))
                    .min_w_0()
                    .truncate()
                    .child(label),
            )
    }

    /// The empty new-session canvas's inline target-space link. It owns its
    /// popover state but selects through the shared app state, so the composer
    /// follows without changing the sidebar filter.
    pub(crate) fn render_new_chat_space_link(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let label = {
            let state = self.state.read(cx);
            state.selected_space_row().map(|space| {
                let (device, _) = state.space_device_tag(space, chrono::Utc::now());
                SharedString::from(format!("{} {device}", space.display_name()))
            })
        }?;
        let link = div()
            .id("new-chat-space-selector")
            .cursor_pointer()
            .hover(|style| style.text_color(theme.text_muted.opacity(0.9)))
            .on_click(cx.listener(|this, _, window, cx| this.toggle(PickerKind::Space, window, cx)))
            .child(div().underline().child(label));
        if self.open != Some(PickerKind::Space) {
            return Some(link.into_any_element());
        }
        let content = self.render_space_popover(cx);
        let popover = self.popover_frame(300.0, content, cx);
        Some(
            link.relative()
                .child(popover::anchored_menu_below(
                    "new-chat-space-popover",
                    popover,
                ))
                .into_any_element(),
        )
    }

    /// A footer-row trigger (t3code ghost `Button size="xs"`): leading icon,
    /// truncating label, trailing chevron — smaller and quieter than the
    /// in-pill chips.
    fn footer_chip(
        &self,
        kind: PickerKind,
        id: &'static str,
        icon_path: &'static str,
        label: SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let open = self.open == Some(kind);
        div()
            .id(id)
            .h(px(20.0))
            .max_w(px(280.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(motion::hover_blend(
                id,
                theme.text_muted.opacity(0.7),
                theme.text.opacity(0.8),
            ))
            .bg(if open {
                theme.element_hover
            } else {
                motion::hover_blend(id, gpui::transparent_black(), theme.element_hover)
            })
            .on_hover(motion::hover_listener(id))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| this.toggle(kind, window, cx)))
            .child(
                crate::icons::icon(icon_path)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(div().min_w_0().truncate().child(label))
            .child(
                crate::icons::icon(crate::icons::CHEVRON_DOWN)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.5)),
            )
    }

    /// A read-only footer label (locked sessions — t3code's
    /// `resolveLockedWorkspaceLabel` span).
    fn footer_label(icon_path: &'static str, label: SharedString, theme: &Theme) -> gpui::Div {
        div()
            .h(px(20.0))
            .max_w(px(280.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted.opacity(0.6))
            .child(
                crate::icons::icon(icon_path)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.6)),
            )
            .child(div().min_w_0().truncate().child(label))
    }

    fn checkout_review_indicator(&self, theme: &Theme) -> Option<gpui::Stateful<gpui::Div>> {
        let review = self.checkout_review.as_ref()?;
        let number = review.number;
        let url = review.url.clone();
        Some(
            div()
                .id(("checkout-review", number))
                .h(px(20.0))
                .flex()
                .items_center()
                .gap(px(4.0))
                .px(px(4.0))
                .text_size(px(12.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.success)
                .cursor_pointer()
                .hover(|style| style.opacity(0.72))
                .on_click(move |_, _, cx| cx.open_url(&url))
                .child(
                    crate::icons::icon(crate::icons::GIT_PULL_REQUEST)
                        .size(px(12.0))
                        .text_color(theme.success),
                )
                .child(format!("#{number}")),
        )
    }

    fn usage_indicator(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let usage = self.state.read(cx).selected_usage.clone();
        let ring_fraction = usage.as_ref().and_then(UsageSummary::context_fraction);
        let color = theme.text_muted.opacity(0.6);
        let open = self.open == Some(PickerKind::Usage);
        let popover = open.then(|| {
            let content = div()
                .id("usage-popover-content")
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx)))
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(cx.new(|_| UsagePopover { usage }))
                .into_any_element();
            popover::anchored_menu_above("usage-popover", content)
        });
        div()
            .id("composer-usage")
            .debug_selector(|| "composer-usage-bounds".into())
            .relative()
            .size(px(20.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            .bg(if open {
                theme.element_hover
            } else {
                motion::hover_blend(
                    "composer-usage",
                    gpui::transparent_black(),
                    theme.element_hover,
                )
            })
            .on_hover(motion::hover_listener("composer-usage"))
            .on_click(cx.listener(|this, _, window, cx| this.toggle(PickerKind::Usage, window, cx)))
            .children(popover)
            .child(usage_progress_ring(
                ring_fraction,
                color,
                theme.text_muted.opacity(0.18),
            ))
    }

    /// Render the context-window control independently from the picker bar.
    ///
    /// The compact composer gives the established model/thinking picker an
    /// intrinsic-width flex slot. Keeping this control as its next sibling
    /// preserves that sizing contract instead of making the picker entity
    /// responsible for an additional child.
    pub(crate) fn render_usage(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        self.usage_indicator(&theme, cx).into_any_element()
    }

    /// The composer footer row (t3code BranchToolbar): checkout-kind on the
    /// left, the ref selector right-aligned. `None` for non-VCS spaces. On an
    /// existing session both sides are read-only labels ("Worktree" /
    /// "Local checkout" + the chat's branch).
    pub fn render_footer(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        // A selected chat whose workspace row hasn't synced yet (the moment
        // right after send mints it) still renders the DRAFT footer — the
        // values are identical, so the toolbar never blinks through a
        // half-empty locked state.
        let (space, session) = {
            let state = self.state.read(cx);
            let space = state.selected_space_row().cloned()?;
            let session = state
                .selected_chat
                .as_ref()
                .and_then(|_| state.selected_chat_row().cloned());
            (space, session)
        };
        if !space.git_detected {
            return None;
        }
        let new_chat = session.is_none();

        // Refs feed both modes (draft labels, mid-session switch list) —
        // eager + idempotent. Existing sessions also resolve their host-side
        // forge review association.
        self.ensure_refs(false, cx);
        if session.is_some() {
            self.ensure_checkout_review(false, cx);
        } else {
            self.invalidate_checkout_review();
        }

        // Symmetric: the container's 8px gap sits above the toolbar; bleeding
        // 8 of the container's 16px bottom padding (mb -8) leaves 8 below —
        // equal air on both sides of the row.
        let row = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .px(px(10.0))
            .mb(px(-8.0));

        // The ref side is LIVE in both modes: draft pick on a new chat,
        // checkout switch on an existing session (t3code keeps its branch
        // selector interactive mid-session too).
        let ref_label = match &session {
            Some(_) => self
                .selected_ref_name(cx)
                .map(SharedString::from)
                .unwrap_or_else(|| SharedString::from("Select ref")),
            None => self.ref_label(),
        };
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.open {
            Some(PickerKind::Branch) => {
                let content = self.render_branch_popover(cx);
                Some((PickerKind::Branch, self.popover_frame(320.0, content, cx)))
            }
            Some(PickerKind::Checkout) if new_chat => {
                let content = self.render_checkout_popover(cx);
                Some((PickerKind::Checkout, self.popover_frame(224.0, content, cx)))
            }
            _ => None,
        };
        let ref_chip = self.footer_chip(
            PickerKind::Branch,
            "picker-branch",
            crate::icons::GIT_BRANCH,
            ref_label,
            &theme,
            cx,
        );
        let ref_side =
            attach_overlay_end(ref_chip, &mut overlay, PickerKind::Branch, "branch-popover");
        let ref_side = div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .when_some(self.checkout_review_indicator(&theme), |element, review| {
                element.child(review)
            })
            .child(ref_side);

        if let Some(chat) = &session {
            // The checkout KIND is fixed at creation (harness resume is
            // cwd-scoped — the session never moves folders): label only. JJ
            // checkouts are working copies/workspaces, never Git worktrees.
            let session_ref = self.session_ref(chat, &space);
            let jujutsu = session_ref
                .is_some_and(|row| row.kind == jolt_proto::RepoRefKind::WorkingCopy)
                || self.refs.ready().is_some_and(|refs| {
                    refs.iter()
                        .any(|row| row.kind != jolt_proto::RepoRefKind::Branch)
                });
            let outside_space = chat.cwd.as_deref().is_some_and(|cwd| cwd != space.path);
            let is_secondary_checkout = match session_ref {
                Some(row) if row.kind == jolt_proto::RepoRefKind::WorkingCopy => !row.current,
                Some(_) | None => outside_space,
            };
            let (icon_path, label) = if jujutsu && is_secondary_checkout {
                (crate::icons::FOLDERS, "Workspace")
            } else if jujutsu {
                (crate::icons::FOLDER, "Working copy")
            } else if is_secondary_checkout {
                (crate::icons::FOLDERS, "Worktree")
            } else {
                (crate::icons::FOLDER, "Local checkout")
            };
            let left = div()
                .flex()
                .items_center()
                .gap(px(2.0))
                .child(Self::footer_label(
                    icon_path,
                    SharedString::from(label),
                    &theme,
                ));
            return Some(row.child(left).child(ref_side).into_any_element());
        }

        let kind_icon = match (self.config.checkout, self.selected_ref_worktree().is_some()) {
            (CheckoutKind::Local, false) => crate::icons::FOLDER,
            _ => crate::icons::FOLDERS,
        };
        let kind_chip = self.footer_chip(
            PickerKind::Checkout,
            "picker-checkout",
            kind_icon,
            SharedString::from(self.checkout_label()),
            &theme,
            cx,
        );
        let kind_side = div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .child(attach_overlay(
                kind_chip,
                &mut overlay,
                PickerKind::Checkout,
                "checkout-popover",
            ));
        Some(row.child(kind_side).child(ref_side).into_any_element())
    }

    fn popover_frame(&self, width: f32, content: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        popover::popover_card(&theme)
            .w(px(width))
            // jolt caps its tallest picker at min(640px, 75vh).
            .max_h(px(640.0))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx)))
            .flex()
            .flex_col()
            .child(content)
            .into_any_element()
    }

    /// [`Self::popover_frame`] without an inset so the harness/model picker's
    /// rail and list panes bleed to the card edge.
    fn popover_frame_flush(
        &self,
        width: f32,
        content: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        popover::popover_card_flush(&theme)
            .w(px(width))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx)))
            .flex()
            .flex_col()
            .child(content)
            .into_any_element()
    }

    fn search_box(&self, theme: &Theme) -> AnyElement {
        popover::search_input_frame(theme, self.search.clone().into_any_element())
            .into_any_element()
    }

    fn retry_row(
        &self,
        id: &'static str,
        message: &str,
        kind: PickerKind,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        popover::error_row(theme, message)
            .child(
                div()
                    .id(id)
                    .px(px(Theme::SPACE_SM))
                    .py(px(3.0))
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .border_1()
                    .border_color(theme.border)
                    .text_color(theme.text)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| match kind {
                        PickerKind::Branch | PickerKind::Checkout => this.ensure_refs(true, cx),
                        PickerKind::HarnessModel | PickerKind::Traits => {
                            this.harnesses = Loadable::Idle;
                            this.models.clear();
                            this.ensure_harnesses(cx);
                        }
                        PickerKind::Space | PickerKind::Usage => {}
                    }))
                    .child(SharedString::from("Retry")),
            )
            .into_any_element()
    }

    /// The ref picker (t3code BranchToolbarBranchSelector): search on top,
    /// rows with right-aligned muted `current`/`worktree` tags, and a
    /// "Showing X of Y refs" footer when the list is capped.
    /// Combined harness/model switcher; existing chats keep other harnesses disabled.
    fn render_harness_model_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let locked = self.harness_locked(cx);
        let effective = self.effective_harness(cx);
        let model_scroll = self.model_scroll.clone();

        let rail: AnyElement = match &self.harnesses {
            Loadable::Loading | Loadable::Idle => div()
                .p(px(4.0))
                .child(popover::skeleton_rows(
                    "harness-skeleton",
                    &theme,
                    3,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            Loadable::Error(message) => {
                let message = message.clone();
                self.retry_row(
                    "harness-retry",
                    &message,
                    PickerKind::HarnessModel,
                    &theme,
                    cx,
                )
            }
            Loadable::Ready(list) => {
                let mut descriptors: Vec<HarnessDescriptor> = visible_harnesses(list);
                // The committed harness always gets its rail tab, even when
                // it's the (normally hidden) mock harness of a dev session.
                if let Some(effective) = effective
                    && !descriptors.iter().any(|d| d.id == effective)
                    && let Some(descriptor) = list.iter().find(|d| d.id == effective)
                {
                    descriptors.insert(0, descriptor.clone());
                }
                // Vertical agents rail (the palette's Devices-rail language):
                // brand icon + name per row, active carries the glass ring.
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .p(px(4.0))
                    .child(popover::menu_heading(&theme, "Agents"))
                    .children(descriptors.into_iter().enumerate().map(|(ix, descriptor)| {
                        let harness = descriptor.id;
                        let is_viewed = effective == Some(harness);
                        let is_disabled = locked && !is_viewed;
                        let (icon_path, tint) = harness_brand_icon(harness);
                        let name: SharedString = descriptor.name.clone().into();
                        div()
                            .id(("harness-tab", ix))
                            .h(px(30.0))
                            .px(px(8.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .rounded(px(8.0))
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(if is_viewed {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .when(is_viewed, |el| {
                                el.bg(crate::theme::card_selected_bg())
                                    .shadow(crate::theme::card_selected_shadows())
                            })
                            .when(is_disabled, |el| el.opacity(0.35))
                            .when(!is_disabled, |el| el.cursor_pointer())
                            // Hover must not replace the viewed row's selected
                            // fill with the weaker wash — that dims the active
                            // row under the pointer (same rule as the sidebar
                            // rows in shell.rs).
                            .when(!is_disabled && !is_viewed, |el| {
                                el.hover(|s| s.bg(crate::theme::ink(0.06)))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.pick_harness(harness, cx);
                            }))
                            .child(
                                crate::icons::icon(icon_path)
                                    .size(px(16.0))
                                    .flex_none()
                                    .text_color(tint.unwrap_or(if is_viewed {
                                        theme.text
                                    } else {
                                        theme.text_muted
                                    })),
                            )
                            .child(div().min_w_0().truncate().child(name))
                    }))
                    .into_any_element()
            }
        };

        let _ = locked; // the lock still dims foreign rail rows above

        // The rows are collected FLAT — they become the scroll container's
        // direct children so `scroll_to_item(active)` maps 1:1 (the palette's
        // keyboard-follow standard).
        let model_children: Vec<AnyElement> = match effective.map(|h| (h, self.models.get(&h))) {
            Some((_, Some(Loadable::Ready(models)))) => {
                // The check mirrors the chip: the resolved concrete pick (draft
                // / chat config / remembered, else the harness default row).
                let selected = self.selected_model(cx).map(|m| m.id.clone());
                let active = self.active;
                let models = models.clone();
                models
                    .into_iter()
                    .enumerate()
                    .map(|(ix, model)| {
                        let label: SharedString = model.label.clone().into();
                        let description: Option<SharedString> =
                            model.description.clone().map(Into::into);
                        let id = model.id.clone();
                        let is_selected = selected.as_deref() == Some(model.id.as_str())
                            || (selected.is_none() && ix == 0);
                        popover::menu_row_nav(
                            &theme,
                            is_selected,
                            ix == active,
                            format!("model-row-{ix}"),
                        )
                        .when(is_selected || ix == active, |el| {
                            el.shadow(crate::theme::card_selected_shadows())
                        })
                        .id(("model-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pick_model(id.clone(), cx);
                        }))
                        .child(
                            // Name with an 11px muted description subline.
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(div().w_full().truncate().child(label))
                                .when_some(description, |el, description| {
                                    el.child(
                                        div()
                                            .w_full()
                                            .truncate()
                                            .text_size(px(11.0))
                                            .text_color(theme.text_muted.opacity(0.7))
                                            .child(description),
                                    )
                                }),
                        )
                        .into_any_element()
                    })
                    .collect()
            }
            Some((_, Some(Loadable::Error(message)))) => {
                let message = message.clone();
                vec![self.retry_row(
                    "model-retry",
                    &message,
                    PickerKind::HarnessModel,
                    &theme,
                    cx,
                )]
            }
            _ => vec![
                div()
                    .px(px(8.0))
                    .py(px(24.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.6))
                    .text_center()
                    .child(SharedString::from("Loading models…"))
                    .into_any_element(),
            ],
        };

        // The model picker owns only harness/model selection. Traits are kept
        // in their own adjacent picker so each control has one clear job.
        div()
            .h(px(320.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .items_stretch()
                    .child(
                        div()
                            .w(px(148.0))
                            .flex_none()
                            .border_r_1()
                            .border_color(crate::theme::hairline(0.06))
                            .child(rail),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                // Pinned heading (the palette's crumbs slot).
                                div()
                                    .flex_none()
                                    .px(px(4.0))
                                    .pt(px(4.0))
                                    .child(popover::menu_heading(&theme, "Models")),
                            )
                            .child(
                                // Models scroll — gutters on the WRAPPER,
                                // outside the scroll viewport (in-content
                                // bottom padding is eaten by the extent), and
                                // rows as DIRECT children so keyboard
                                // `scroll_to_item` indices line up.
                                div().flex_1().min_h_0().pb(px(4.0)).child(
                                    div()
                                        .id("model-menu-scroll")
                                        .size_full()
                                        .flex()
                                        .flex_col()
                                        .gap(px(2.0))
                                        .px(px(4.0))
                                        .overflow_y_scroll()
                                        .track_scroll(&model_scroll)
                                        .children(model_children),
                                ),
                            ),
                    ),
            )
            .child(
                // The palette's legend footer, on the recessed band.
                div()
                    .flex_none()
                    .bg(popover::band())
                    .border_t_1()
                    .border_color(crate::theme::hairline(0.06))
                    .px(px(12.0))
                    .py(px(8.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .child(popover::key_hint_pair(
                        &theme,
                        crate::icons::ARROW_UP,
                        crate::icons::ARROW_DOWN,
                        "Navigate",
                    ))
                    .child(popover::key_hint(
                        &theme,
                        crate::icons::CORNER_DOWN_LEFT,
                        "Select",
                    )),
            )
            .into_any_element()
    }

    /// The traits dropdown body (Comet PR #29 / t3code TraitsPicker): the
    /// reasoning ladder plus every advertised model option as headed sections
    /// of menu rows. Default choices carry a quiet badge and sections are
    /// separated by hairlines. Selecting keeps the menu open for multi-adjust.
    fn render_traits_sections(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(model) = self.selected_model(cx).cloned() else {
            return popover::skeleton_rows("traits-skeleton", &theme, 3, cx.entity_id(), cx);
        };
        let levels = self.trait_ladder(cx);
        // Display the effective level (draft pick or the chat's config), so
        // the ladder check mirrors the chip summary.
        let current = self.effective_reasoning(cx);

        let mut sections: Vec<AnyElement> = Vec::new();
        if !levels.is_empty() {
            let default_level = default_reasoning(&levels);
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    .child(popover::menu_heading(&theme, "Reasoning"))
                    .children(levels.into_iter().enumerate().map(|(ix, level)| {
                        let is_active = current == Some(level);
                        let is_default = default_level == Some(level);
                        let mut row =
                            popover::menu_row(&theme, is_active, format!("trait-reasoning-{ix}"))
                                .id(("reasoning-row", ix))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_reasoning(level, cx);
                                }))
                                .child(SharedString::from(reasoning_label(level)))
                                .child(div().flex_1());
                        if is_default {
                            row = row.child(default_badge(&theme));
                        }
                        row
                    }))
                    .into_any_element(),
            );
        }

        let selections = self.explicit_options(cx);
        for (opt_ix, option) in model.options.iter().enumerate() {
            if !sections.is_empty() {
                sections.push(popover::menu_separator().into_any_element());
            }
            let selected_choice = selections
                .get(&option.id)
                .and_then(|v| v.as_str())
                .unwrap_or(&option.default_choice)
                .to_string();
            let option_id = option.id.clone();
            let default_choice = option.default_choice.clone();
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    .child(popover::menu_heading(&theme, &option.label))
                    .children(
                        option
                            .choices
                            .iter()
                            .enumerate()
                            .map(|(choice_ix, choice)| {
                                let is_active = selected_choice == choice.id;
                                let choice_id = choice.id.clone();
                                let option_id = option_id.clone();
                                let is_default = choice.id == default_choice;
                                let mut row = popover::menu_row(
                                    &theme,
                                    is_active,
                                    format!("trait-choice-{opt_ix}-{choice_ix}"),
                                )
                                .id(("trait-choice", opt_ix * 32 + choice_ix))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_option(
                                        option_id.clone(),
                                        choice_id.clone(),
                                        is_default,
                                        cx,
                                    );
                                }))
                                .child(SharedString::from(choice.label.clone()))
                                .child(div().flex_1());
                                if is_default {
                                    row = row.child(default_badge(&theme));
                                }
                                row
                            }),
                    )
                    .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_col()
            .pb(px(2.0))
            .children(sections)
            .into_any_element()
    }
}

/// Quiet marker beside a section's default choice, matching Comet PR #29.
fn default_badge(theme: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .text_size(px(10.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text_muted.opacity(0.6))
        .child(SharedString::from("Default"))
}

/// Brand mark + optional tint for a harness (the Claude mark keeps its brand
/// orange even on the monochrome surface; the mock harness scripts
/// Claude-flavoured runs, so it wears the Claude mark).
pub(crate) fn harness_brand_icon(harness: HarnessId) -> (&'static str, Option<gpui::Hsla>) {
    match harness {
        HarnessId::ClaudeCode | HarnessId::Mock => (
            crate::icons::CLAUDE_MARK,
            Some(crate::icons::claude_brand()),
        ),
        HarnessId::Codex => (crate::icons::OPENAI_MARK, None),
        HarnessId::Pi => (crate::icons::PI_MARK, None),
    }
}

/// Display-only 18×32 toggle switch whose knob slides right and whose track
/// flips white when on. State is owned
/// by the parent row.
#[allow(dead_code)]
fn toggle_switch(theme: &Theme, on: bool) -> gpui::Div {
    div()
        .flex_none()
        .w(px(32.0))
        .h(px(18.0))
        .rounded_full()
        .bg(if on {
            theme.text
        } else {
            crate::theme::ink(0.15)
        })
        .relative()
        .child(
            div()
                .absolute()
                .top(px(2.0))
                .left(px(if on { 16.0 } else { 2.0 }))
                .size(px(14.0))
                .rounded_full()
                .bg(if on {
                    theme.on_solid
                } else {
                    crate::theme::ink(0.7)
                }),
        )
}

/// `JOLT_HARNESS=mock` (the e2e/dev rig) opts the mock harness into the UI;
/// production launches never set it, so the mock never surfaces there.
fn mock_harness_enabled() -> bool {
    std::env::var("JOLT_HARNESS").ok().as_deref().map(str::trim) == Some("mock")
}

/// Production pickers AND chip resolution hide the mock harness — the
/// registry always lists it, but it must never surface in real UI (neither in
/// the picker rail nor as the eager default the chips resolve against).
/// `JOLT_HARNESS=mock` shows it; otherwise it only remains when it's
/// literally all there is (a dev build with no real harness registered).
pub fn visible_harnesses(list: &[HarnessDescriptor]) -> Vec<HarnessDescriptor> {
    visible_harnesses_impl(list, mock_harness_enabled())
}

fn visible_harnesses_impl(list: &[HarnessDescriptor], allow_mock: bool) -> Vec<HarnessDescriptor> {
    if allow_mock {
        return list.to_vec();
    }
    let real: Vec<HarnessDescriptor> = list
        .iter()
        .filter(|d| d.id != HarnessId::Mock)
        .cloned()
        .collect();
    if real.is_empty() { list.to_vec() } else { real }
}

/// Resolve the live ref owning a chat checkout. Cwd/current markers take
/// precedence over a potentially stale persisted branch label.
fn session_checkout_ref<'a>(
    refs: &'a [RepoRef],
    branch: Option<&str>,
    cwd: Option<&str>,
    same_checkout: bool,
) -> Option<&'a RepoRef> {
    if let Some(cwd) = cwd
        && let Some(row) = refs
            .iter()
            .find(|row| row.worktree_path.as_deref() == Some(cwd))
    {
        return Some(row);
    }
    if same_checkout && let Some(row) = refs.iter().find(|row| row.current) {
        return Some(row);
    }
    match branch {
        Some(branch) => refs.iter().find(|row| row.name == branch),
        None => refs.iter().find(|row| row.current),
    }
}

/// Attach the (single) open popover overlay to its trigger chip.
fn attach_overlay(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
) -> gpui::Stateful<gpui::Div> {
    if overlay.as_ref().is_some_and(|(k, _)| *k == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip.child(popover::anchored_menu_above(id, element));
    }
    chip
}

/// [`attach_overlay`] with the menu RIGHT-ALIGNED to the trigger (t3code
/// `align="end"` — right-edge triggers like the ref picker open leftward).
fn attach_overlay_end(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
) -> gpui::Stateful<gpui::Div> {
    if overlay.as_ref().is_some_and(|(k, _)| *k == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip
            .relative()
            .child(popover::anchored_menu_above_end(id, element));
    }
    chip
}

impl Pickers {
    /// Render composer controls with either a definite full width (expanded)
    /// or their intrinsic width (compact). A percentage width inside the
    /// compact composer's `flex_none` slot resolves to zero and lets the chip
    /// text paint over later siblings.
    pub(crate) fn render_controls(
        &mut self,
        fill_width: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        // A JOLT_OPEN_PICKER popover never went through `toggle`, so claim
        // its keyboard focus here (re-claim until it sticks — the shell's
        // first-paint fallback focuses the composer after our first render).
        if self.boot_focus_pending {
            match self.open {
                Some(PickerKind::Branch) => {
                    self.search.update(cx, |input, cx| {
                        input.set_placeholder("Search refs…", cx);
                    });
                    let handle = self.search.read(cx).focus_handle(cx);
                    if handle.is_focused(window) {
                        self.boot_focus_pending = false;
                    } else {
                        window.focus(&handle, cx);
                    }
                }
                Some(_) => {
                    if self.focus.is_focused(window) {
                        self.boot_focus_pending = false;
                    } else {
                        window.focus(&self.focus, cx);
                    }
                }
                None => self.boot_focus_pending = false,
            }
        }

        // Eager-load the harness catalog + effective harness's models so the
        // chip reads "Fable 5" (a concrete pick) before any popover opens.
        self.ensure_harnesses(cx);
        if let Some(harness) = self.effective_harness(cx) {
            self.ensure_models(harness, cx);
        }
        // A popover opened data-side (JOLT_OPEN_PICKER) never went through
        // `toggle`, so kick its loads here (all ensure_* are idempotent).
        if matches!(
            self.open,
            Some(PickerKind::Branch) | Some(PickerKind::Checkout)
        ) && matches!(self.refs, Loadable::Idle)
        {
            self.ensure_refs(false, cx);
        }
        // Chip shows the model's display name alone (jolt `modelText`); the
        // harness reads from the brand mark beside it. Never "Default model":
        // before the catalog lands the remembered label (or the configured id)
        // names the pick; the loaded list then resolves it to a concrete row.
        let model_label: SharedString = {
            let loaded = self.selected_model(cx).map(|m| m.label.clone());
            let label = loaded.or_else(|| {
                let remembered = self
                    .effective_harness(cx)
                    .and_then(|h| self.defaults.model_for(h));
                match self.effective_model_id(cx) {
                    Some(id) => Some(
                        remembered
                            .filter(|m| m.id == id)
                            .map(|m| m.label.clone())
                            .or_else(|| self.defaults.label_for(id).map(str::to_string))
                            .unwrap_or_else(|| id.to_string()),
                    ),
                    None => remembered.map(|m| m.label.clone()),
                }
            });
            label.map(SharedString::from).unwrap_or_default()
        };
        let harness_icon: (&'static str, Option<gpui::Hsla>) = self
            .effective_harness(cx)
            .map(harness_brand_icon)
            .unwrap_or((
                crate::icons::CLAUDE_MARK,
                Some(crate::icons::claude_brand()),
            ));
        let explicit_options = self.explicit_options(cx);
        let traits_set = traits_summary(
            self.selected_model(cx),
            self.effective_reasoning(cx),
            &explicit_options,
        );
        let traits_label: SharedString = traits_set
            .clone()
            .map(SharedString::from)
            .unwrap_or_else(|| SharedString::from("Traits"));

        // Render the open popover's body first (mutable borrow), then the
        // chips. Branch/Checkout render in the composer FOOTER row (see
        // `render_footer`), not here.
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.open {
            Some(PickerKind::Branch) | Some(PickerKind::Checkout) => None,
            Some(PickerKind::HarnessModel) => {
                let content = self.render_harness_model_popover(cx);
                Some((
                    PickerKind::HarnessModel,
                    self.popover_frame_flush(460.0, content, cx),
                ))
            }
            Some(PickerKind::Space) => {
                let content = self.render_space_popover(cx);
                Some((PickerKind::Space, self.popover_frame(300.0, content, cx)))
            }
            Some(PickerKind::Traits) => {
                let content = div()
                    .p(px(4.0))
                    .child(self.render_traits_sections(cx))
                    .into_any_element();
                Some((
                    PickerKind::Traits,
                    self.popover_frame_flush(240.0, content, cx),
                ))
            }
            Some(PickerKind::Usage) | None => None,
        };

        // The target-space chip exists only on the new-session canvas.
        let new_chat = self.state.read(cx).selected_chat.is_none();
        let space_label = new_chat
            .then(|| {
                let state = self.state.read(cx);
                state.selected_space_row().map(|space| {
                    let (device, _) = state.space_device_tag(space, chrono::Utc::now());
                    SharedString::from(format!("{} {device}", space.display_name()))
                })
            })
            .flatten();
        let mut left = div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .gap(px(4.0));
        if let Some(label) = space_label {
            let chip = self.trigger_chip(
                PickerKind::Space,
                label,
                true,
                Some((crate::icons::FOLDER, None)),
                &theme,
                cx,
            );
            left = left.child(attach_overlay(
                chip,
                &mut overlay,
                PickerKind::Space,
                "space-popover",
            ));
        }
        let model_chip = self.trigger_chip(
            PickerKind::HarnessModel,
            model_label,
            true,
            Some(harness_icon),
            &theme,
            cx,
        );
        // During catalog loading keep the trigger stable; once a concrete
        // model is known, hide it when that model advertises no traits.
        let has_traits = self
            .selected_model(cx)
            .is_none_or(|model| !self.trait_ladder(cx).is_empty() || !model.options.is_empty());
        let traits_chip = has_traits.then(|| {
            self.trigger_chip(
                PickerKind::Traits,
                traits_label,
                traits_set.is_some(),
                None,
                &theme,
                cx,
            )
        });
        let model_chip = attach_overlay_end(
            model_chip,
            &mut overlay,
            PickerKind::HarnessModel,
            "model-popover",
        );
        let traits_chip = traits_chip.map(|chip| {
            attach_overlay_end(chip, &mut overlay, PickerKind::Traits, "traits-popover")
        });
        // Compact gives this entity a flexible lane. Put any unused width at
        // the lane's START, then keep the natural Model/Traits group pinned to
        // Context at the end. Previously the Traits hit box stretched across
        // that remainder, creating the large visible hole in screenshots.
        if !fill_width {
            return div()
                .debug_selector(|| "picker-controls-compact".into())
                .w_full()
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .justify_end()
                .gap(px(Theme::SPACE_XS))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .justify_end()
                        .child(model_chip.max_w_full()),
                )
                .children(traits_chip.map(|chip| chip.flex_none()))
                .into_any_element();
        }
        let right = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .gap(px(Theme::SPACE_XS))
            // End-anchored: the menu's right edge sits flush with the chip's
            // right edge (user request), same as the footer's ref popover.
            .child(model_chip)
            .children(traits_chip);
        div()
            .debug_selector(|| "picker-controls-expanded".into())
            .w_full()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(Theme::SPACE_SM))
            .child(left)
            .child(right.flex_none())
            .into_any_element()
    }
}

impl Render for Pickers {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_controls(true, window, cx)
    }
}

#[cfg(test)]
mod tests;
