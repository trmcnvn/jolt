//! Loaders: the jolt pulse loader, dotted activity orb, gradient matrix spinner, and boot
//! splash content. All motion routes through `crate::motion` pure helpers, so
//! the math is unit-tested and these elements are testable-by-compile.
//!
//! Rendering pattern: repeating loaders share the leased clock in `motion` so
//! instances stay phase-locked and stop scheduling when unmounted. The activity
//! orb uses display-linked canvas paint; cell loaders animate inside fixed
//! slots. Motion is paint-local and never shifts surrounding layout, while
//! reduced motion snaps every loader to a static frame.

use gpui::{
    AnyElement, App, EntityId, IntoElement, ParentElement, SharedString, Styled, canvas, div, px,
};

use crate::motion::{self, ACTIVITY_ORB, GRADIENT_SPIN, JOLT_PULSE, PULSE_STAGGER, SPLASH_OUT};
use crate::theme::Theme;

// Shared with the terminal viewport (`jolt_proto::motion`) so both animate the
// same loaders from the same numbers.
pub use jolt_proto::motion::{JOLT_CELLS, MARK_CELLS, MARK_SPREAD, MATRIX_SIDE, mark_cell_stagger};

/// The animated jolt mark (jolt-loader.tsx `JoltLoader`): the full logo
/// pixel grid with a light wave sweeping tail→head. Each cell rests dim
/// (opacity 0.08, scale 0.9) and flares to full as the crest passes; per-cell
/// stagger follows the flight axis. `height_px` sets the mark's height (width
/// follows the 820:940 canvas).
pub fn jolt_mark_loader(
    _id: &'static str,
    theme: &Theme,
    height_px: f32,
    view: EntityId,
    cx: &mut App,
) -> impl IntoElement {
    let color = theme.text;
    let scale = height_px / 940.0;
    let cell = 100.0 * scale;
    let delta = motion::pulse_delta(&JOLT_PULSE, view, cx);
    div()
        .relative()
        .w(px(820.0 * scale))
        .h(px(height_px))
        .children(MARK_CELLS.iter().map(move |&(x, y)| {
            let stagger = mark_cell_stagger(x, y);
            // Fixed slot; the animated cell breathes inside it (paint-local).
            div()
                .absolute()
                .left(px(x * scale))
                .top(px(y * scale))
                .size(px(cell))
                .flex()
                .items_center()
                .justify_center()
                .child({
                    // Negative CSS delay ⇒ the cell starts mid-cycle:
                    // the stagger ADDS phase (jolt-loader.tsx delayFor).
                    let phase = (delta + stagger).rem_euclid(1.0);
                    div()
                        .rounded(px(16.0 * scale))
                        .bg(color)
                        .opacity(motion::pulse_opacity(phase))
                        .size(px(cell * motion::pulse_scale(phase)))
                })
        }))
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

/// Display-linked dotted activity orb used while work is in progress.
///
/// The violet globe stays geometrically fixed while a bright meridian sweeps
/// around it, avoiding the tiny moving-dot quantization of the solving orb.
pub fn activity_orb(
    key: impl Into<SharedString>,
    theme: &Theme,
    size_px: f32,
    _view: EntityId,
    _cx: &mut App,
) -> impl IntoElement {
    let _key = key.into();
    let compact = size_px <= 10.0;
    let latitude_count = if compact { 4 } else { 6 };
    let longitude_density = if compact { 8 } else { 12 };
    let minimum_radius = if compact { 0.45 } else { 0.4 };
    let orb_color = theme.code_text;

    canvas(
        |_, _, _| (),
        move |bounds, _, window, cx| {
            let phase = motion::display_link_delta(&ACTIVITY_ORB, window, cx);
            let mut dots = Vec::with_capacity(latitude_count * longitude_density);
            for latitude in 0..latitude_count {
                let lat = -std::f32::consts::FRAC_PI_2
                    + latitude as f32 / (latitude_count - 1) as f32 * std::f32::consts::PI;
                let longitude_count = (lat.cos().abs() * longitude_density as f32)
                    .round()
                    .max(1.0) as usize;
                for longitude in 0..longitude_count {
                    dots.push(jolt_proto::motion::activity_orb_dot(
                        phase,
                        latitude,
                        latitude_count,
                        longitude,
                        longitude_count,
                    ));
                }
            }
            dots.sort_by(|a, b| a.depth.total_cmp(&b.depth));

            for dot in dots {
                let radius = (size_px * dot.radius).max(minimum_radius);
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
                    orb_color.opacity(dot.opacity),
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

/// The gradient matrix spinner, ported from jolt's
/// gradient-spin.tsx: a 3×3 grid of round cells tinted per row from the
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

/// Full-window boot splash (jolt App.tsx `Splash`): the animated jolt mark
/// (`h-16`) over the app background with an uppercase tracked "Loading" line.
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
        .child(jolt_mark_loader("boot-splash", theme, 64.0, view, cx))
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
    assert!(ACTIVITY_ORB.duration_ms == 2800);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_stagger_follows_flight_axis() {
        // Tail tip (720, 0) leads: near-maximal stagger (starts deepest into
        // the cycle); head (0, 840) trails with stagger 0.
        let tail = mark_cell_stagger(720.0, 0.0);
        let head = mark_cell_stagger(0.0, 840.0);
        assert!(tail > head, "tail {tail} should lead head {head}");
        assert!((head - 0.0).abs() < 1e-6, "head stagger ≈ 0, got {head}");
        assert!(tail <= MARK_SPREAD + 1e-6, "stagger capped at SPREAD");
        // Every logo cell stays inside [0, SPREAD].
        for &(x, y) in &MARK_CELLS {
            let s = mark_cell_stagger(x, y);
            assert!(
                (0.0..=MARK_SPREAD + 1e-6).contains(&s),
                "cell ({x},{y}) stagger {s}"
            );
        }
    }
}
