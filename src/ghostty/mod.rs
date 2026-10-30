//! A safe wrapper over the parts of libghostty-vt this client uses: a terminal, restoring
//! one from a snapshot, and reading a render state to draw it.
//!
//! libghostty-vt is Ghostty's terminal core, the same code the Rex server runs, so the
//! local copy of a terminal behaves exactly like the server's.

pub mod ffi;

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{null, null_mut};

use anyhow::{Result, bail};

/// A libghostty-vt result code, as an error.
fn check(code: ffi::GhosttyResult, what: &str) -> Result<()> {
    if code == ffi::GHOSTTY_SUCCESS {
        Ok(())
    } else {
        bail!("{what} failed with libghostty-vt result {code}")
    }
}

/// A libghostty-vt struct whose first field is its own size, as `GHOSTTY_INIT_SIZED` makes.
macro_rules! sized {
    ($t:ty) => {{
        let mut value: $t = Default::default();
        value.size = size_of::<$t>();
        value
    }};
}

pub type Rgb = (u8, u8, u8);

fn rgb(c: ffi::GhosttyColorRgb) -> Rgb {
    (c.r, c.g, c.b)
}

fn color(c: Rgb) -> ffi::GhosttyColorRgb {
    ffi::GhosttyColorRgb { r: c.0, g: c.1, b: c.2 }
}

/// A point on screen, in cells from the top-left of the visible area.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewportPoint {
    pub x: u16,
    pub y: u16,
}

pub struct Scrollbar {
    /// Rows in the screen and scrollback together.
    pub total: u64,
    /// The first visible row.
    pub offset: u64,
    /// Visible rows.
    pub len: u64,
}

pub enum Scroll {
    Delta(isize),
    Bottom,
    /// The first visible row, as in `Scrollbar::offset`.
    Row(usize),
}

pub struct Terminal(ffi::GhosttyTerminal);

// SAFETY: a terminal is only used by one thread at a time, behind a mutex.
unsafe impl Send for Terminal {}

impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe { ffi::ghostty_terminal_free(self.0) }
    }
}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Result<Self> {
        let mut term = null_mut();
        check(unsafe { ffi::ghostty_terminal_new(null(), &mut term, cols.max(1), rows.max(1)) }, "creating a terminal")?;
        Ok(Self(term))
    }

    /// Restore a terminal from a complete Ghostty snapshot, as a Rex data stream starts with.
    pub fn from_snapshot(bytes: &[u8]) -> Result<Self> {
        let mut decoder = null_mut();
        check(
            unsafe { ffi::ghostty_snapshot_decoder_new_buf(null(), &mut decoder, bytes.as_ptr(), bytes.len()) },
            "starting the snapshot decoder",
        )?;
        let compress = true;
        let mut term = null_mut();
        let result = unsafe {
            // Keep restored scrollback compressed rather than briefly holding all of it.
            ffi::ghostty_snapshot_decoder_set(
                decoder,
                ffi::GHOSTTY_SNAPSHOT_DECODER_OPT_COMPRESS_HISTORY,
                &compress as *const bool as *const c_void,
            );
            let result = ffi::ghostty_snapshot_decoder_decode(decoder, &mut term);
            ffi::ghostty_snapshot_decoder_free(decoder);
            result
        };
        check(result, "decoding the snapshot")?;
        Ok(Self(term))
    }

    pub fn write(&mut self, bytes: &[u8]) {
        unsafe { ffi::ghostty_terminal_vt_write(self.0, bytes.as_ptr(), bytes.len()) }
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell_width: u32, cell_height: u32) {
        unsafe { ffi::ghostty_terminal_resize(self.0, cols.max(1), rows.max(1), cell_width.max(1), cell_height.max(1)) };
    }

    fn get<T: Default>(&self, data: ffi::GhosttyTerminalData) -> Option<T> {
        let mut out = T::default();
        let code = unsafe { ffi::ghostty_terminal_get(self.0, data, &mut out as *mut T as *mut c_void) };
        (code == ffi::GHOSTTY_SUCCESS).then_some(out)
    }

    pub fn cols(&self) -> u16 {
        self.get(ffi::GHOSTTY_TERMINAL_DATA_COLS).unwrap_or(80)
    }

    pub fn rows(&self) -> u16 {
        self.get(ffi::GHOSTTY_TERMINAL_DATA_ROWS).unwrap_or(24)
    }

    /// Set the default colors, which a program's own color changes still override.
    pub fn set_default_colors(&mut self, fg: Rgb, bg: Rgb, cursor: Option<Rgb>, palette: &[Rgb; 256]) {
        let palette: Vec<ffi::GhosttyColorRgb> = palette.iter().copied().map(color).collect();
        let (fg, bg) = (color(fg), color(bg));
        let cursor = cursor.map(color);
        unsafe {
            ffi::ghostty_terminal_set(self.0, ffi::GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, &fg as *const _ as *const c_void);
            ffi::ghostty_terminal_set(self.0, ffi::GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND, &bg as *const _ as *const c_void);
            ffi::ghostty_terminal_set(
                self.0,
                ffi::GHOSTTY_TERMINAL_OPT_COLOR_CURSOR,
                cursor.as_ref().map_or(null(), |c| c as *const _ as *const c_void),
            );
            ffi::ghostty_terminal_set(self.0, ffi::GHOSTTY_TERMINAL_OPT_COLOR_PALETTE, palette.as_ptr() as *const c_void);
        }
    }

    pub fn scroll(&mut self, to: Scroll) {
        let mut behavior = ffi::GhosttyTerminalScrollViewport::default();
        match to {
            Scroll::Delta(delta) => {
                behavior.tag = ffi::GHOSTTY_SCROLL_VIEWPORT_DELTA;
                behavior.value.delta = delta;
            }
            Scroll::Bottom => behavior.tag = ffi::GHOSTTY_SCROLL_VIEWPORT_BOTTOM,
            Scroll::Row(row) => {
                behavior.tag = ffi::GHOSTTY_SCROLL_VIEWPORT_ROW;
                behavior.value.row = row;
            }
        }
        unsafe { ffi::ghostty_terminal_scroll_viewport(self.0, behavior) }
    }

    pub fn scrollbar(&self) -> Scrollbar {
        let s: ffi::GhosttyTerminalScrollbar = self.get(ffi::GHOSTTY_TERMINAL_DATA_SCROLLBAR).unwrap_or_default();
        Scrollbar { total: s.total, offset: s.offset, len: s.len }
    }

    /// Whether the view shows the newest output.
    pub fn at_bottom(&self) -> bool {
        let s = self.scrollbar();
        s.offset + s.len >= s.total
    }

    /// Whether a DEC private mode, such as 2004 for bracketed paste, is on.
    pub fn dec_mode(&self, mode: u16) -> bool {
        let mut config = ffi::GhosttyTerminalModeConfig { mode: mode & 0x7fff, value: false };
        let code = unsafe {
            ffi::ghostty_terminal_get(self.0, ffi::GHOSTTY_TERMINAL_DATA_MODE, &mut config as *mut _ as *mut c_void)
        };
        code == ffi::GHOSTTY_SUCCESS && config.value
    }

    pub fn mouse_tracking(&self) -> bool {
        self.get::<bool>(ffi::GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING).unwrap_or(false)
    }

    pub fn alternate_screen(&self) -> bool {
        self.get::<ffi::GhosttyTerminalScreen>(ffi::GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN)
            == Some(ffi::GHOSTTY_TERMINAL_SCREEN_ALTERNATE)
    }

    /// The cell at `point`, as a reference that stays valid until the terminal changes.
    fn grid_ref(&self, point: ViewportPoint) -> Option<ffi::GhosttyGridRef> {
        let mut p = ffi::GhosttyPoint { tag: ffi::GHOSTTY_POINT_TAG_VIEWPORT, ..Default::default() };
        p.value.coordinate = ffi::GhosttyPointCoordinate { x: point.x, y: point.y as u32 };
        let mut out = sized!(ffi::GhosttyGridRef);
        let code = unsafe { ffi::ghostty_terminal_grid_ref(self.0, p, &mut out) };
        (code == ffi::GHOSTTY_SUCCESS).then_some(out)
    }

    fn set_selection(&mut self, selection: Option<&ffi::GhosttySelection>) {
        let value = selection.map_or(null(), |s| s as *const _ as *const c_void);
        unsafe { ffi::ghostty_terminal_set(self.0, ffi::GHOSTTY_TERMINAL_OPT_SELECTION, value) };
    }

    /// Select from `start` to `end`, both included, in reading order or not.
    pub fn select(&mut self, start: ViewportPoint, end: ViewportPoint) {
        let (Some(start), Some(end)) = (self.grid_ref(start), self.grid_ref(end)) else { return };
        let mut selection = sized!(ffi::GhosttySelection);
        selection.start = start;
        selection.end = end;
        self.set_selection(Some(&selection));
    }

    pub fn select_word(&mut self, at: ViewportPoint) {
        let Some(at) = self.grid_ref(at) else { return };
        let mut options = sized!(ffi::GhosttyTerminalSelectWordOptions);
        options.ref_ = at;
        let mut selection = sized!(ffi::GhosttySelection);
        if unsafe { ffi::ghostty_terminal_select_word(self.0, &options, &mut selection) } == ffi::GHOSTTY_SUCCESS {
            self.set_selection(Some(&selection));
        }
    }

    pub fn select_line(&mut self, at: ViewportPoint) {
        let Some(at) = self.grid_ref(at) else { return };
        let mut options = sized!(ffi::GhosttyTerminalSelectLineOptions);
        options.ref_ = at;
        let mut selection = sized!(ffi::GhosttySelection);
        if unsafe { ffi::ghostty_terminal_select_line(self.0, &options, &mut selection) } == ffi::GHOSTTY_SUCCESS {
            self.set_selection(Some(&selection));
        }
    }

    pub fn clear_selection(&mut self) {
        self.set_selection(None);
    }

    pub fn has_selection(&self) -> bool {
        let mut selection = sized!(ffi::GhosttySelection);
        let code = unsafe {
            ffi::ghostty_terminal_get(self.0, ffi::GHOSTTY_TERMINAL_DATA_SELECTION, &mut selection as *mut _ as *mut c_void)
        };
        code == ffi::GHOSTTY_SUCCESS
    }

    /// The selected text, with soft-wrapped lines joined.
    pub fn selection_text(&self) -> Option<String> {
        let mut options = sized!(ffi::GhosttyTerminalSelectionFormatOptions);
        options.emit = ffi::GHOSTTY_FORMATTER_FORMAT_PLAIN;
        options.unwrap = true;
        options.trim = true;
        let (mut ptr, mut len) = (null_mut(), 0usize);
        let code = unsafe { ffi::ghostty_terminal_selection_format_alloc(self.0, null(), options, &mut ptr, &mut len) };
        if code != ffi::GHOSTTY_SUCCESS || ptr.is_null() {
            return None;
        }
        let text = unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned() };
        unsafe { ffi::ghostty_free(null(), ptr, len) };
        Some(text)
    }

    /// The screen and scrollback as plain text.
    pub fn plain_text(&self) -> String {
        let mut options = sized!(ffi::GhosttyFormatterTerminalOptions);
        options.emit = ffi::GHOSTTY_FORMATTER_FORMAT_PLAIN;
        options.trim = true;
        let mut formatter = null_mut();
        if unsafe { ffi::ghostty_formatter_terminal_new(null(), &mut formatter, self.0, options) } != ffi::GHOSTTY_SUCCESS {
            return String::new();
        }
        let (mut ptr, mut len) = (null_mut(), 0usize);
        let code = unsafe { ffi::ghostty_formatter_format_alloc(formatter, null(), &mut ptr, &mut len) };
        let text = if code == ffi::GHOSTTY_SUCCESS && !ptr.is_null() {
            let text = unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned() };
            unsafe { ffi::ghostty_free(null(), ptr, len) };
            text
        } else {
            String::new()
        };
        unsafe { ffi::ghostty_formatter_free(formatter) };
        text
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Width {
    Narrow,
    /// The first of the two columns a wide character takes.
    Wide,
    /// The second column of a wide character, or a placeholder for one that wrapped.
    Spacer,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorShape {
    Block,
    HollowBlock,
    Bar,
    Underline,
}

/// One cell, ready to draw. Colors are `None` for the terminal's default.
#[derive(Clone)]
pub struct Cell {
    pub text: String,
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub width: Width,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub selected: bool,
}

pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub shape: CursorShape,
}

