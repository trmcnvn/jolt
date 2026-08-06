//! Terminal paint + input encoding.
//!
//! - the ANSI palette on an appearance-aware terminal background and the
//!   256-color cube/grayscale resolution;
//! - keystroke → PTY byte encoding (printables, control keys, arrows/nav
//!   escape sequences, Ctrl- combos, Alt prefixing);
//! - the 12 ms input coalescer and the 80 ms resize debounce constants (the
//!   panel drives the timers; the buffer logic here is pure);
//! - [`TerminalElement`] — a custom gpui element that measures cell metrics
//!   from the real mono font (the "font probe"), reports the resulting
//!   cols×rows back to the panel, and paints the grid: background quads for
//!   non-default cells, one `ShapedLine` per row (same font whatever the
//!   colors — paint never changes layout), and the cursor block.

use gpui::{
    App, Bounds, Entity, GlobalElementId, Hsla, LayoutId, Modifiers, PaintQuad, Pixels, ShapedLine,
    SharedString, Style, TextRun, Window, fill, font, outline, point, px, relative, size,
};

use crate::theme::{Appearance, Theme, rgb_to_hsl};

use super::emulator::{CellColor, CellSnapshot};
use super::panel::TerminalPanel;

/// Terminal font metrics (mono).
pub const TERM_FONT_SIZE: f32 = 13.0;
pub const TERM_LINE_HEIGHT: f32 = 18.0;
/// Inner padding of the grid area.
pub const TERM_PADDING: f32 = 12.0;

/// Keyboard input coalescing window before a `WriteTerminal` flush.
pub const COALESCE_MS: u64 = 12;
/// Debounce for `ResizeTerminal` after viewport-driven size changes.
pub const RESIZE_DEBOUNCE_MS: u64 = 80;

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// Terminal background for an appearance. Dark keeps the reference `#090909`;
/// light steps down from the white content plane to `#fafafa`.
pub fn terminal_bg_for(appearance: Appearance) -> Hsla {
    match appearance {
        Appearance::Dark => rgb8(0x09, 0x09, 0x09),
        Appearance::Light => rgb8(0xfa, 0xfa, 0xfa),
    }
}

pub fn terminal_bg(theme: &Theme) -> Hsla {
    terminal_bg_for(theme.appearance)
}

/// Neutral selection wash that preserves ANSI hue while changing lightness.
pub fn terminal_selection_for(appearance: Appearance) -> Hsla {
    match appearance {
        Appearance::Dark => gpui::hsla(0.0, 0.0, 1.0, 0.22),
        Appearance::Light => gpui::hsla(0.0, 0.0, 0.0, 0.16),
    }
}

/// The 16 ANSI colors tuned for the near-black background (indexes 0-7 normal,
/// 8-15 bright).
const ANSI16_DARK: [(u8, u8, u8); 16] = [
    (0x24, 0x24, 0x24), // black — visible against #090909
    (0xf8, 0x71, 0x71), // red
    (0x4a, 0xde, 0x80), // green
    (0xfa, 0xcc, 0x15), // yellow
    (0x60, 0xa5, 0xfa), // blue
    (0xc0, 0x84, 0xfc), // magenta
    (0x22, 0xd3, 0xee), // cyan
    (0xd4, 0xd4, 0xd8), // white
    (0x52, 0x52, 0x5b), // bright black
    (0xfc, 0xa5, 0xa5), // bright red
    (0x86, 0xef, 0xac), // bright green
    (0xfd, 0xe0, 0x47), // bright yellow
    (0x93, 0xc5, 0xfd), // bright blue
    (0xd8, 0xb4, 0xfe), // bright magenta
    (0x67, 0xe8, 0xf9), // bright cyan
    (0xfa, 0xfa, 0xfa), // bright white
];

