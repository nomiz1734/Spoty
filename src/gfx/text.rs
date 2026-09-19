//! Text rendering with fontdue and a glyph cache.

use std::collections::HashMap;
use std::path::Path;

use fontdue::{Font, FontSettings};
use unicode_normalization::UnicodeNormalization;

use super::canvas::{Canvas, Color, Mask};

static REGULAR_TTF: &[u8] = include_bytes!("../../assets/fonts/NotoSans-Regular.ttf");
static BOLD_TTF: &[u8] = include_bytes!("../../assets/fonts/NotoSans-Bold.ttf");

const MAX_CACHED_GLYPHS: usize = 6000;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Weight {
    Regular,
    Bold,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct GlyphKey {
    ch: char,
    size: u16,
    weight: Weight,
}

struct Glyph {
    mask: Mask,
    xmin: i32,
    /// Distance from the baseline to the top of the bitmap.
    top: i32,
    advance: f32,
}

pub struct Fonts {
    regular: Font,
    bold: Font,
    fallback: Vec<Font>,
    cache: HashMap<GlyphKey, Glyph>,
}

impl Fonts {
    /// Loads the embedded fonts plus any extra .ttf/.otf found in `extra_dir`
    /// (for example a CJK font), used for characters the main font lacks.
    pub fn load(extra_dir: &Path) -> Self {
        let settings = FontSettings::default();
        let regular = Font::from_bytes(REGULAR_TTF, settings).expect("embedded regular font");
        let bold = Font::from_bytes(BOLD_TTF, settings).expect("embedded bold font");
        let mut fallback = Vec::new();
        if let Ok(entries) = std::fs::read_dir(extra_dir) {
            let mut files: Vec<_> = entries.flatten().map(|e| e.path()).collect();
            files.sort();
            for path in files {
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if ext != "ttf" && ext != "otf" {
                    continue;
                }
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with("NotoSans-") {
                    continue; // already embedded
                }
                match std::fs::read(&path).map(|b| Font::from_bytes(b, settings)) {
                    Ok(Ok(f)) => {
                        log::info!("loaded fallback font {}", path.display());
                        fallback.push(f);
                    }
                    _ => log::warn!("could not load font {}", path.display()),
                }
            }
        }
        Self {
            regular,
            bold,
            fallback,
            cache: HashMap::new(),
        }
    }

    fn primary(&self, weight: Weight) -> &Font {
        match weight {
            Weight::Regular => &self.regular,
            Weight::Bold => &self.bold,
        }
    }

    /// Ascent in pixels: add to the top of a line to get its baseline.
    pub fn ascent(&self, size: f32) -> f32 {
        self.regular
            .horizontal_line_metrics(size)
            .map(|m| m.ascent)
            .unwrap_or(size * 0.8)
    }

    /// Height of a line of text (ascent + descent).
    pub fn line_height(&self, size: f32) -> f32 {
        self.regular
            .horizontal_line_metrics(size)
            .map(|m| m.ascent - m.descent)
            .unwrap_or(size * 1.2)
    }

    fn glyph(&mut self, ch: char, size: f32, weight: Weight) -> &Glyph {
        let key = GlyphKey {
            ch,
            size: (size * 4.0) as u16,
            weight,
        };
        if !self.cache.contains_key(&key) {
            if self.cache.len() > MAX_CACHED_GLYPHS {
                self.cache.clear();
            }
            let font = {
                let primary = self.primary(weight);
                if ch == ' ' || primary.lookup_glyph_index(ch) != 0 {
                    primary
                } else {
                    self.fallback
                        .iter()
                        .find(|f| f.lookup_glyph_index(ch) != 0)
                        .unwrap_or(primary)
                }
            };
            let (m, bitmap) = font.rasterize(ch, size);
            let glyph = Glyph {
                mask: Mask {
                    w: m.width,
                    h: m.height,
                    a: bitmap,
                },
                xmin: m.xmin,
                top: m.height as i32 + m.ymin,
                advance: m.advance_width,
            };
            self.cache.insert(key, glyph);
        }
        &self.cache[&key]
    }

    pub fn measure(&mut self, text: &str, size: f32, weight: Weight) -> f32 {
        text.chars().map(|c| self.glyph(c, size, weight).advance).sum()
    }

    /// Draws `text` with its top-left at (x, y). Returns the advance width.
    pub fn draw(
        &mut self,
        canvas: &mut Canvas,
        x: i32,
        y: i32,
        text: &str,
        size: f32,
        weight: Weight,
        color: Color,
    ) -> f32 {
        let baseline = y as f32 + self.ascent(size);
        let mut pen = x as f32;
        for ch in text.chars() {
            let g = self.glyph(ch, size, weight);
            if g.mask.w > 0 {
                let gx = (pen + g.xmin as f32).round() as i32;
                let gy = (baseline - g.top as f32).round() as i32;
                canvas.draw_mask(gx, gy, &g.mask, color, 255);
            }
            pen += g.advance;
        }
        pen - x as f32
    }

    /// Shortens `text` with an ellipsis so it fits in `max_w`.
    pub fn ellipsize(&mut self, text: &str, size: f32, weight: Weight, max_w: f32) -> String {
        if self.measure(text, size, weight) <= max_w {
            return text.to_string();
        }
        let ell = self.measure("…", size, weight);
        let mut w = 0.0;
        let mut out = String::new();
        for ch in text.chars() {
            let a = self.glyph(ch, size, weight).advance;
            if w + a + ell > max_w {
                break;
            }
            w += a;
            out.push(ch);
        }
        let trimmed = out.trim_end().to_string();
        trimmed + "…"
    }

    pub fn draw_fit(
        &mut self,
        canvas: &mut Canvas,
        x: i32,
        y: i32,
        max_w: i32,
        text: &str,
        size: f32,
        weight: Weight,
        color: Color,
    ) -> f32 {
        let t = self.ellipsize(text, size, weight, max_w as f32);
        self.draw(canvas, x, y, &t, size, weight, color)
    }

    /// Draws text right-aligned so that it ends at `right`.
    pub fn draw_right(
        &mut self,
        canvas: &mut Canvas,
        right: i32,
        y: i32,
        text: &str,
        size: f32,
        weight: Weight,
        color: Color,
    ) {
        let w = self.measure(text, size, weight);
        self.draw(canvas, right - w.round() as i32, y, text, size, weight, color);
    }

    pub fn draw_centered(
        &mut self,
        canvas: &mut Canvas,
        cx: i32,
        y: i32,
        text: &str,
        size: f32,
        weight: Weight,
        color: Color,
    ) {
        let w = self.measure(text, size, weight);
        self.draw(canvas, cx - (w / 2.0).round() as i32, y, text, size, weight, color);
    }

    /// Word-wraps text into at most `max_lines` lines; the last one is ellipsized.
    pub fn wrap(
        &mut self,
        text: &str,
        size: f32,
        weight: Weight,
        max_w: f32,
        max_lines: usize,
    ) -> Vec<String> {
        if max_lines <= 1 {
            return vec![self.ellipsize(text, size, weight, max_w)];
        }
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        let words: Vec<&str> = text.split(' ').collect();
        let mut i = 0;
        while i < words.len() {
            let word = words[i];
            let candidate = if current.is_empty() {
                word.to_string()
            } else {
                format!("{current} {word}")
            };
            if self.measure(&candidate, size, weight) <= max_w || current.is_empty() {
                current = candidate;
                i += 1;
            } else {
                lines.push(std::mem::take(&mut current));
                if lines.len() == max_lines - 1 {
                    break;
                }
            }
        }
        if i < words.len() {
            // Remaining words go into the last line, which gets ellipsized.
            let rest = words[i..].join(" ");
            current = if current.is_empty() {
                rest
            } else {
                format!("{current} {rest}")
            };
        }
        if !current.is_empty() {
            lines.push(current);
        }
        lines
            .into_iter()
            .map(|l| self.ellipsize(&l, size, weight, max_w))
            .collect()
    }
}

/// Spotify strings are usually NFC already, but decomposed Vietnamese would render the
/// combining marks badly, so normalise everything that comes from the network.
pub fn clean(s: &str) -> String {
    s.nfc().collect()
}
