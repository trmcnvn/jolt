//! Loaders: the jolt pulse loader, dotted activity orb, gradient matrix spinner, and boot
//! splash content. All motion routes through `crate::motion` pure helpers, so
//! the math is unit-tested and these elements are testable-by-compile.
//!
//! Rendering pattern: repeating loaders share leased clocks in `motion` so
//! instances stay phase-locked and stop scheduling when unmounted. The compact
//! activity orb is capped at 15fps; cell loaders animate inside fixed slots.
//! Motion never shifts surrounding layout, while reduced motion snaps every
//! loader to a static frame.

use gpui::{
    AnyElement, App, EntityId, IntoElement, ParentElement, SharedString, Styled, canvas, div, px,
};

use crate::motion::{self, GRADIENT_SPIN, JOLT_PULSE, PULSE_STAGGER, SPLASH_OUT};
use crate::theme::Theme;

// Shared with the terminal viewport (`jolt_proto::motion`) so both animate the
// same loaders from the same numbers.
pub use jolt_proto::motion::{JOLT_CELLS, MATRIX_SIDE};

/// Jolt's lightning mark rendered as a native animated ASCII object.
pub fn jolt_mark_loader(
    _id: &'static str,
    theme: &Theme,
    height_px: f32,
    view: EntityId,
    cx: &mut App,
) -> impl IntoElement {
    crate::ascii_mark::ascii_jolt_mark(
        theme,
        height_px,
        crate::ascii_mark::AsciiMarkMotion::Splash,
        view,
        cx,
    )
}

/// The jolt wave loader: a row of cells pulsing opacity 0.08→1 / scale 0.9→1
/// over 2.4s with a 0.15s stagger per cell.
///
/// `id` scopes the per-cell animation state — give each loader instance a
/// distinct id.
pub fn jolt_loader(
    _id: &'static str,
    theme: &Theme,
    cell_px: f32,
    view: EntityId,
    cx: &mut App,
) -> impl IntoElement {
    let color = theme.text;
    let slot = cell_px;
    let delta = motion::pulse_delta(&JOLT_PULSE, view, cx);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(slot / 2.0))
        .children((0..JOLT_CELLS).map(move |i| {
            // Fixed slot; the animated cell breathes inside it.
            div()
                .size(px(slot))
                .flex()
                .items_center()
                .justify_center()
                .child({
                    let phase = motion::staggered_phase(delta, i, PULSE_STAGGER);
                    div()
                        .rounded(px(slot / 4.0))
                        .bg(color)
                        .opacity(motion::pulse_opacity(phase))
                        .size(px(slot * motion::pulse_scale(phase)))
                })
        }))
}

pub use jolt_proto::motion::{GSPIN_DIM, GSPIN_ROW_TINTS};

/// Display-linked violet activity orb used while work is in progress.
///
/// This is the compact connecting state from `thinking-orbs`: nearby drifting
/// nodes wire themselves into a constellation carrying a bright packet.
pub fn activity_orb(
    key: impl Into<SharedString>,
    theme: &Theme,
    size_px: f32,
    view: EntityId,
    cx: &mut App,
) -> impl IntoElement {
    let _key = key.into();
    let orb_color = theme.code_text;
    let time = if cx.reduce_motion() {
        0.6
    } else {
        motion::activity_elapsed(view, cx) * jolt_proto::motion::ACTIVITY_WEB_SPEED
    };
    let frame = jolt_proto::motion::activity_orb_frame(time, size_px);

    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            for line in &frame.lines {
                let mut builder = gpui::PathBuilder::stroke(px(line.width));
                builder.move_to(gpui::point(
                    bounds.left() + px(size_px * line.x1),
                    bounds.top() + px(size_px * line.y1),
                ));
                builder.line_to(gpui::point(
                    bounds.left() + px(size_px * line.x2),
                    bounds.top() + px(size_px * line.y2),
                ));
                if let Ok(path) = builder.build() {
                    window.paint_path(path, orb_color.opacity(line.opacity));
                }
            }
            for dot in &frame.dots {
                let radius = dot.radius.max(0.3);
                let dot_bounds = gpui::Bounds::new(
                    gpui::point(
                        bounds.left() + px(size_px * dot.x - radius),
                        bounds.top() + px(size_px * dot.y - radius),
                    ),
                    gpui::size(px(radius * 2.0), px(radius * 2.0)),
                );
                window.paint_quad(gpui::quad(
                    dot_bounds,
                    px(radius),
                    orb_color.opacity(dot.opacity * (1.0 - dot.ink)),
                    px(0.0),
                    gpui::transparent_black(),
                    gpui::BorderStyle::default(),
                ));
            }
        },
    )
    .size(px(size_px))
    .flex_none()
}

