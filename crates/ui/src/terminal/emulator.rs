//! The terminal emulator core: `libghostty_vt` wrapped as a pure state
//! machine.
//!
//! Bytes in ([`Emulator::feed`] — the decoded `SubscribeTerminal` Data frames),
//! grid snapshots out ([`Emulator::snapshot`]). No PTY I/O, timers, or GPUI:
//! the panel owns RPC and scheduling, while the view owns paint. Query
//! responses (DSR/DA/…) are captured through Ghostty's effect callbacks and
//! returned from [`Emulator::feed`] for the panel to write back.

use std::cell::RefCell;
use std::rc::Rc;

use libghostty_vt::fmt::Format;
use libghostty_vt::render::{CellIterator, RowIterator};
use libghostty_vt::screen::{CellWide, TrackedGridRef};
use libghostty_vt::selection::{
    FormatOptions, SelectLineOptions, SelectWordBetweenOptions, Selection,
};
use libghostty_vt::style::{StyleColor, Underline};
use libghostty_vt::terminal::{
    ColorScheme, ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType, Mode,
    Point, PointCoordinate, PrimaryDeviceAttributes, ScrollViewport, SecondaryDeviceAttributes,
    SizeReportSize, TertiaryDeviceAttributes,
};
use libghostty_vt::{RenderState, Terminal, TerminalOptions};

/// Scrollback history kept client-side (lines). The engine's replay window is
/// bounded separately (1 MiB); this only caps what stays scrollable in the UI.
pub const SCROLLBACK_LINES: usize = 10_000;

/// Viewport dimensions in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
}

impl GridSize {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: cols.max(2),
            rows: rows.max(1),
        }
    }
}

/// A cell's paint color, decoupled from the palette: the view resolves these
/// against the theme (default fg/bg, 256-color index, or direct RGB).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellColor {
    /// Default foreground.
    Foreground,
    /// Default background.
    Background,
    /// Indexed color: 0-15 ANSI, 16-231 color cube, 232-255 grayscale ramp.
    Indexed(u8),
    /// Direct 24-bit color.
    Rgb(u8, u8, u8),
}

