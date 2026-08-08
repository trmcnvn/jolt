//! Animated ASCII treatment of Jolt's lightning mark.
//!
//! The app icon stays a clean SVG. Large in-app brand moments sample the same
//! fixed bolt polygon onto a monospace grid, then animate the sampling transform
//! and character density. This keeps the effect native to GPUI: no web view,
//! JavaScript runtime, 3D renderer, or per-character element tree.

use std::f32::consts::TAU;

use gpui::{
    App, Bounds, Element, EntityId, GlobalElementId, Hsla, IntoElement, LayoutId, Pixels,
    ShapedLine, SharedString, Style, TextAlign, TextRun, Window, font, point, px,
};

use crate::motion::{self, JOLT_PULSE};
use crate::theme::Theme;

const COLS: usize = 32;
const ROWS: usize = 19;
const CANVAS: f32 = 440.0;
const CENTER: f32 = CANVAS / 2.0;
const EDGE_WIDTH: f32 = 17.0;

const BOLT: [(f32, f32); 7] = [
    (245.0, 58.0),
    (335.0, 58.0),
    (268.0, 169.0),
    (316.0, 169.0),
    (130.0, 391.0),
    (194.0, 228.0),
    (139.0, 228.0),
];

/// Motion treatment for an in-app ASCII brand mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsciiMarkMotion {
    /// Energetic shimmer and orbiting punctuation for the boot splash.
    Splash,
    /// A quieter rocking watermark for an empty/new session canvas.
    Idle,
}

/// Build a fixed-size, animated ASCII rendering of the Jolt bolt.
pub fn ascii_jolt_mark(
    theme: &Theme,
    size_px: f32,
    mode: AsciiMarkMotion,
    view: EntityId,
    cx: &mut App,
) -> impl IntoElement {
    AsciiJoltMark {
        size_px,
        colors: [theme.accent, theme.code_text, theme.busy],
        motion: mode,
        phase: motion::pulse_delta(&JOLT_PULSE, view, cx),
        font_family: theme.font_mono.clone(),
    }
}

struct AsciiJoltMark {
    size_px: f32,
    colors: [Hsla; 3],
    motion: AsciiMarkMotion,
    phase: f32,
    font_family: SharedString,
}

struct AsciiMarkPrepaint {
    lines: Vec<ShapedLine>,
    line_height: Pixels,
}

impl IntoElement for AsciiJoltMark {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for AsciiJoltMark {
    type RequestLayoutState = ();
    type PrepaintState = AsciiMarkPrepaint;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = px(self.size_px).into();
        style.size.height = px(self.size_px).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let rows = ascii_frame(self.phase, self.motion);
        let line_height = px(self.size_px / ROWS as f32);
        let font_size = line_height * 0.92;
        let mono = font(self.font_family.clone());
        let lines = rows
            .into_iter()
            .enumerate()
            .map(|(row, text)| {
                let position = row as f32 / (ROWS - 1) as f32;
                let mut color = if position < 0.5 {
                    crate::theme::mix(self.colors[0], self.colors[1], position * 2.0)
                } else {
                    crate::theme::mix(self.colors[1], self.colors[2], (position - 0.5) * 2.0)
                };
                color.a *= row_opacity(self.phase, self.motion, row);
                let runs = [TextRun {
                    len: text.len(),
                    font: mono.clone(),
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }];
                window
                    .text_system()
                    .shape_line(SharedString::from(text), font_size, &runs, None)
            })
            .collect();
        AsciiMarkPrepaint { lines, line_height }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for (row, line) in prepaint.lines.iter().enumerate() {
                let _ = line.paint(
                    point(
                        bounds.left(),
                        bounds.top() + prepaint.line_height * row as f32,
                    ),
                    prepaint.line_height,
                    TextAlign::Center,
                    Some(bounds.size.width),
                    window,
                    cx,
                );
            }
        });
    }
}

fn ascii_frame(phase: f32, motion: AsciiMarkMotion) -> Vec<String> {
    let phase = phase.rem_euclid(1.0);
    let energy = match motion {
        AsciiMarkMotion::Splash => 1.0,
        AsciiMarkMotion::Idle => 0.45,
    };
    let angle = (phase * TAU).sin() * (0.025 + 0.025 * energy);
    let scale = 1.0 + (phase * TAU * 2.0).sin() * (0.008 + 0.01 * energy);
    let (sin, cos) = angle.sin_cos();
    let tick = (phase
        * match motion {
            AsciiMarkMotion::Splash => 18.0,
            AsciiMarkMotion::Idle => 9.0,
        })
    .floor() as u32;

    (0..ROWS)
        .map(|row| {
            let mut line = String::with_capacity(COLS);
            for col in 0..COLS {
                let canvas_x = (col as f32 + 0.5) * CANVAS / COLS as f32;
                let canvas_y = (row as f32 + 0.5) * CANVAS / ROWS as f32;
                // Inverse-transform the sample point so the ASCII silhouette
                // rocks without changing the element's layout bounds.
                let x = canvas_x - CENTER;
                let y = canvas_y - CENTER;
                let sample_x = CENTER + (x * cos + y * sin) / scale;
                let sample_y = CENTER + (-x * sin + y * cos) / scale;

                let character = if point_in_bolt(sample_x, sample_y) {
                    let edge = nearest_edge(sample_x, sample_y);
                    if edge.distance < EDGE_WIDTH {
                        edge_character(edge.dx, edge.dy)
                    } else {
                        interior_character(col, row, tick, phase, energy)
                    }
                } else if motion == AsciiMarkMotion::Splash
                    && spark_at(canvas_x, canvas_y, phase, col, row)
                {
                    if (col + row + tick as usize).is_multiple_of(3) {
                        '+'
                    } else {
                        '.'
                    }
                } else {
                    ' '
                };
                line.push(character);
            }
            line
        })
        .collect()
}

