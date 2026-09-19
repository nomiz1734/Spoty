//! Scrollable list state and the on-screen keyboard model.

/// Selection + smoothly animated scroll offset for a vertical list.
#[derive(Clone, Debug, Default)]
pub struct ListState {
    pub sel: usize,
    /// Current scroll offset in pixels (animated).
    pub scroll: f32,
    /// Where the scroll offset is heading.
    pub target: f32,
    /// Animated y of the highlight, in list coordinates.
    pub hl: f32,
    pub row_h: f32,
    pub view_h: f32,
}

impl ListState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_geometry(&mut self, row_h: f32, view_h: f32, len: usize) {
        let changed = self.row_h != row_h || self.view_h != view_h;
        self.row_h = row_h;
        self.view_h = view_h;
        if changed {
            self.sel = self.sel.min(len.saturating_sub(1));
            self.retarget(len);
            self.scroll = self.target;
            self.hl = self.sel as f32 * row_h;
        }
    }

    /// Moves the selection. `wrap` lets a single press go from the last row to the first.
    pub fn move_by(&mut self, delta: i32, len: usize, wrap: bool) {
        if len == 0 {
            return;
        }
        let last = len as i32 - 1;
        let cur = self.sel as i32;
        let mut next = cur + delta;
        if wrap && delta.abs() == 1 {
            if next < 0 {
                next = last;
            } else if next > last {
                next = 0;
            }
        }
        self.set(next.clamp(0, last) as usize, len);
    }

    pub fn set(&mut self, idx: usize, len: usize) {
        self.sel = idx.min(len.saturating_sub(1));
        self.retarget(len);
    }

    fn retarget(&mut self, len: usize) {
        if self.row_h <= 0.0 || self.view_h <= 0.0 {
            return;
        }
        let total = len as f32 * self.row_h;
        let max_scroll = (total - self.view_h).max(0.0);
        let margin = self.row_h.min(self.view_h / 3.0);
        let top = self.sel as f32 * self.row_h;
        let bottom = top + self.row_h;
        let mut t = self.target;
        if top - t < margin {
            t = top - margin;
        }
        if bottom - t > self.view_h - margin {
            t = bottom - self.view_h + margin;
        }
        self.target = t.clamp(0.0, max_scroll);
    }

    /// Advances the animation; returns true while still moving.
    pub fn animate(&mut self, dt: f32) -> bool {
        let k = 1.0 - (-dt * 22.0).exp();
        let hl_target = self.sel as f32 * self.row_h;
        let mut moving = false;
        for (v, t) in [(&mut self.scroll, self.target), (&mut self.hl, hl_target)] {
            let d = t - *v;
            if d.abs() < 0.5 {
                *v = t;
            } else {
                *v += d * k;
                moving = true;
            }
        }
        moving
    }

    /// Range of rows intersecting the viewport.
    pub fn visible(&self, len: usize) -> std::ops::Range<usize> {
        if self.row_h <= 0.0 {
            return 0..len.min(12);
        }
        let first = (self.scroll / self.row_h).floor().max(0.0) as usize;
        let count = (self.view_h / self.row_h).ceil() as usize + 2;
        first.min(len)..(first + count).min(len)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    /// Switches between Vietnamese (Telex) and English typing.
    Lang,
    Space,
    Delete,
    Search,
    /// The clear button inside the text field.
    Clear,
}

pub const KEY_ROWS: [&str; 4] = ["1234567890", "qwertyuiop", "asdfghjkl'", "zxcvbnm-.&"];
/// Bottom row: (key, width in columns).
pub const BOTTOM_ROW: [(Key, usize); 4] =
    [(Key::Lang, 2), (Key::Space, 3), (Key::Delete, 2), (Key::Search, 3)];
pub const KEY_COLS: usize = 10;
const MAX_CHARS: usize = 60;

#[derive(Clone, Debug)]
pub struct Keyboard {
    pub text: String,
    pub row: usize,
    pub col: usize,
    /// Telex input ("tinhf" -> "tình") instead of plain letters.
    pub vi: bool,
    /// Focus is on the clear button in the text field (above the keys).
    pub on_clear: bool,
}

impl Default for Keyboard {
    fn default() -> Self {
        Self::new(true)
    }
}

impl Keyboard {
    pub fn new(vi: bool) -> Self {
        Self { text: String::new(), row: 0, col: 0, vi, on_clear: false }
    }

    pub fn key_at(row: usize, col: usize) -> Key {
        if row < KEY_ROWS.len() {
            Key::Char(KEY_ROWS[row].chars().nth(col).unwrap_or(' '))
        } else {
            let mut start = 0;
            for (k, span) in BOTTOM_ROW {
                if col < start + span {
                    return k;
                }
                start += span;
            }
            BOTTOM_ROW[BOTTOM_ROW.len() - 1].0
        }
    }

    pub fn current(&self) -> Key {
        if self.on_clear {
            Key::Clear
        } else {
            Self::key_at(self.row, self.col)
        }
    }

    /// Column span (start, width) of the key under the cursor.
    pub fn span_of(row: usize, col: usize) -> (usize, usize) {
        if row < KEY_ROWS.len() {
            return (col, 1);
        }
        let mut start = 0;
        for (_, span) in BOTTOM_ROW {
            if col < start + span {
                return (start, span);
            }
            start += span;
        }
        (start, 1)
    }