fn map_style_color(color: StyleColor, default: CellColor) -> CellColor {
    match color {
        StyleColor::None => default,
        StyleColor::Palette(index) => CellColor::Indexed(index.0),
        StyleColor::Rgb(rgb) => CellColor::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

/// One rendered cell: grapheme + colors + the flags paint cares about.
#[derive(Debug, Clone, PartialEq)]
pub struct CellSnapshot {
    /// Full grapheme cluster. Empty terminal cells contain one space.
    pub text: String,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub hidden: bool,
    /// A double-width grapheme (occupies this cell plus the next spacer cell).
    pub wide: bool,
    /// The spacer half of a wide grapheme — never shaped, only background-painted.
    pub wide_spacer: bool,
    /// Whether the cell belongs to the active text selection.
    pub selected: bool,
}

impl CellSnapshot {
    /// Effective paint colors after INVERSE/HIDDEN resolution.
    pub fn display_colors(&self) -> (CellColor, CellColor) {
        let (fg, bg) = if self.inverse {
            (self.bg, self.fg)
        } else {
            (self.fg, self.bg)
        };
        if self.hidden { (bg, bg) } else { (fg, bg) }
    }
}

/// Cursor position in viewport coordinates (row 0 = top of the visible grid).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorSnapshot {
    pub row: usize,
    pub col: usize,
}

#[derive(Debug, Default)]
struct EffectCapture {
    responses: Vec<u8>,
    bell: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionKind {
    Cell,
    Word,
    Line,
}

/// The emulator: a pure fold of PTY bytes into a renderable grid.
pub struct Emulator {
    term: Terminal<'static, 'static>,
    render_state: RenderState<'static>,
    row_iterator: RowIterator<'static>,
    cell_iterator: CellIterator<'static>,
    effects: Rc<RefCell<EffectCapture>>,
    selection_anchor: Option<TrackedGridRef>,
    selection_kind: SelectionKind,
}

impl Emulator {
    pub fn new(cols: u16, rows: u16) -> Self {
        let size = GridSize::new(cols, rows);
        let effects = Rc::new(RefCell::new(EffectCapture::default()));
        let mut term: Terminal<'static, 'static> = Terminal::new(TerminalOptions {
            cols: size.cols,
            rows: size.rows,
            max_scrollback: SCROLLBACK_LINES,
        })
        .expect("libghostty-vt terminal should initialize");

        term.on_pty_write({
            let effects = effects.clone();
            move |_term, data| effects.borrow_mut().responses.extend_from_slice(data)
        })
        .expect("libghostty-vt PTY callback should register");
        term.on_bell({
            let effects = effects.clone();
            move |_term| effects.borrow_mut().bell = true
        })
        .expect("libghostty-vt bell callback should register");
        term.on_title_changed(|_term| {})
            .expect("libghostty-vt title callback should register");
        term.on_xtversion(|_term| Some("Jolt"))
            .expect("libghostty-vt version callback should register");
        term.on_color_scheme(|_term| Some(ColorScheme::Dark))
            .expect("libghostty-vt color-scheme callback should register");
        term.on_size(|term| {
            Some(SizeReportSize {
                rows: term.rows().ok()?,
                columns: term.cols().ok()?,
                cell_width: 0,
                cell_height: 0,
            })
        })
        .expect("libghostty-vt size callback should register");
        term.on_device_attributes(|_term| {
            Some(DeviceAttributes {
                primary: PrimaryDeviceAttributes::new(
                    ConformanceLevel::VT220,
                    &[DeviceAttributeFeature::ANSI_COLOR],
                ),
                secondary: SecondaryDeviceAttributes {
                    device_type: DeviceType::VT220,
                    firmware_version: 1,
                    rom_cartridge: 0,
                },
                tertiary: TertiaryDeviceAttributes::default(),
            })
        })
        .expect("libghostty-vt device-attributes callback should register");

        Self {
            term,
            render_state: RenderState::new().expect("libghostty-vt render state should initialize"),
            row_iterator: RowIterator::new().expect("libghostty-vt row iterator should initialize"),
            cell_iterator: CellIterator::new()
                .expect("libghostty-vt cell iterator should initialize"),
            effects,
            selection_anchor: None,
            selection_kind: SelectionKind::Cell,
        }
    }

    /// Advance the state machine over decoded PTY output. Returns bytes the
    /// terminal wants written back to the PTY (DSR/DA query responses etc.).
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.term.vt_write(bytes);
        std::mem::take(&mut self.effects.borrow_mut().responses)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let size = GridSize::new(cols, rows);
        self.term
            .resize(size.cols, size.rows, 0, 0)
            .expect("valid terminal dimensions should resize");
    }

    pub fn cols(&self) -> usize {
        usize::from(
            self.term
                .cols()
                .expect("libghostty-vt terminal should report columns"),
        )
    }

    pub fn rows(&self) -> usize {
        usize::from(
            self.term
                .rows()
                .expect("libghostty-vt terminal should report rows"),
        )
    }

    /// OSC title, if the running program set one.
    pub fn title(&self) -> Option<&str> {
        let title = self
            .term
            .title()
            .expect("libghostty-vt terminal should report its title");
        (!title.is_empty()).then_some(title)
    }

    /// True once a BEL arrived; reading clears it.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.effects.borrow_mut().bell)
    }

    /// Arrow keys should send SS3 (`ESC O A`) instead of CSI.
    pub fn app_cursor_mode(&self) -> bool {
        self.term.mode(Mode::DECCKM).unwrap_or(false)
    }

    /// Pastes should be wrapped in `ESC [200~` / `ESC [201~`.
    pub fn bracketed_paste_mode(&self) -> bool {
        self.term.mode(Mode::BRACKETED_PASTE).unwrap_or(false)
    }

    /// Lines scrolled back into history (0 = pinned to the live bottom).
    pub fn display_offset(&self) -> usize {
        let scrollbar = self
            .term
            .scrollbar()
            .expect("libghostty-vt terminal should report scrollbar state");
        usize::try_from(
            scrollbar
                .total
                .saturating_sub(scrollbar.len)
                .saturating_sub(scrollbar.offset),
        )
        .unwrap_or(usize::MAX)
    }

    /// Lines available above the viewport.
    pub fn history_lines(&self) -> usize {
        let scrollbar = self
            .term
            .scrollbar()
            .expect("libghostty-vt terminal should report scrollbar state");
        usize::try_from(scrollbar.total.saturating_sub(scrollbar.len)).unwrap_or(usize::MAX)
    }

    /// Scroll the view: positive = up into history, negative = toward live.
    /// Returns whether the visible offset changed.
    pub fn scroll(&mut self, delta: i32) -> bool {
        let before = self.display_offset();
        self.term
            .scroll_viewport(ScrollViewport::Delta((delta as isize).saturating_neg()));
        self.display_offset() != before
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_viewport(ScrollViewport::Bottom);
    }

    // ---- selection ----

    fn viewport_point(&self, row: usize, col: usize) -> Point {
        Point::Viewport(PointCoordinate {
            x: col.min(self.cols().saturating_sub(1)) as u16,
            y: row.min(self.rows().saturating_sub(1)) as u32,
        })
    }

    /// Start a cell, word, or line selection from a pointer press. A single
    /// click records only the anchor; the first drag update creates the
    /// selection so focus clicks do not highlight a stray cell.
    pub fn start_selection(&mut self, row: usize, col: usize, click_count: usize) {
        self.clear_selection();
        let point = self.viewport_point(row, col);
        let Ok(anchor) = self.term.track_grid_ref(point) else {
            return;
        };
        self.selection_anchor = Some(anchor);
        self.selection_kind = match click_count {
            2 => SelectionKind::Word,
            3.. => SelectionKind::Line,
            _ => SelectionKind::Cell,
        };
        if self.selection_kind != SelectionKind::Cell {
            self.update_selection(row, col);
        }
    }

    /// Extend the active selection to a viewport cell. The tracked anchor
    /// follows its text through output, scrollback pruning, and resize.
    pub fn update_selection(&mut self, row: usize, col: usize) {
        let Some(anchor) = self.selection_anchor.as_ref() else {
            return;
        };
        let Ok(Some(anchor)) = anchor.snapshot(&self.term) else {
            return;
        };
        let Ok(current) = self.term.grid_ref(self.viewport_point(row, col)) else {
            return;
        };
        let selection = match self.selection_kind {
            SelectionKind::Cell => Selection::new(anchor, current, false),
            SelectionKind::Word => {
                let Ok(Some(start)) = self.term.select_word_between(SelectWordBetweenOptions::new(
                    anchor.clone(),
                    current.clone(),
                )) else {
                    return;
                };
                let Ok(Some(end)) = self
                    .term
                    .select_word_between(SelectWordBetweenOptions::new(current, anchor))
                else {
                    return;
                };
                Selection::new(start.start(), end.end(), false)
            }
            SelectionKind::Line => {
                let Ok(Some(start)) = self.term.select_line(SelectLineOptions::new(anchor)) else {
                    return;
                };
                let Ok(Some(end)) = self.term.select_line(SelectLineOptions::new(current)) else {
                    return;
                };
                Selection::new(start.start(), end.end(), false)
            }
        };
        let _ = self.term.set_selection(Some(&selection));
    }

    pub fn clear_selection(&mut self) {
        let _ = self.term.set_selection(None);
        self.selection_anchor = None;
    }

    pub fn has_selection(&self) -> bool {
        self.selection_text().is_some()
    }

    pub fn selection_text(&self) -> Option<String> {
        let options = FormatOptions::new()
            .with_emit_format(Format::Plain)
            .with_unwrap(true)
            .with_trim(true);
        let bytes = self
            .term
            .format_selection_alloc(None, options)
            .ok()
            .flatten()?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        (!text.is_empty()).then_some(text)
    }

    /// Snapshot the visible grid and cursor together from one render-state update.
    pub fn snapshot(&mut self) -> (Vec<Vec<CellSnapshot>>, Option<CursorSnapshot>) {
        let snapshot = self
            .render_state
            .update(&self.term)
            .expect("libghostty-vt render state should update");
        let cursor = if snapshot
            .cursor_visible()
            .expect("libghostty-vt cursor visibility should be readable")
        {
            snapshot
                .cursor_viewport()
                .expect("libghostty-vt cursor position should be readable")
                .map(|cursor| CursorSnapshot {
                    row: usize::from(cursor.y),
                    col: usize::from(cursor.x),
                })
        } else {
            None
        };

        let row_count = usize::from(
            snapshot
                .rows()
                .expect("libghostty-vt render state should report rows"),
        );
        let col_count = usize::from(
            snapshot
                .cols()
                .expect("libghostty-vt render state should report columns"),
        );
        let mut lines = Vec::with_capacity(row_count);
        let mut rows = self
            .row_iterator
            .update(&snapshot)
            .expect("libghostty-vt row iterator should update");
        while let Some(row) = rows.next() {
            let mut line = Vec::with_capacity(col_count);
            let mut cells = self
                .cell_iterator
                .update(row)
                .expect("libghostty-vt cell iterator should update");
            while let Some(cell) = cells.next() {
                let style = cell
                    .style()
                    .expect("libghostty-vt cell style should be readable");
                let raw = cell
                    .raw_cell()
                    .expect("libghostty-vt raw cell should be readable");
                let wide = raw
                    .wide()
                    .expect("libghostty-vt cell width should be readable");
                let mut text = String::new();
                cell.graphemes_utf8(&mut text)
                    .expect("libghostty-vt cell grapheme should be readable");
                if text.is_empty() {
                    text.push(' ');
                }
                let bg = match raw
                    .content_tag()
                    .expect("libghostty-vt cell content should be readable")
                {
                    libghostty_vt::screen::CellContentTag::BgColorPalette => CellColor::Indexed(
                        raw.bg_color_palette()
                            .expect("palette background should have an index")
                            .0,
                    ),
                    libghostty_vt::screen::CellContentTag::BgColorRgb => {
                        let color = raw
                            .bg_color_rgb()
                            .expect("RGB background should have a color");
                        CellColor::Rgb(color.r, color.g, color.b)
                    }
                    _ => map_style_color(style.bg_color, CellColor::Background),
                };
                line.push(CellSnapshot {
                    text,
                    fg: map_style_color(style.fg_color, CellColor::Foreground),
                    bg,
                    bold: style.bold,
                    dim: style.faint,
                    italic: style.italic,
                    underline: style.underline != Underline::None,
                    inverse: style.inverse,
                    hidden: style.invisible,
                    wide: wide == CellWide::Wide,
                    wide_spacer: matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead),
                    selected: cell.is_selected().unwrap_or(false),
                });
            }
            lines.push(line);
        }
        while lines.len() < row_count {
            lines.push(Vec::new());
        }
        (lines, cursor)
    }

    /// Snapshot one viewport row (0 = top).
    pub fn line(&mut self, viewport_row: usize) -> Vec<CellSnapshot> {
        self.snapshot()
            .0
            .into_iter()
            .nth(viewport_row)
            .unwrap_or_default()
    }

    /// All viewport rows, top to bottom.
    pub fn lines(&mut self) -> Vec<Vec<CellSnapshot>> {
        self.snapshot().0
    }

    /// Cursor in viewport coordinates; `None` when hidden or scrolled out.
    pub fn cursor(&mut self) -> Option<CursorSnapshot> {
        self.snapshot().1
    }

    /// Test/diagnostic helper: a viewport row as trimmed text (wide-char
    /// spacers skipped).
    pub fn row_text(&mut self, viewport_row: usize) -> String {
        let mut text = String::new();
        for cell in self.line(viewport_row) {
            if !cell.wide_spacer {
                text.push_str(&cell.text);
            }
        }
        while text.ends_with(' ') {
            text.pop();
        }
        text
    }
}

