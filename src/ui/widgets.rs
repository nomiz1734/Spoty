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
    Space,
    Delete,
    Search,
}

pub const KEY_ROWS: [&str; 4] = ["1234567890", "qwertyuiop", "asdfghjkl'", "zxcvbnm-.&"];
/// Bottom row: (key, width in columns).
pub const BOTTOM_ROW: [(Key, usize); 3] = [(Key::Space, 4), (Key::Delete, 3), (Key::Search, 3)];
pub const KEY_COLS: usize = 10;

#[derive(Clone, Debug, Default)]
pub struct Keyboard {
    pub text: String,
    pub row: usize,
    pub col: usize,
}

impl Keyboard {
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
        Self::key_at(self.row, self.col)
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
        if dy != 0 {
            self.row = (self.row as i32 + dy).rem_euclid(rows as i32) as usize;
        }
        if dx != 0 {
            let (start, span) = Self::span_of(self.row, self.col);
            let next = if dx > 0 {
                start + span
            } else {
                start as i32 as usize + KEY_COLS - 1
            };
            self.col = next % KEY_COLS;
            // Land on the first column of multi-column keys when moving left.
            let (s, _) = Self::span_of(self.row, self.col);
            if dx < 0 {
                self.col = s;
            }
        }
    }

    /// Applies the key under the cursor; returns true if the user asked to search.
    pub fn press(&mut self) -> bool {
        match self.current() {
            Key::Char(c) => {
                if self.text.chars().count() < 60 {
                    self.text.push(c);
                }
            }
            Key::Space => {
                if !self.text.ends_with(' ') && !self.text.is_empty() {
                    self.text.push(' ');
                }
            }
            Key::Delete => {
                self.text.pop();
            }
            Key::Search => return !self.text.trim().is_empty(),
        }
        false
    }
}