/// The same named slots tuned for a light background. Bright variants move
/// darker, not lighter, so they remain more prominent on near-white.
const ANSI16_LIGHT: [(u8, u8, u8); 16] = [
    (0x1f, 0x1f, 0x1f),
    (0xdc, 0x26, 0x26),
    (0x16, 0xa3, 0x4a),
    (0xb4, 0x53, 0x09),
    (0x25, 0x63, 0xeb),
    (0x93, 0x33, 0xea),
    (0x0e, 0x74, 0x90),
    (0x3f, 0x3f, 0x46),
    (0x71, 0x71, 0x7a),
    (0xb9, 0x1c, 0x1c),
    (0x15, 0x80, 0x3d),
    (0x92, 0x40, 0x0e),
    (0x1d, 0x4e, 0xd8),
    (0x7e, 0x22, 0xce),
    (0x15, 0x5e, 0x75),
    (0x18, 0x18, 0x1b),
];

fn rgb8(r: u8, g: u8, b: u8) -> Hsla {
    let (h, s, l) = rgb_to_hsl(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    gpui::hsla(h, s, l, 1.0)
}

/// xterm 256-color cube component levels.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Resolve an indexed color (0-255) for an appearance. Named ANSI colors use
/// appearance-specific tables; the literal color cube stays unchanged; the
/// grayscale ramp reverses in light mode so its dim end remains dim.
pub fn indexed_rgb(appearance: Appearance, index: u8) -> (u8, u8, u8) {
    match index {
        0..=15 => match appearance {
            Appearance::Dark => ANSI16_DARK[index as usize],
            Appearance::Light => ANSI16_LIGHT[index as usize],
        },
        16..=231 => {
            let n = index as usize - 16;
            (
                CUBE_LEVELS[n / 36],
                CUBE_LEVELS[(n / 6) % 6],
                CUBE_LEVELS[n % 6],
            )
        }
        232..=255 => {
            let step = index - 232;
            let step = match appearance {
                Appearance::Dark => step,
                Appearance::Light => 23 - step,
            };
            let value = 8 + 10 * step;
            (value, value, value)
        }
    }
}

/// Resolve a cell color to paint against the theme.
pub fn resolve_color(color: CellColor, theme: &Theme) -> Hsla {
    match color {
        CellColor::Foreground => theme.text,
        CellColor::Background => terminal_bg_for(theme.appearance),
        CellColor::Indexed(ix) => {
            let (r, g, b) = indexed_rgb(theme.appearance, ix);
            rgb8(r, g, b)
        }
        CellColor::Rgb(r, g, b) => rgb8(r, g, b),
    }
}

// ---------------------------------------------------------------------------
// Pointer → cell
// ---------------------------------------------------------------------------

pub const SELECTION_DRAG_THRESHOLD: f32 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellHit {
    pub row: usize,
    pub col: usize,
}

/// Map a position relative to the first grid glyph onto the nearest cell.
pub fn cell_at(x: f32, y: f32, cell_w: f32, line_h: f32, cols: usize, rows: usize) -> CellHit {
    let usable = |value: f32| value.is_finite() && value > 0.0;
    if cols == 0 || rows == 0 || !usable(cell_w) || !usable(line_h) {
        return CellHit { row: 0, col: 0 };
    }
    let x = if x.is_finite() { x } else { 0.0 };
    let y = if y.is_finite() { y } else { 0.0 };
    CellHit {
        row: ((y / line_h).floor().max(0.0) as usize).min(rows - 1),
        col: ((x / cell_w).floor().max(0.0) as usize).min(cols - 1),
    }
}

// ---------------------------------------------------------------------------
// Keyboard → bytes
// ---------------------------------------------------------------------------

/// Encode a keystroke as PTY bytes. `None` means "not ours" — the event should
/// fall through (e.g. the platform-primary shortcuts that drive app actions).
///
/// `app_cursor` switches arrows/home/end from CSI to SS3 per DECCKM.
pub fn keystroke_bytes(
    key: &str,
    key_char: Option<&str>,
    mods: &Modifiers,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    // Platform-primary combos (Cmd on macOS, the super key elsewhere) belong to
    // the app keymap, never the PTY.
    if mods.platform {
        return None;
    }
    if mods.alt {
        // ESC-prefix the same keystroke without alt.
        let inner = keystroke_bytes(
            key,
            key_char,
            &Modifiers {
                alt: false,
                ..*mods
            },
            app_cursor,
        )?;
        let mut out = vec![0x1b];
        out.extend(inner);
        return Some(out);
    }
    if mods.control {
        return control_bytes(key);
    }

    let seq = |csi: &[u8], ss3: &[u8]| {
        Some(if app_cursor {
            ss3.to_vec()
        } else {
            csi.to_vec()
        })
    };
    match key {
        "enter" => Some(b"\r".to_vec()),
        "backspace" => Some(vec![0x7f]),
        "tab" => Some(if mods.shift {
            b"\x1b[Z".to_vec()
        } else {
            b"\t".to_vec()
        }),
        "escape" => Some(vec![0x1b]),
        "space" => Some(b" ".to_vec()),
        "up" => seq(b"\x1b[A", b"\x1bOA"),
        "down" => seq(b"\x1b[B", b"\x1bOB"),
        "right" => seq(b"\x1b[C", b"\x1bOC"),
        "left" => seq(b"\x1b[D", b"\x1bOD"),
        "home" => seq(b"\x1b[H", b"\x1bOH"),
        "end" => seq(b"\x1b[F", b"\x1bOF"),
        "insert" => Some(b"\x1b[2~".to_vec()),
        "delete" => Some(b"\x1b[3~".to_vec()),
        "pageup" => Some(b"\x1b[5~".to_vec()),
        "pagedown" => Some(b"\x1b[6~".to_vec()),
        "f1" => Some(b"\x1bOP".to_vec()),
        "f2" => Some(b"\x1bOQ".to_vec()),
        "f3" => Some(b"\x1bOR".to_vec()),
        "f4" => Some(b"\x1bOS".to_vec()),
        "f5" => Some(b"\x1b[15~".to_vec()),
        "f6" => Some(b"\x1b[17~".to_vec()),
        "f7" => Some(b"\x1b[18~".to_vec()),
        "f8" => Some(b"\x1b[19~".to_vec()),
        "f9" => Some(b"\x1b[20~".to_vec()),
        "f10" => Some(b"\x1b[21~".to_vec()),
        "f11" => Some(b"\x1b[23~".to_vec()),
        "f12" => Some(b"\x1b[24~".to_vec()),
        _ => {
            // Printable: prefer the typed character (IME/shift-aware).
            let text = key_char.filter(|c| !c.is_empty()).or({
                // Fall back to single-char key names ("a", "/", …).
                if key.chars().count() == 1 {
                    Some(key)
                } else {
                    None
                }
            })?;
            Some(text.as_bytes().to_vec())
        }
    }
}

/// Ctrl-key encoding (caret notation).
fn control_bytes(key: &str) -> Option<Vec<u8>> {
    let mut chars = key.chars();
    let (c, rest) = (chars.next()?, chars.next());
    if rest.is_some() {
        return match key {
            "space" => Some(vec![0x00]),
            "backspace" => Some(vec![0x08]),
            "enter" => Some(b"\r".to_vec()),
            _ => None,
        };
    }
    match c {
        'a'..='z' => Some(vec![c as u8 - b'a' + 1]),
        '@' => Some(vec![0x00]),
        '[' => Some(vec![0x1b]),
        '\\' => Some(vec![0x1c]),
        ']' => Some(vec![0x1d]),
        '^' => Some(vec![0x1e]),
        '_' | '/' => Some(vec![0x1f]),
        '?' => Some(vec![0x7f]),
        _ => None,
    }
}

/// Wrap pasted text for the PTY (bracketed-paste aware; strips the one control
/// sequence a paste could inject).
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let sanitized = text.replace("\x1b[201~", "");
    if bracketed {
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(sanitized.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        sanitized.into_bytes()
    }
}

// ---------------------------------------------------------------------------
// Input coalescer (pure buffer; the panel owns the 12 ms timer)
// ---------------------------------------------------------------------------

/// Buffers keyboard bytes between flushes. `push` returns `true` exactly when
/// a flush timer should be scheduled (the buffer was empty), so at most one
/// timer is in flight per burst.
#[derive(Debug, Default)]
pub struct InputCoalescer {
    pending: Vec<u8>,
}

impl InputCoalescer {
    pub fn push(&mut self, bytes: &[u8]) -> bool {
        let was_empty = self.pending.is_empty();
        self.pending.extend_from_slice(bytes);
        was_empty && !self.pending.is_empty()
    }

    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Grid element
// ---------------------------------------------------------------------------

/// Paints the active tab's grid. Cell metrics come from the resolved mono font
/// each frame (font probe): `em_advance` for the cell width, the fixed line
/// height for rows. The measured cols×rows feed back into the panel, which
/// resizes the emulator immediately and debounces the `ResizeTerminal` RPC.
pub struct TerminalElement {
    panel: Entity<TerminalPanel>,
    focused: bool,
}

impl TerminalElement {
    pub fn new(panel: Entity<TerminalPanel>, focused: bool) -> Self {
        Self { panel, focused }
    }
}

pub struct TerminalPrepaint {
    bg_quads: Vec<PaintQuad>,
    selection_quads: Vec<PaintQuad>,
    lines: Vec<ShapedLine>,
    cursor: Option<PaintQuad>,
}

impl gpui::IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl gpui::Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = TerminalPrepaint;

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
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let theme = Theme::of(cx).clone();
        let mono = font(theme.font_terminal.clone());
        // Font probe: measure the actual advance of the resolved mono font so
        // cols/rows track real glyph metrics, not a guessed aspect ratio.
        let font_size = px(f32::from(theme.font_sizes.terminal));
        let font_id = window.text_system().resolve_font(&mono);
        let cell_w = window
            .text_system()
            .em_advance(font_id, font_size)
            .unwrap_or(font_size * 0.6);
        let line_h = px(theme.font_sizes.terminal_line_height());

        let inner_w = f32::from(bounds.size.width) - 2.0 * TERM_PADDING;
        let inner_h = f32::from(bounds.size.height) - 2.0 * TERM_PADDING;
        let cols = ((inner_w / f32::from(cell_w)).floor() as i64).clamp(2, 500) as u16;
        let rows = ((inner_h / f32::from(line_h)).floor() as i64).clamp(1, 500) as u16;

        // Report the measured grid and its current window placement, then
        // snapshot for painting. Pointer selection consumes the same metrics.
        let origin = point(
            bounds.left() + px(TERM_PADDING),
            bounds.top() + px(TERM_PADDING),
        );
        let snapshot = self.panel.update(cx, |panel, cx| {
            panel.on_grid_metrics(
                super::panel::GridGeometry {
                    origin,
                    cell_w: f32::from(cell_w),
                    line_h: f32::from(line_h),
                    cols,
                    rows,
                },
                cx,
            );
            panel.active_grid_snapshot(cx)
        });
        let Some(snapshot) = snapshot else {
            return TerminalPrepaint {
                bg_quads: Vec::new(),
                selection_quads: Vec::new(),
                lines: Vec::new(),
                cursor: None,
            };
        };

        let mut bg_quads = Vec::new();
        let mut selection_quads = Vec::new();
        let mut lines = Vec::with_capacity(snapshot.lines.len());

        for (row_ix, row) in snapshot.lines.iter().enumerate() {
            let y = origin.y + line_h * row_ix as f32;
            let mut selection_start = None;
            for col in 0..=row.len() {
                let selected = row.get(col).is_some_and(|cell| cell.selected);
                match (selection_start, selected) {
                    (None, true) => selection_start = Some(col),
                    (Some(start), false) => {
                        selection_quads.push(fill(
                            Bounds::new(
                                point(origin.x + cell_w * start as f32, y),
                                size(cell_w * (col - start) as f32, line_h),
                            ),
                            terminal_selection_for(theme.appearance),
                        ));
                        selection_start = None;
                    }
                    _ => {}
                }
            }
            // Merge consecutive non-default background cells into quads.
            let mut run_start: Option<(usize, Hsla)> = None;
            for (col, color) in row
                .iter()
                .map(|cell| cell.display_colors().1)
                .chain(std::iter::once(CellColor::Background))
                .enumerate()
            {
                let paint = match color {
                    CellColor::Background => None,
                    other => Some(resolve_color(other, &theme)),
                };
                match (&run_start, paint) {
                    (None, Some(color)) => run_start = Some((col, color)),
                    (Some((start, current)), next) if next != Some(*current) => {
                        bg_quads.push(fill(
                            Bounds::new(
                                point(origin.x + cell_w * *start as f32, y),
                                size(cell_w * (col - *start) as f32, line_h),
                            ),
                            *current,
                        ));
                        run_start = next.map(|color| (col, color));
                    }
                    _ => {}
                }
            }
            lines.push(shape_row(row, &theme, &mono, font_size, window));
        }

        let cursor = snapshot.cursor.map(|c| {
            let cursor_bounds = Bounds::new(
                point(
                    origin.x + cell_w * c.col as f32,
                    origin.y + line_h * c.row as f32,
                ),
                size(cell_w, line_h),
            );
            if self.focused {
                // Translucent block: the glyph underneath stays legible.
                fill(cursor_bounds, theme.cursor)
            } else {
                outline(cursor_bounds, theme.cursor, gpui::BorderStyle::Solid)
            }
        });

        TerminalPrepaint {
            bg_quads,
            selection_quads,
            lines,
            cursor,
        }
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
        let line_h = px(Theme::of(cx).font_sizes.terminal_line_height());
        let origin = point(
            bounds.left() + px(TERM_PADDING),
            bounds.top() + px(TERM_PADDING),
        );
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for quad in prepaint.bg_quads.drain(..) {
                window.paint_quad(quad);
            }
            for quad in prepaint.selection_quads.drain(..) {
                window.paint_quad(quad);
            }
            for (ix, line) in prepaint.lines.iter().enumerate() {
                let _ = line.paint(
                    point(origin.x, origin.y + line_h * ix as f32),
                    line_h,
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        });
    }
}

