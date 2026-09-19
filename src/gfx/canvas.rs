//! Software canvas in 0x00RRGGBB. Everything the UI draws goes through here.

pub type Color = u32;

pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

pub fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    let t = (t.clamp(0.0, 1.0) * 256.0) as u32;
    blend_px(a, b, t)
}

/// Mixes `src` over `dst` with alpha in 0..=256.
#[inline(always)]
fn blend_px(dst: u32, src: u32, a: u32) -> u32 {
    let ia = 256 - a;
    let rb = (((src & 0xFF00FF) * a + (dst & 0xFF00FF) * ia) >> 8) & 0xFF00FF;
    let g = (((src & 0x00FF00) * a + (dst & 0x00FF00) * ia) >> 8) & 0x00FF00;
    rb | g
}

#[inline(always)]
fn alpha256(a: u8) -> u32 {
    a as u32 + (a as u32 >> 7)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }
    fn intersect(&self, o: &Rect) -> Rect {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = (self.x + self.w).min(o.x + o.w);
        let y1 = (self.y + self.h).min(o.y + o.h);
        Rect::new(x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
    }
}

/// An RGB image ready to blit.
#[derive(Clone)]
pub struct Image {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Image({}x{})", self.w, self.h)
    }
}

/// An image with alpha (0xAARRGGBB), for logos.
pub struct RgbaImage {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
}