fn interior_character(col: usize, row: usize, tick: u32, phase: f32, energy: f32) -> char {
    const DENSITY: [char; 6] = [':', '+', '*', '#', '%', '@'];
    let hash = cell_hash(col as u32, row as u32, tick);
    let shimmer = ((col as f32 * 0.55 - row as f32 * 0.8) + phase * TAU * 2.0).sin();
    let level = 2.3 + shimmer * (0.7 + energy * 0.45) + hash * 0.85;
    DENSITY[(level.round() as i32).clamp(0, DENSITY.len() as i32 - 1) as usize]
}

fn row_opacity(phase: f32, motion: AsciiMarkMotion, row: usize) -> f32 {
    let travel = (phase * TAU * 2.0 - row as f32 * 0.42).sin() * 0.5 + 0.5;
    match motion {
        AsciiMarkMotion::Splash => 0.82 + travel * 0.18,
        AsciiMarkMotion::Idle => 0.54 + travel * 0.24,
    }
}

fn point_in_bolt(x: f32, y: f32) -> bool {
    let mut inside = false;
    for index in 0..BOLT.len() {
        let (x1, y1) = BOLT[index];
        let (x2, y2) = BOLT[(index + 1) % BOLT.len()];
        if (y1 > y) != (y2 > y) && x < (x2 - x1) * (y - y1) / (y2 - y1) + x1 {
            inside = !inside;
        }
    }
    inside
}

#[derive(Clone, Copy)]
struct EdgeSample {
    distance: f32,
    dx: f32,
    dy: f32,
}

fn nearest_edge(x: f32, y: f32) -> EdgeSample {
    let mut nearest = EdgeSample {
        distance: f32::MAX,
        dx: 0.0,
        dy: 0.0,
    };
    for index in 0..BOLT.len() {
        let (x1, y1) = BOLT[index];
        let (x2, y2) = BOLT[(index + 1) % BOLT.len()];
        let dx = x2 - x1;
        let dy = y2 - y1;
        let length_squared = dx * dx + dy * dy;
        let projection = (((x - x1) * dx + (y - y1) * dy) / length_squared).clamp(0.0, 1.0);
        let distance =
            ((x - (x1 + projection * dx)).powi(2) + (y - (y1 + projection * dy)).powi(2)).sqrt();
        if distance < nearest.distance {
            nearest = EdgeSample { distance, dx, dy };
        }
    }
    nearest
}

fn edge_character(dx: f32, dy: f32) -> char {
    if dx.abs() > dy.abs() * 1.8 {
        '_'
    } else if dy.abs() > dx.abs() * 1.8 {
        '|'
    } else if dx * dy > 0.0 {
        '\\'
    } else {
        '/'
    }
}

fn spark_at(x: f32, y: f32, phase: f32, col: usize, row: usize) -> bool {
    (0..5).any(|index| {
        let orbit = phase * TAU + index as f32 * TAU / 5.0;
        let radius_x = 132.0 + index as f32 * 6.0;
        let radius_y = 164.0 - index as f32 * 5.0;
        let spark_x = CENTER + orbit.cos() * radius_x;
        let spark_y = CENTER + (orbit * 1.4).sin() * radius_y;
        let threshold = if (col + row + index).is_multiple_of(2) {
            12.0
        } else {
            8.0
        };
        (x - spark_x).powi(2) + (y - spark_y).powi(2) < threshold * threshold
    })
}

fn cell_hash(col: u32, row: u32, tick: u32) -> f32 {
    let value = col
        .wrapping_mul(0x9E37_79B9)
        .wrapping_add(row.wrapping_mul(0x85EB_CA6B))
        .wrapping_add(tick.wrapping_mul(0xC2B2_AE35));
    (value ^ (value >> 16)) as f32 / u32::MAX as f32 * 2.0 - 1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_is_fixed_ascii_grid() {
        let frame = ascii_frame(0.0, AsciiMarkMotion::Idle);
        assert_eq!(frame.len(), ROWS);
        assert!(frame.iter().all(|row| row.len() == COLS));
        assert!(
            frame
                .iter()
                .flat_map(|row| row.bytes())
                .all(|byte| byte.is_ascii())
        );
    }

    #[test]
    fn animation_is_periodic_and_keeps_a_bolt_silhouette() {
        assert_eq!(
            ascii_frame(0.0, AsciiMarkMotion::Idle),
            ascii_frame(1.0, AsciiMarkMotion::Idle)
        );
        let frame = ascii_frame(0.0, AsciiMarkMotion::Idle);
        let occupied = frame
            .iter()
            .map(|row| row.bytes().filter(|byte| *byte != b' ').count())
            .sum::<usize>();
        assert!((45..=120).contains(&occupied), "occupied={occupied}");
        assert!(point_in_bolt(280.0, 80.0));
        assert!(point_in_bolt(150.0, 350.0));
        assert!(!point_in_bolt(80.0, 80.0));
    }
}