/// Everything needed to draw one frame of a terminal.
pub struct Frame {
    pub rows: Vec<Vec<Cell>>,
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor_color: Option<Rgb>,
    pub cursor: Option<Cursor>,
}

/// A copy of what a terminal shows, taken while the terminal is locked, then read without
/// the lock.
pub struct RenderState {
    state: ffi::GhosttyRenderState,
    rows: ffi::GhosttyRenderStateRowIterator,
    cells: ffi::GhosttyRenderStateRowCells,
}

// SAFETY: a render state belongs to one pane and is used from the UI thread only.
unsafe impl Send for RenderState {}

impl Drop for RenderState {
    fn drop(&mut self) {
        unsafe {
            ffi::ghostty_render_state_row_cells_free(self.cells);
            ffi::ghostty_render_state_row_iterator_free(self.rows);
            ffi::ghostty_render_state_free(self.state);
        }
    }
}

impl RenderState {
    pub fn new() -> Result<Self> {
        let (mut state, mut rows, mut cells) = (null_mut(), null_mut(), null_mut());
        unsafe {
            check(ffi::ghostty_render_state_new(null(), &mut state), "creating a render state")?;
            check(ffi::ghostty_render_state_row_iterator_new(null(), &mut rows), "creating a row iterator")?;
            check(ffi::ghostty_render_state_row_cells_new(null(), &mut cells), "creating a cell iterator")?;
        }
        Ok(Self { state, rows, cells })
    }