    pub fn move_by(&mut self, dx: i32, dy: i32) {
        let rows = KEY_ROWS.len() + 1;
        if self.on_clear {
            // The clear button sits above the first row, and below the last one
            // when wrapping around.
            if dy != 0 {
                self.on_clear = false;
                self.row = if dy > 0 { 0 } else { rows - 1 };
            }
            return;
        }
        if dy != 0 {
            let next = self.row as i32 + dy;
            if !self.text.is_empty() && (next < 0 || next >= rows as i32) {
                self.on_clear = true;
                return;
            }
            self.row = next.rem_euclid(rows as i32) as usize;
        }
        if dx != 0 {
            let (start, span) = Self::span_of(self.row, self.col);
            let next = if dx > 0 {
                start + span
            } else {
                start + KEY_COLS - 1
            };
            self.col = next % KEY_COLS;
            // Land on the first column of multi-column keys when moving left.
            let (s, _) = Self::span_of(self.row, self.col);
            if dx < 0 {
                self.col = s;
            }
        }
    }

    /// Types one letter, through Telex in Vietnamese mode.
    pub fn type_char(&mut self, c: char) {
        let next = if self.vi && c.is_ascii_alphabetic() {
            super::telex::apply(&self.text, c)
        } else {
            let mut t = self.text.clone();
            t.push(c);
            t
        };
        if next.chars().count() <= MAX_CHARS {
            self.text = next;
        }
    }

    pub fn space(&mut self) {
        if !self.text.ends_with(' ') && !self.text.is_empty() {
            self.text.push(' ');
        }
    }

    pub fn clear(&mut self) {
        self.text.clear();
        if self.on_clear {
            self.on_clear = false;
            self.row = 0;
        }
    }

    /// Removes the last character; the clear button goes away with the text.
    pub fn backspace(&mut self) {
        self.text.pop();
        if self.text.is_empty() && self.on_clear {
            self.on_clear = false;
            self.row = 0;
        }
    }

    /// Applies the key under the cursor; returns true if the user asked to search.
    pub fn press(&mut self) -> bool {
        match self.current() {
            Key::Char(c) => self.type_char(c),
            Key::Lang => self.vi = !self.vi,
            Key::Space => self.space(),
            Key::Delete => self.backspace(),
            Key::Clear => self.clear(),
            Key::Search => return !self.text.trim().is_empty(),
        }
        false
    }
}

/// Rows of horizontally scrolling cards (the home feed), with animated scrolling
/// on both axes.
#[derive(Clone, Debug, Default)]
pub struct FeedState {
    pub row: usize,
    pub cols: Vec<usize>,
    pub y: f32,
    ty: f32,
    pub xs: Vec<f32>,
    txs: Vec<f32>,
    shelf_h: f32,
    stride: f32,
    view_w: f32,
    view_h: f32,
}

impl FeedState {
    pub fn new() -> Self {
        Self::default()
    }

    fn sync(&mut self, lens: &[usize]) {
        let n = lens.len();
        self.cols.resize(n, 0);
        self.xs.resize(n, 0.0);
        self.txs.resize(n, 0.0);
        for (c, &l) in self.cols.iter_mut().zip(lens) {
            *c = (*c).min(l.saturating_sub(1));
        }
        self.row = self.row.min(n.saturating_sub(1));
    }

    pub fn set_geometry(&mut self, shelf_h: f32, stride: f32, view_w: f32, view_h: f32, lens: &[usize]) {
        let first = self.shelf_h == 0.0;
        self.shelf_h = shelf_h;
        self.stride = stride;
        self.view_w = view_w;
        self.view_h = view_h;
        self.sync(lens);
        self.retarget(lens);
        if first {
            self.y = self.ty;
            self.xs.clone_from(&self.txs);
        }
    }

    pub fn move_row(&mut self, delta: i32, lens: &[usize]) {
        if lens.is_empty() {
            return;
        }
        self.row = (self.row as i32 + delta).clamp(0, lens.len() as i32 - 1) as usize;
        self.retarget(lens);
    }

    pub fn move_col(&mut self, delta: i32, lens: &[usize]) {
        let Some(&len) = lens.get(self.row) else { return };
        if len == 0 {
            return;
        }
        let c = &mut self.cols[self.row];
        *c = (*c as i32 + delta).clamp(0, len as i32 - 1) as usize;
        self.retarget(lens);
    }

    fn retarget(&mut self, lens: &[usize]) {
        if lens.is_empty() || self.shelf_h <= 0.0 {
            return;
        }
        let total = lens.len() as f32 * self.shelf_h;
        self.ty = (self.row as f32 * self.shelf_h).min((total - self.view_h).max(0.0));
        let len = lens[self.row];
        let max_x = (len as f32 * self.stride - self.view_w).max(0.0);
        // Keep one card of context to the left of the selection.
        let col = self.cols[self.row] as f32;
        self.txs[self.row] = ((col - 1.0).max(0.0) * self.stride).min(max_x);
    }

    pub fn animate(&mut self, dt: f32) -> bool {
        let k = 1.0 - (-dt * 20.0).exp();
        let mut moving = false;
        let mut step = |v: &mut f32, t: f32| {
            let d = t - *v;
            if d.abs() < 0.5 {
                *v = t;
            } else {
                *v += d * k;
                moving = true;
            }
        };
        let ty = self.ty;
        step(&mut self.y, ty);
        for i in 0..self.xs.len() {
            let t = self.txs[i];
            step(&mut self.xs[i], t);
        }
        moving
    }

    /// Jumps straight to the targets (screenshots).
    pub fn settle(&mut self) {
        self.y = self.ty;
        self.xs.clone_from(&self.txs);
    }
}