/// 8-bit coverage mask (glyphs, icons).
pub struct Mask {
    pub w: usize,
    pub h: usize,
    pub a: Vec<u8>,
}

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
    clip: Rect,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            px: vec![0; w * h],
            clip: Rect::new(0, 0, w as i32, h as i32),
        }
    }

    pub fn full(&self) -> Rect {
        Rect::new(0, 0, self.w as i32, self.h as i32)
    }

    pub fn set_clip(&mut self, r: Rect) {
        self.clip = r.intersect(&self.full());
    }

    pub fn reset_clip(&mut self) {
        self.clip = self.full();
    }

    pub fn clear(&mut self, c: Color) {
        self.px.fill(c);
    }

    fn clipped(&self, x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect::new(x, y, w, h).intersect(&self.clip)
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: Color) {
        let r = self.clipped(x, y, w, h);
        if r.w <= 0 || r.h <= 0 {
            return;
        }
        for yy in r.y..r.y + r.h {
            let row = yy as usize * self.w;
            self.px[row + r.x as usize..row + (r.x + r.w) as usize].fill(c);
        }
    }

    pub fn fill_rect_alpha(&mut self, x: i32, y: i32, w: i32, h: i32, c: Color, alpha: u8) {
        if alpha == 255 {
            return self.fill_rect(x, y, w, h, c);
        }
        let r = self.clipped(x, y, w, h);
        let a = alpha256(alpha);
        for yy in r.y..r.y + r.h {
            let row = yy as usize * self.w;
            for p in &mut self.px[row + r.x as usize..row + (r.x + r.w) as usize] {
                *p = blend_px(*p, c, a);
            }
        }
    }

    /// Vertical gradient from `top` to `bottom`.
    pub fn fill_vgradient(&mut self, x: i32, y: i32, w: i32, h: i32, top: Color, bottom: Color) {
        if h <= 0 {
            return;
        }
        let r = self.clipped(x, y, w, h);
        for yy in r.y..r.y + r.h {
            let t = (yy - y) as f32 / h as f32;
            let c = lerp_color(top, bottom, t);
            let row = yy as usize * self.w;
            self.px[row + r.x as usize..row + (r.x + r.w) as usize].fill(c);
        }
    }

    /// Anti-aliased rounded rectangle.
    pub fn fill_rounded(&mut self, x: i32, y: i32, w: i32, h: i32, radius: i32, c: Color) {
        self.fill_rounded_alpha(x, y, w, h, radius, c, 255);
    }

    pub fn fill_rounded_alpha(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        radius: i32,
        c: Color,
        alpha: u8,
    ) {
        let rad = radius.min(w / 2).min(h / 2).max(0);
        if rad == 0 {
            return self.fill_rect_alpha(x, y, w, h, c, alpha);
        }
        // Middle band and side bands are plain rectangles.
        self.fill_rect_alpha(x, y + rad, w, h - 2 * rad, c, alpha);
        self.fill_rect_alpha(x + rad, y, w - 2 * rad, rad, c, alpha);
        self.fill_rect_alpha(x + rad, y + h - rad, w - 2 * rad, rad, c, alpha);
        let r = rad as f32;
        let corners = [
            (x, y, x + rad, y + rad),
            (x + w - rad, y, x + w - rad, y + rad),
            (x, y + h - rad, x + rad, y + h - rad),
            (x + w - rad, y + h - rad, x + w - rad, y + h - rad),
        ];
        for (cx0, cy0, ccx, ccy) in corners {
            for yy in cy0..cy0 + rad {
                for xx in cx0..cx0 + rad {
                    let dx = xx as f32 + 0.5 - ccx as f32;
                    let dy = yy as f32 + 0.5 - ccy as f32;
                    let d = (dx * dx + dy * dy).sqrt();
                    let cov = (r - d + 0.5).clamp(0.0, 1.0);
                    if cov > 0.0 {
                        self.blend_point(xx, yy, c, (cov * alpha as f32) as u8);
                    }
                }
            }
        }
    }

    #[inline]
    pub fn blend_point(&mut self, x: i32, y: i32, c: Color, alpha: u8) {
        let cl = &self.clip;
        if x < cl.x || y < cl.y || x >= cl.x + cl.w || y >= cl.y + cl.h {
            return;
        }
        let i = y as usize * self.w + x as usize;
        self.px[i] = blend_px(self.px[i], c, alpha256(alpha));
    }

    /// Anti-aliased filled circle.
    pub fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, c: Color) {
        let x0 = (cx - r - 1.0).floor() as i32;
        let x1 = (cx + r + 1.0).ceil() as i32;
        let y0 = (cy - r - 1.0).floor() as i32;
        let y1 = (cy + r + 1.0).ceil() as i32;
        for y in y0..y1 {
            for x in x0..x1 {
                let dx = x as f32 + 0.5 - cx;
                let dy = y as f32 + 0.5 - cy;
                let cov = (r - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend_point(x, y, c, (cov * 255.0) as u8);
                }
            }
        }
    }

    /// Draws a coverage mask tinted with `c`.
    pub fn draw_mask(&mut self, x: i32, y: i32, m: &Mask, c: Color, opacity: u8) {
        let r = self.clipped(x, y, m.w as i32, m.h as i32);
        if r.w <= 0 || r.h <= 0 {
            return;
        }
        let op = alpha256(opacity);
        for yy in r.y..r.y + r.h {
            let my = (yy - y) as usize;
            let mrow = &m.a[my * m.w..(my + 1) * m.w];
            let row = yy as usize * self.w;
            for xx in r.x..r.x + r.w {
                let a = mrow[(xx - x) as usize];
                if a == 0 {
                    continue;
                }
                let a = (alpha256(a) * op) >> 8;
                let p = &mut self.px[row + xx as usize];
                *p = if a >= 256 { c } else { blend_px(*p, c, a) };
            }
        }
    }

    /// Alpha-blended blit.
    pub fn blit_rgba(&mut self, x: i32, y: i32, img: &RgbaImage) {
        let r = self.clipped(x, y, img.w as i32, img.h as i32);
        for yy in r.y..r.y + r.h {
            let sy = (yy - y) as usize;
            let row = yy as usize * self.w;
            for xx in r.x..r.x + r.w {
                let p = img.px[sy * img.w + (xx - x) as usize];
                let a = p >> 24;
                if a == 0 {
                    continue;
                }
                let d = &mut self.px[row + xx as usize];
                *d = if a == 255 {
                    p & 0xFF_FFFF
                } else {
                    blend_px(*d, p & 0xFF_FFFF, a + (a >> 7))
                };
            }
        }
    }

    /// Opaque blit.
    pub fn blit(&mut self, x: i32, y: i32, img: &Image) {
        let r = self.clipped(x, y, img.w as i32, img.h as i32);
        if r.w <= 0 || r.h <= 0 {
            return;
        }
        for yy in r.y..r.y + r.h {
            let sy = (yy - y) as usize;
            let sx = (r.x - x) as usize;
            let src = &img.px[sy * img.w + sx..sy * img.w + sx + r.w as usize];
            let row = yy as usize * self.w;
            self.px[row + r.x as usize..row + (r.x + r.w) as usize].copy_from_slice(src);
        }
    }

    /// Blit with anti-aliased rounded corners (the corner pixels blend with what is below).
    pub fn blit_rounded(&mut self, x: i32, y: i32, img: &Image, radius: i32) {
        self.blit(x, y, img);
        let rad = radius.min(img.w as i32 / 2).min(img.h as i32 / 2);
        if rad <= 0 {
            return;
        }
        // Re-draw the corners: restore background where the corner is cut.
        // We cannot know the background, so we blend with the pixel just outside the image
        // row (left/right neighbour), which is what was there before the blit.
        let w = img.w as i32;
        let h = img.h as i32;
        let r = rad as f32;
        for (cx0, cy0, ccx, ccy, bgx) in [
            (0, 0, rad, rad, -1),
            (w - rad, 0, w - rad, rad, w),
            (0, h - rad, rad, h - rad, -1),
            (w - rad, h - rad, w - rad, h - rad, w),
        ] {
            for yy in cy0..cy0 + rad {
                let gy = y + yy;
                let bg = self.get(x + bgx, gy);
                for xx in cx0..cx0 + rad {
                    let dx = xx as f32 + 0.5 - ccx as f32;
                    let dy = yy as f32 + 0.5 - ccy as f32;
                    let cov = (r - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
                    if cov < 1.0 {
                        if let Some(bg) = bg {
                            let src = img.px[yy as usize * img.w + xx as usize];
                            let v = blend_px(bg, src, (cov * 256.0) as u32);
                            self.set(x + xx, gy, v);
                        }
                    }
                }
            }
        }
    }

    pub fn get(&self, x: i32, y: i32) -> Option<u32> {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return None;
        }
        Some(self.px[y as usize * self.w + x as usize])
    }

    fn set(&mut self, x: i32, y: i32, c: u32) {
        let cl = &self.clip;
        if x < cl.x || y < cl.y || x >= cl.x + cl.w || y >= cl.y + cl.h {
            return;
        }
        self.px[y as usize * self.w + x as usize] = c;
    }

    /// Darkens a region (used behind overlays).
    pub fn dim(&mut self, r: Rect, alpha: u8) {
        self.fill_rect_alpha(r.x, r.y, r.w, r.h, 0, alpha);
    }
}