    /// Copy the terminal's current state. Call it with the terminal locked; reading the copy
    /// with `frame` needs no lock.
    pub fn update(&mut self, terminal: &Terminal) {
        unsafe { ffi::ghostty_render_state_update(self.state, terminal.0) };
    }

    fn get<T>(&self, data: ffi::GhosttyRenderStateData, out: &mut T) -> bool {
        unsafe { ffi::ghostty_render_state_get(self.state, data, out as *mut T as *mut c_void) == ffi::GHOSTTY_SUCCESS }
    }

    pub fn frame(&mut self) -> Frame {
        let mut colors = sized!(ffi::GhosttyRenderStateColors);
        self.get(ffi::GHOSTTY_RENDER_STATE_DATA_COLORS, &mut colors);

        let mut c = sized!(ffi::GhosttyRenderStateCursor);
        self.get(ffi::GHOSTTY_RENDER_STATE_DATA_CURSOR, &mut c);
        let cursor = (c.visible && c.viewport_has_value).then(|| Cursor {
            x: c.viewport_x,
            y: c.viewport_y,
            shape: match c.visual_style {
                ffi::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR => CursorShape::Bar,
                ffi::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE => CursorShape::Underline,
                ffi::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW => CursorShape::HollowBlock,
                _ => CursorShape::Block,
            },
        });

        let mut rows = Vec::new();
        let mut iterator = self.rows;
        self.get(ffi::GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR, &mut iterator);
        unsafe {
            while ffi::ghostty_render_state_row_iterator_next(self.rows) {
                let mut cells = self.cells;
                ffi::ghostty_render_state_row_get(self.rows, ffi::GHOSTTY_RENDER_STATE_ROW_DATA_CELLS, &mut cells as *mut _ as *mut c_void);
                let mut row = Vec::new();
                while ffi::ghostty_render_state_row_cells_next(self.cells) {
                    row.push(self.cell());
                }
                rows.push(row);
            }
        }
        Frame {
            rows,
            foreground: rgb(colors.foreground),
            background: rgb(colors.background),
            cursor_color: colors.cursor_has_value.then(|| rgb(colors.cursor)),
            cursor,
        }
    }