impl std::fmt::Debug for Emulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Emulator")
            .field("cols", &self.cols())
            .field("rows", &self.rows())
            .field("display_offset", &self.display_offset())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emu(cols: u16, rows: u16) -> Emulator {
        Emulator::new(cols, rows)
    }

    #[test]
    fn plain_text_lands_on_row_zero() {
        let mut e = emu(20, 5);
        e.feed(b"hello");
        assert_eq!(e.row_text(0), "hello");
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 0, col: 5 }));
    }

    #[test]
    fn crlf_moves_lines_and_cr_returns_to_column_zero() {
        let mut e = emu(20, 5);
        e.feed(b"one\r\ntwo\r\nthree");
        assert_eq!(e.row_text(0), "one");
        assert_eq!(e.row_text(1), "two");
        assert_eq!(e.row_text(2), "three");
        e.feed(b"\rXX");
        assert_eq!(e.row_text(2), "XXree");
    }

    #[test]
    fn long_line_wraps_at_the_grid_width() {
        let mut e = emu(10, 4);
        e.feed(b"abcdefghijKLM");
        assert_eq!(e.row_text(0), "abcdefghij");
        assert_eq!(e.row_text(1), "KLM");
    }

    #[test]
    fn sgr_colors_and_attributes() {
        let mut e = emu(40, 4);
        e.feed(b"\x1b[31mred\x1b[0m plain \x1b[1;44mboldbg\x1b[0m");
        let line = e.line(0);
        assert_eq!(line[0].fg, CellColor::Indexed(1));
        assert_eq!(line[0].bg, CellColor::Background);
        // After reset: defaults.
        assert_eq!(line[4].fg, CellColor::Foreground);
        // Bold + blue background segment starts at col 10 ("red plain " = 10).
        let bold_cell = &line[10];
        assert!(bold_cell.bold);
        assert_eq!(bold_cell.bg, CellColor::Indexed(4));
    }

    #[test]
    fn bright_256_and_truecolor_sgr() {
        let mut e = emu(40, 2);
        e.feed(b"\x1b[95mA\x1b[38;5;196mB\x1b[38;2;10;20;30mC");
        let line = e.line(0);
        assert_eq!(line[0].fg, CellColor::Indexed(13)); // bright magenta
        assert_eq!(line[1].fg, CellColor::Indexed(196));
        assert_eq!(line[2].fg, CellColor::Rgb(10, 20, 30));
    }

    #[test]
    fn inverse_and_hidden_resolve_in_display_colors() {
        let mut e = emu(10, 2);
        e.feed(b"\x1b[7mI\x1b[0m\x1b[8mH");
        let line = e.line(0);
        let inv = &line[0];
        assert!(inv.inverse);
        assert_eq!(
            inv.display_colors(),
            (CellColor::Background, CellColor::Foreground)
        );
        let hid = &line[1];
        assert!(hid.hidden);
        let (fg, bg) = hid.display_colors();
        assert_eq!(fg, bg, "hidden text paints foreground as background");
    }

    #[test]
    fn cursor_addressing_and_relative_moves() {
        let mut e = emu(20, 6);
        e.feed(b"\x1b[3;5Hx");
        // CSI H is 1-based; cell written at row 2, col 4; cursor advanced by 1.
        assert_eq!(e.line(2)[4].text, "x");
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 2, col: 5 }));
        e.feed(b"\x1b[2D"); // left twice
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 2, col: 3 }));
        e.feed(b"\x1b[A"); // up
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 1, col: 3 }));
    }

    #[test]
    fn clear_screen_and_home() {
        let mut e = emu(20, 4);
        e.feed(b"aaa\r\nbbb\r\nccc");
        e.feed(b"\x1b[2J\x1b[H");
        for row in 0..4 {
            assert_eq!(e.row_text(row), "");
        }
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 0, col: 0 }));
        e.feed(b"fresh");
        assert_eq!(e.row_text(0), "fresh");
    }

    #[test]
    fn erase_line_variants() {
        let mut e = emu(20, 2);
        e.feed(b"abcdef\x1b[3D\x1b[K"); // erase from cursor (col 3) to end
        assert_eq!(e.row_text(0), "abc");
    }

    #[test]
    fn scrollback_history_and_scrolling() {
        let mut e = emu(10, 3);
        for i in 1..=8 {
            e.feed(format!("line{i}\r\n").as_bytes());
        }
        // Viewport shows the tail (line7, line8, then the blank prompt row).
        assert_eq!(e.row_text(0), "line7");
        assert_eq!(e.history_lines(), 6);
        assert_eq!(e.display_offset(), 0);
        // Scroll up into history.
        assert!(e.scroll(2));
        assert_eq!(e.display_offset(), 2);
        assert_eq!(e.row_text(0), "line5");
        // Cursor is below the viewport while scrolled back.
        assert_eq!(e.cursor(), None);
        // Over-scroll clamps to the top of history.
        assert!(e.scroll(100));
        assert_eq!(e.display_offset(), 6);
        assert!(!e.scroll(1), "scrolling past the top is a no-op");
        assert_eq!(e.row_text(0), "line1");
        e.scroll_to_bottom();
        assert_eq!(e.display_offset(), 0);
        assert_eq!(e.row_text(0), "line7");
    }

    #[test]
    fn simple_selection_yields_text_and_marks_cells() {
        let mut e = emu(20, 3);
        e.feed(b"hello world");
        e.start_selection(0, 0, 1);
        assert!(!e.has_selection(), "a focus click alone selects nothing");
        e.update_selection(0, 4);
        assert_eq!(e.selection_text().as_deref(), Some("hello"));
        let line = e.line(0);
        assert!(line[..5].iter().all(|cell| cell.selected));
        assert!(!line[5].selected);
        e.clear_selection();
        assert!(!e.has_selection());
    }

    #[test]
    fn word_and_line_selection_use_terminal_boundaries() {
        let mut e = emu(30, 3);
        e.feed(b"alpha beta gamma\r\nsecond row");
        e.start_selection(0, 7, 2);
        assert_eq!(e.selection_text().as_deref(), Some("beta"));
        e.start_selection(1, 3, 3);
        assert_eq!(e.selection_text().as_deref(), Some("second row"));
    }

    #[test]
    fn selection_anchor_follows_scrolling_output() {
        let mut e = emu(10, 3);
        e.feed(b"target\r\n");
        e.start_selection(0, 0, 1);
        e.update_selection(0, 5);
        assert_eq!(e.selection_text().as_deref(), Some("target"));
        e.feed(b"a\r\nb\r\nc\r\n");
        assert_eq!(e.selection_text().as_deref(), Some("target"));
    }

    #[test]
    fn alt_screen_restores_primary_content() {
        let mut e = emu(20, 4);
        e.feed(b"primary");
        // Enter the alt screen; 1049 keeps the cursor position, so home first.
        e.feed(b"\x1b[?1049h\x1b[H");
        e.feed(b"alt-content");
        assert_eq!(e.row_text(0), "alt-content");
        e.feed(b"\x1b[?1049l"); // leave
        assert_eq!(e.row_text(0), "primary");
    }

    #[test]
    fn dsr_cursor_report_produces_pty_response() {
        let mut e = emu(20, 4);
        e.feed(b"\x1b[2;3H");
        let responses = e.feed(b"\x1b[6n");
        assert_eq!(String::from_utf8_lossy(&responses), "\x1b[2;3R");
    }

    #[test]
    fn osc_title_and_bell() {
        let mut e = emu(20, 2);
        assert_eq!(e.title(), None);
        e.feed(b"\x1b]0;my title\x07");
        assert_eq!(e.title(), Some("my title"));
        assert!(!e.take_bell());
        e.feed(b"\x07");
        assert!(e.take_bell());
        assert!(!e.take_bell(), "bell reads clear it");
    }

    #[test]
    fn app_cursor_and_bracketed_paste_modes_toggle() {
        let mut e = emu(10, 2);
        assert!(!e.app_cursor_mode());
        e.feed(b"\x1b[?1h");
        assert!(e.app_cursor_mode());
        e.feed(b"\x1b[?1l");
        assert!(!e.app_cursor_mode());
        e.feed(b"\x1b[?2004h");
        assert!(e.bracketed_paste_mode());
    }

    #[test]
    fn hidden_cursor_mode() {
        let mut e = emu(10, 2);
        e.feed(b"\x1b[?25l");
        assert_eq!(e.cursor(), None);
        e.feed(b"\x1b[?25h");
        assert!(e.cursor().is_some());
    }

    #[test]
    fn resize_preserves_content_and_reflows_cursor() {
        let mut e = emu(20, 5);
        e.feed(b"keepme\r\nsecond");
        e.resize(30, 3);
        assert_eq!(e.cols(), 30);
        assert_eq!(e.rows(), 3);
        assert_eq!(e.row_text(0), "keepme");
        assert_eq!(e.row_text(1), "second");
    }

    #[test]
    fn wide_chars_occupy_two_cells_with_spacer() {
        let mut e = emu(10, 2);
        e.feed("宽w".as_bytes());
        let line = e.line(0);
        assert!(line[0].wide);
        assert_eq!(line[0].text, "宽");
        assert!(line[1].wide_spacer);
        assert_eq!(line[2].text, "w");
        assert_eq!(e.row_text(0), "宽w");
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 0, col: 3 }));
    }

    #[test]
    fn combining_codepoints_stay_in_one_grapheme_cell() {
        let mut e = emu(10, 2);
        e.feed("e\u{301}x".as_bytes());
        let line = e.line(0);
        assert_eq!(line[0].text, "e\u{301}");
        assert_eq!(line[1].text, "x");
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 0, col: 2 }));
    }

    #[test]
    fn utf8_split_across_feeds_reassembles() {
        let mut e = emu(10, 2);
        let bytes = "é".as_bytes();
        e.feed(&bytes[..1]);
        e.feed(&bytes[1..]);
        assert_eq!(e.row_text(0), "é");
    }
}