/// Shape one grid row: wide-char spacers are skipped (the wide glyph covers
/// both columns), attributes map to font weight/style, colors to run colors.
fn shape_row(
    row: &[CellSnapshot],
    theme: &Theme,
    mono: &gpui::Font,
    font_size: Pixels,
    window: &Window,
) -> ShapedLine {
    let mut text = String::with_capacity(row.iter().map(|cell| cell.text.len()).sum());
    let mut runs: Vec<TextRun> = Vec::new();
    for cell in row {
        if cell.wide_spacer {
            continue;
        }
        let grapheme = if cell.hidden {
            if cell.wide { "  " } else { " " }
        } else {
            &cell.text
        };
        let (fg, _) = cell.display_colors();
        let mut color = resolve_color(fg, theme);
        if cell.dim {
            color.a *= 0.6;
        }
        let mut cell_font = mono.clone();
        cell_font.weight = if cell.bold {
            gpui::FontWeight::BOLD
        } else {
            gpui::FontWeight::NORMAL
        };
        cell_font.style = if cell.italic {
            gpui::FontStyle::Italic
        } else {
            gpui::FontStyle::Normal
        };
        let underline = cell.underline.then_some(gpui::UnderlineStyle {
            color: Some(color),
            thickness: px(1.0),
            wavy: false,
        });
        let len = grapheme.len();
        text.push_str(grapheme);
        match runs.last_mut() {
            Some(last)
                if last.color == color && last.font == cell_font && last.underline == underline =>
            {
                last.len += len;
            }
            _ => runs.push(TextRun {
                len,
                font: cell_font,
                color,
                background_color: None,
                underline,
                strikethrough: None,
            }),
        }
    }
    window
        .text_system()
        .shape_line(SharedString::from(text), font_size, &runs, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods() -> Modifiers {
        Modifiers::default()
    }

    #[test]
    fn printables_prefer_key_char() {
        assert_eq!(
            keystroke_bytes("a", Some("a"), &mods(), false),
            Some(b"a".to_vec())
        );
        assert_eq!(
            keystroke_bytes(
                "a",
                Some("A"),
                &Modifiers {
                    shift: true,
                    ..mods()
                },
                false
            ),
            Some(b"A".to_vec())
        );
        // Multi-byte characters pass through as UTF-8.
        assert_eq!(
            keystroke_bytes("e", Some("é"), &mods(), false),
            Some("é".as_bytes().to_vec())
        );
        // Named single-char keys fall back to the key name.
        assert_eq!(
            keystroke_bytes("/", None, &mods(), false),
            Some(b"/".to_vec())
        );
        // Unknown multi-char keys are not ours.
        assert_eq!(keystroke_bytes("capslock", None, &mods(), false), None);
    }

    #[test]
    fn control_keys_and_sequences() {
        assert_eq!(
            keystroke_bytes("enter", None, &mods(), false),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            keystroke_bytes("backspace", None, &mods(), false),
            Some(vec![0x7f])
        );
        assert_eq!(
            keystroke_bytes("tab", None, &mods(), false),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            keystroke_bytes(
                "tab",
                None,
                &Modifiers {
                    shift: true,
                    ..mods()
                },
                false
            ),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            keystroke_bytes("escape", None, &mods(), false),
            Some(vec![0x1b])
        );
        assert_eq!(
            keystroke_bytes("delete", None, &mods(), false),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(
            keystroke_bytes("pageup", None, &mods(), false),
            Some(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            keystroke_bytes("f5", None, &mods(), false),
            Some(b"\x1b[15~".to_vec())
        );
    }

    #[test]
    fn arrows_respect_app_cursor_mode() {
        assert_eq!(
            keystroke_bytes("up", None, &mods(), false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            keystroke_bytes("up", None, &mods(), true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            keystroke_bytes("home", None, &mods(), false),
            Some(b"\x1b[H".to_vec())
        );
        assert_eq!(
            keystroke_bytes("end", None, &mods(), true),
            Some(b"\x1bOF".to_vec())
        );
    }

    #[test]
    fn ctrl_combos_map_to_control_bytes() {
        let ctrl = Modifiers {
            control: true,
            ..mods()
        };
        assert_eq!(
            keystroke_bytes("c", Some("c"), &ctrl, false),
            Some(vec![0x03])
        );
        assert_eq!(keystroke_bytes("z", None, &ctrl, false), Some(vec![0x1a]));
        assert_eq!(
            keystroke_bytes("space", None, &ctrl, false),
            Some(vec![0x00])
        );
        assert_eq!(keystroke_bytes("[", None, &ctrl, false), Some(vec![0x1b]));
        assert_eq!(keystroke_bytes("_", None, &ctrl, false), Some(vec![0x1f]));
        // Ctrl+1 has no caret encoding — not ours.
        assert_eq!(keystroke_bytes("1", Some("1"), &ctrl, false), None);
    }

    #[test]
    fn alt_prefixes_escape() {
        let alt = Modifiers {
            alt: true,
            ..mods()
        };
        assert_eq!(
            keystroke_bytes("b", Some("b"), &alt, false),
            Some(vec![0x1b, b'b'])
        );
        let alt_ctrl = Modifiers {
            alt: true,
            control: true,
            ..mods()
        };
        assert_eq!(
            keystroke_bytes("c", None, &alt_ctrl, false),
            Some(vec![0x1b, 0x03])
        );
    }

    #[test]
    fn platform_primary_combos_fall_through() {
        let cmd = Modifiers {
            platform: true,
            ..mods()
        };
        assert_eq!(keystroke_bytes("j", Some("j"), &cmd, false), None);
        assert_eq!(keystroke_bytes("enter", None, &cmd, false), None);
    }

    #[test]
    fn paste_wraps_when_bracketed() {
        assert_eq!(paste_bytes("hi", false), b"hi".to_vec());
        assert_eq!(paste_bytes("hi", true), b"\x1b[200~hi\x1b[201~".to_vec());
        // Close-bracket injection is stripped.
        assert_eq!(
            paste_bytes("a\x1b[201~rm -rf", true),
            b"\x1b[200~arm -rf\x1b[201~".to_vec()
        );
    }

    #[test]
    fn coalescer_schedules_once_per_burst() {
        let mut c = InputCoalescer::default();
        assert!(c.is_empty());
        assert!(c.push(b"a"), "first push schedules the flush");
        assert!(!c.push(b"b"), "subsequent pushes ride the pending flush");
        assert!(!c.push(b"c"));
        assert_eq!(c.take(), b"abc".to_vec());
        assert!(c.is_empty());
        // Next burst schedules again.
        assert!(c.push(b"d"));
        // Empty pushes never schedule.
        let mut c = InputCoalescer::default();
        assert!(!c.push(b""));
    }

    #[test]
    fn cube_is_appearance_independent() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            assert_eq!(indexed_rgb(appearance, 16), (0, 0, 0));
            assert_eq!(indexed_rgb(appearance, 231), (255, 255, 255));
            assert_eq!(indexed_rgb(appearance, 196), (255, 0, 0));
            assert_eq!(indexed_rgb(appearance, 21), (0, 0, 255));
        }
    }

    #[test]
    fn grayscale_ramp_mirrors_in_light() {
        assert_eq!(indexed_rgb(Appearance::Dark, 232), (8, 8, 8));
        assert_eq!(indexed_rgb(Appearance::Dark, 255), (238, 238, 238));
        assert_eq!(indexed_rgb(Appearance::Light, 232), (238, 238, 238));
        assert_eq!(indexed_rgb(Appearance::Light, 255), (8, 8, 8));
    }

    #[test]
    fn ansi_range_uses_the_appearance_palette() {
        assert_eq!(indexed_rgb(Appearance::Dark, 1), ANSI16_DARK[1]);
        assert_eq!(indexed_rgb(Appearance::Light, 1), ANSI16_LIGHT[1]);
        assert_ne!(ANSI16_DARK, ANSI16_LIGHT);
    }

    #[test]
    fn terminal_bg_follows_appearance() {
        assert_eq!(terminal_bg(&Theme::dark()), rgb8(0x09, 0x09, 0x09));
        assert_eq!(terminal_bg(&Theme::light()), rgb8(0xfa, 0xfa, 0xfa));
    }

    #[test]
    fn pointer_positions_map_and_clamp_to_grid_cells() {
        assert_eq!(
            cell_at(0.0, 0.0, 10.0, 20.0, 8, 4),
            CellHit { row: 0, col: 0 }
        );
        assert_eq!(
            cell_at(25.0, 45.0, 10.0, 20.0, 8, 4),
            CellHit { row: 2, col: 2 }
        );
        assert_eq!(
            cell_at(9_999.0, 9_999.0, 10.0, 20.0, 8, 4),
            CellHit { row: 3, col: 7 }
        );
        assert_eq!(
            cell_at(-50.0, -50.0, 10.0, 20.0, 8, 4),
            CellHit { row: 0, col: 0 }
        );
        assert_eq!(
            cell_at(5.0, 5.0, 0.0, 20.0, 8, 4),
            CellHit { row: 0, col: 0 }
        );
    }

    #[test]
    fn selection_wash_is_neutral_and_translucent() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let wash = terminal_selection_for(appearance);
            assert_eq!(wash.s, 0.0);
            assert!(wash.a > 0.0 && wash.a < 0.5);
        }
    }

    #[test]
    fn timing_constants_match_spec() {
        assert_eq!(COALESCE_MS, 12);
        assert_eq!(RESIZE_DEBOUNCE_MS, 80);
    }
}