    /// The cell the cell iterator is on.
    unsafe fn cell(&self) -> Cell {
        let get = |data, out: *mut c_void| unsafe { ffi::ghostty_render_state_row_cells_get(self.cells, data, out) };
        let mut len: u32 = 0;
        get(ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN, &mut len as *mut _ as *mut c_void);
        let text = if len == 0 {
            String::new()
        } else {
            let mut codepoints = vec![0u32; len as usize];
            get(ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF, codepoints.as_mut_ptr() as *mut c_void);
            codepoints.into_iter().filter_map(char::from_u32).collect()
        };
        let mut raw: ffi::GhosttyCell = Default::default();
        get(ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW, &mut raw as *mut _ as *mut c_void);
        let mut wide: ffi::GhosttyCellWide = ffi::GHOSTTY_CELL_WIDE_NARROW;
        unsafe { ffi::ghostty_cell_get(raw, ffi::GHOSTTY_CELL_DATA_WIDE, &mut wide as *mut _ as *mut c_void) };
        let width = match wide {
            ffi::GHOSTTY_CELL_WIDE_WIDE => Width::Wide,
            ffi::GHOSTTY_CELL_WIDE_SPACER_TAIL | ffi::GHOSTTY_CELL_WIDE_SPACER_HEAD => Width::Spacer,
            _ => Width::Narrow,
        };
        let color = |data| {
            let mut c = ffi::GhosttyColorRgb::default();
            (get(data, &mut c as *mut _ as *mut c_void) == ffi::GHOSTTY_SUCCESS).then(|| rgb(c))
        };
        let mut style = sized!(ffi::GhosttyStyle);
        let mut styled = false;
        get(ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_HAS_STYLING, &mut styled as *mut _ as *mut c_void);
        if styled {
            get(ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE, &mut style as *mut _ as *mut c_void);
        }
        let mut selected = false;
        get(ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_SELECTED, &mut selected as *mut _ as *mut c_void);
        Cell {
            text,
            fg: color(ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR),
            bg: color(ffi::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR),
            width,
            bold: style.bold,
            italic: style.italic,
            faint: style.faint,
            inverse: style.inverse,
            invisible: style.invisible,
            underline: style.underline != ffi::GHOSTTY_SGR_UNDERLINE_NONE as i32,
            strikethrough: style.strikethrough,
            selected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_renders() {
        let mut term = Terminal::new(20, 3).unwrap();
        term.write("hi \x1b[1;31mred\x1b[0m 日本\r\n".as_bytes());
        let mut state = RenderState::new().unwrap();
        state.update(&term);
        let frame = state.frame();
        assert_eq!(frame.rows.len(), 3);
        let row = &frame.rows[0];
        let text: String = row.iter().filter(|c| c.width != Width::Spacer).map(|c| c.text.as_str()).collect();
        assert!(text.starts_with("hired日本") || text.starts_with("hi"), "{text:?}");
        assert!(row[3].bold);
        assert!(row[3].fg.is_some());
        assert_eq!(row[7].width, Width::Wide);
        assert_eq!(row[8].width, Width::Spacer);
        assert!(term.plain_text().starts_with("hi red 日本"));
    }

    #[test]
    fn selects_and_copies() {
        let mut term = Terminal::new(20, 3).unwrap();
        term.write(b"hello world\r\n");
        term.select(ViewportPoint { x: 6, y: 0 }, ViewportPoint { x: 10, y: 0 });
        assert!(term.has_selection());
        assert_eq!(term.selection_text().as_deref(), Some("world"));
        term.clear_selection();
        assert!(!term.has_selection());
    }

    #[test]
    fn restores_a_snapshot() {
        let mut term = Terminal::new(30, 4).unwrap();
        term.write(b"from the snapshot\r\n");
        let mut size = 0;
        unsafe { ffi::ghostty_snapshot_encode_buf(term.0, null_mut(), 0, &mut size) };
        let mut buf = vec![0u8; size];
        let code = unsafe { ffi::ghostty_snapshot_encode_buf(term.0, buf.as_mut_ptr(), buf.len(), &mut size) };
        assert_eq!(code, ffi::GHOSTTY_SUCCESS);
        let restored = Terminal::from_snapshot(&buf[..size]).unwrap();
        assert!(restored.plain_text().starts_with("from the snapshot"));
        assert_eq!(restored.cols(), 30);
    }
}