/// The gradient matrix spinner: a 3×3 grid of round cells tinted per row from
/// the
/// sunrise gradient. Each cell pulses opacity once per 750ms period; the
/// per-cell phase follows the "arrow-up" pattern (the pulse enters at the
/// bottom edge and converges toward the top-center cell), so the wave reads
/// as travelling upward.
pub fn gradient_spinner(
    _id: &'static str,
    _theme: &Theme,
    cell_px: f32,
    view: EntityId,
    cx: &mut App,
) -> impl IntoElement {
    let center = (MATRIX_SIDE as f32 - 1.0) / 2.0;
    let max = MATRIX_SIDE as f32 - 1.0 + center;
    let delta = motion::pulse_delta(&GRADIENT_SPIN, view, cx);
    div()
        .flex()
        .flex_col()
        .gap(px(cell_px / 2.0))
        .children((0..MATRIX_SIDE).map(move |row| {
            let tint: gpui::Hsla = gpui::rgb(GSPIN_ROW_TINTS[row]).into();
            div()
                .flex()
                .flex_row()
                .gap(px(cell_px / 2.0))
                .children((0..MATRIX_SIDE).map(move |col| {
                    // Distance of this cell from the wave origin, normalized
                    // into a phase offset (gradient-spin's `--gspin-phase`).
                    let d = MATRIX_SIDE as f32 - 1.0 - row as f32 + (col as f32 - center).abs();
                    let phase = if max == 0.0 { 0.0 } else { d / (max + 1.0) };
                    div()
                        .size(px(cell_px))
                        .rounded(px(cell_px / 2.0))
                        .bg(tint)
                        .opacity(motion::gspin_opacity(delta + phase, GSPIN_DIM))
                }))
        }))
}

/// A 2×3 miniature of [`gradient_spinner`] sized for compact loading slots:
/// same row tints and pulse timing, but the
/// brightness SNAKES around the grid's perimeter (every cell of a 2×3 grid is
/// on the ring) instead of sweeping as a vertical wave — a tiny radial chase.
/// ~6×10px footprint at the default 2.5px cells.
pub fn mini_gradient_spinner(
    key: impl Into<SharedString>,
    cell_px: f32,
    view: EntityId,
    cx: &mut App,
) -> impl IntoElement {
    const COLS: usize = 2;
    const ROWS: usize = 3;
    /// Clockwise ring position of each `(row, col)` cell, top-left first:
    /// (0,0) → (0,1) → (1,1) → (2,1) → (2,0) → (1,0).
    const RING: [[usize; COLS]; ROWS] = [[0, 1], [5, 2], [4, 3]];
    const RING_LEN: f32 = (COLS * ROWS) as f32;
    let _key = key.into();
    let delta = motion::pulse_delta(&GRADIENT_SPIN, view, cx);
    div()
        .flex()
        .flex_col()
        .gap(px(cell_px / 2.0))
        .children((0..ROWS).map(move |row| {
            let tint: gpui::Hsla = gpui::rgb(GSPIN_ROW_TINTS[row]).into();
            div()
                .flex()
                .flex_row()
                .gap(px(cell_px / 2.0))
                .children((0..COLS).map(move |col| {
                    let phase = RING[row][col] as f32 / RING_LEN;
                    div()
                        .size(px(cell_px))
                        .rounded(px(cell_px / 2.0))
                        .bg(tint)
                        .opacity(motion::gspin_opacity(delta + phase, GSPIN_DIM))
                }))
        }))
}

/// Full-window boot splash: the animated ASCII Jolt mark over the app
/// background with an uppercase tracked "Loading" line.
/// While `fading` it plays `splash-out` (150ms hold, then 0.5s fade + 6px
/// lift); the shell removes it once [`SPLASH_OUT`] has run its course.
pub fn splash_overlay(theme: &Theme, fading: bool, view: EntityId, cx: &mut App) -> AnyElement {
    let content = div()
        .absolute()
        .inset_0()
        .bg(theme.bg)
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(28.0))
        .child(jolt_mark_loader("boot-splash", theme, 144.0, view, cx))
        .child(loading_word(theme));
    if fading {
        motion::splash_out("boot-splash-out", content).into_any_element()
    } else {
        content.into_any_element()
    }
}

/// "L O A D I N G" — `text-[11px] uppercase tracking-[0.32em]
/// text-muted-foreground/70`; tracking approximated with thin spaces (gpui has
/// no letter-spacing at the pinned rev).
pub fn loading_word(theme: &Theme) -> impl IntoElement {
    div()
        .text_size(px(11.0))
        .text_color(theme.text_muted.opacity(0.7))
        .child(SharedString::from(
            "L\u{2009}O\u{2009}A\u{2009}D\u{2009}I\u{2009}N\u{2009}G",
        ))
}

// Compile-time proof the specs referenced here stay wired to the catalog.
const _: () = {
    assert!(SPLASH_OUT.delay_ms == 150);
    assert!(JOLT_PULSE.duration_ms == 2400);
    assert!(GRADIENT_SPIN.duration_ms == 750);
    assert!(jolt_proto::motion::ACTIVITY_WEB_SPEED == 6.63);
};
