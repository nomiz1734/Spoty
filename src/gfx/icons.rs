//! Vector icons described with signed distance functions on a 24x24 grid,
//! rasterized once per size into anti-aliased masks.

use std::collections::HashMap;

use super::canvas::{Canvas, Color, Mask};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Icon {
    Play,
    Pause,
    Next,
    Prev,
    Shuffle,
    Repeat,
    Heart,
    Search,
    Library,
    Note,
    Speaker,
    Person,
    Folder,
    Home,
    Disc,
    Refresh,
    Check,
    /// A cross, for "clear".
    Close,
    /// D-pad left/right, for button hints.
    PadLR,
    /// D-pad up/down, for button hints.
    PadUD,
}

enum Shape {
    Circle(f32, f32, f32),
    /// Ring: centre, radius, half stroke width.
    Ring(f32, f32, f32, f32),
    /// Segment with round caps: a, b, radius.
    Line(f32, f32, f32, f32, f32),
    /// Filled triangle, optionally inflated by a rounding radius.
    Tri([(f32, f32); 3], f32),
    /// Rounded box: x0, y0, x1, y1, radius.
    Box(f32, f32, f32, f32, f32),
    /// Outline of a rounded box: x0, y0, x1, y1, radius, half stroke.
    BoxOutline(f32, f32, f32, f32, f32, f32),
    /// Arc: centre, radius, half stroke, start and end angle (radians, clockwise from +x).
    Arc(f32, f32, f32, f32, f32, f32),
}

fn shapes(icon: Icon) -> Vec<Shape> {
    use Shape::*;
    match icon {
        Icon::Play => vec![Tri([(7.0, 4.5), (19.5, 12.0), (7.0, 19.5)], 1.0)],
        Icon::Pause => vec![Box(5.5, 4.0, 10.0, 20.0, 1.2), Box(14.0, 4.0, 18.5, 20.0, 1.2)],
        Icon::Next => vec![
            Tri([(4.5, 5.0), (15.0, 12.0), (4.5, 19.0)], 0.8),
            Box(16.0, 5.0, 19.0, 19.0, 1.0),
        ],
        Icon::Prev => vec![
            Tri([(19.5, 5.0), (9.0, 12.0), (19.5, 19.0)], 0.8),
            Box(5.0, 5.0, 8.0, 19.0, 1.0),
        ],
        Icon::Shuffle => vec![
            Line(3.0, 7.0, 7.0, 7.0, 1.0),
            Line(7.0, 7.0, 15.0, 17.0, 1.0),
            Line(15.0, 17.0, 18.0, 17.0, 1.0),
            Line(3.0, 17.0, 7.0, 17.0, 1.0),
            Line(7.0, 17.0, 15.0, 7.0, 1.0),
            Line(15.0, 7.0, 18.0, 7.0, 1.0),
            Tri([(17.0, 3.5), (22.0, 7.0), (17.0, 10.5)], 0.3),
            Tri([(17.0, 13.5), (22.0, 17.0), (17.0, 20.5)], 0.3),
        ],
        Icon::Repeat => vec![
            BoxOutline(3.5, 6.5, 20.5, 17.5, 4.0, 1.0),
            Tri([(13.0, 3.0), (18.0, 6.5), (13.0, 10.0)], 0.3),
            Tri([(11.0, 14.0), (6.0, 17.5), (11.0, 21.0)], 0.3),
        ],
        Icon::Heart => vec![
            Circle(8.3, 9.0, 4.6),
            Circle(15.7, 9.0, 4.6),
            Tri([(4.0, 11.2), (20.0, 11.2), (12.0, 20.5)], 0.6),
        ],
        Icon::Search => vec![
            Ring(10.0, 10.0, 6.0, 1.2),
            Line(14.5, 14.5, 20.0, 20.0, 1.5),
        ],
        Icon::Library => vec![
            Line(5.0, 4.0, 5.0, 20.0, 1.2),
            Line(10.0, 4.0, 10.0, 20.0, 1.2),
            Line(14.5, 4.5, 19.0, 19.5, 1.2),
        ],
        Icon::Note => vec![
            Circle(8.0, 17.0, 3.2),
            Line(10.6, 17.0, 10.6, 4.5, 1.0),
            Line(10.6, 4.5, 18.0, 3.0, 1.2),
            Line(18.0, 3.0, 18.0, 7.0, 1.0),
            Line(10.6, 8.0, 18.0, 6.5, 1.0),
        ],
        Icon::Speaker => vec![
            Box(3.0, 9.0, 8.0, 15.0, 1.0),
            Tri([(6.5, 9.0), (13.0, 3.5), (13.0, 20.5)], 0.5),
            Line(16.5, 9.0, 16.5, 15.0, 1.0),
            Line(20.0, 6.5, 20.0, 17.5, 1.0),
        ],
        Icon::Person => vec![Circle(12.0, 8.0, 4.2), Box(4.5, 14.0, 19.5, 22.0, 5.0)],
        Icon::Folder => vec![
            Box(2.5, 6.5, 21.5, 19.5, 2.0),
            Box(2.5, 4.0, 10.5, 9.0, 1.5),
        ],
        Icon::Home => vec![
            Tri([(2.5, 11.5), (12.0, 3.0), (21.5, 11.5)], 0.6),
            Box(5.0, 10.0, 19.0, 21.0, 1.5),
        ],
        Icon::Disc => vec![Ring(12.0, 12.0, 8.3, 1.3), Circle(12.0, 12.0, 2.6)],
        Icon::Refresh => vec![
            Arc(12.0, 12.5, 7.0, 1.2, -1.2, 4.3),
            Tri([(15.5, 2.5), (20.5, 6.0), (15.0, 9.5)], 0.3),
        ],
        Icon::Check => vec![
            Line(4.5, 12.5, 9.5, 17.5, 1.5),
            Line(9.5, 17.5, 19.5, 6.5, 1.5),
        ],
        Icon::Close => vec![
            Line(6.0, 6.0, 18.0, 18.0, 1.6),
            Line(18.0, 6.0, 6.0, 18.0, 1.6),
        ],
        Icon::PadLR => vec![
            Tri([(2.0, 12.0), (9.5, 6.0), (9.5, 18.0)], 0.5),
            Tri([(22.0, 12.0), (14.5, 6.0), (14.5, 18.0)], 0.5),
        ],
        Icon::PadUD => vec![
            Tri([(12.0, 2.0), (6.0, 9.5), (18.0, 9.5)], 0.5),
            Tri([(12.0, 22.0), (6.0, 14.5), (18.0, 14.5)], 0.5),
        ],
    }
}

fn sd_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (pax, pay) = (px - ax, py - ay);
    let (bax, bay) = (bx - ax, by - ay);
    let len2 = bax * bax + bay * bay;
    let h = if len2 > 0.0 {
        ((pax * bax + pay * bay) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let dx = pax - bax * h;
    let dy = pay - bay * h;
    (dx * dx + dy * dy).sqrt()
}

fn sd_triangle(px: f32, py: f32, t: &[(f32, f32); 3]) -> f32 {
    // Inigo Quilez's exact triangle SDF.
    let (p0, p1, p2) = (t[0], t[1], t[2]);
    let e0 = (p1.0 - p0.0, p1.1 - p0.1);
    let e1 = (p2.0 - p1.0, p2.1 - p1.1);
    let e2 = (p0.0 - p2.0, p0.1 - p2.1);
    let v0 = (px - p0.0, py - p0.1);
    let v1 = (px - p1.0, py - p1.1);
    let v2 = (px - p2.0, py - p2.1);
    let dot = |a: (f32, f32), b: (f32, f32)| a.0 * b.0 + a.1 * b.1;
    let pq = |v: (f32, f32), e: (f32, f32)| {
        let h = (dot(v, e) / dot(e, e)).clamp(0.0, 1.0);
        (v.0 - e.0 * h, v.1 - e.1 * h)
    };
    let pq0 = pq(v0, e0);
    let pq1 = pq(v1, e1);
    let pq2 = pq(v2, e2);
    let s = (e0.0 * e2.1 - e0.1 * e2.0).signum();
    let d0 = (dot(pq0, pq0), s * (v0.0 * e0.1 - v0.1 * e0.0));
    let d1 = (dot(pq1, pq1), s * (v1.0 * e1.1 - v1.1 * e1.0));
    let d2 = (dot(pq2, pq2), s * (v2.0 * e2.1 - v2.1 * e2.0));
    let dmin = d0.0.min(d1.0).min(d2.0);
    let smin = d0.1.min(d1.1).min(d2.1);
    -dmin.sqrt() * smin.signum()
}

fn sd_box(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> f32 {
    let cx = (x0 + x1) / 2.0;
    let cy = (y0 + y1) / 2.0;
    let hx = (x1 - x0) / 2.0 - r;
    let hy = (y1 - y0) / 2.0 - r;
    let qx = (px - cx).abs() - hx;
    let qy = (py - cy).abs() - hy;
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

fn sdf(shape: &Shape, x: f32, y: f32) -> f32 {
    match *shape {
        Shape::Circle(cx, cy, r) => ((x - cx).powi(2) + (y - cy).powi(2)).sqrt() - r,
        Shape::Ring(cx, cy, r, hw) => (((x - cx).powi(2) + (y - cy).powi(2)).sqrt() - r).abs() - hw,
        Shape::Line(ax, ay, bx, by, r) => sd_segment(x, y, ax, ay, bx, by) - r,
        Shape::Tri(ref t, r) => sd_triangle(x, y, t) - r,
        Shape::Box(x0, y0, x1, y1, r) => sd_box(x, y, x0, y0, x1, y1, r),
        Shape::BoxOutline(x0, y0, x1, y1, r, hw) => sd_box(x, y, x0, y0, x1, y1, r).abs() - hw,
        Shape::Arc(cx, cy, r, hw, a0, a1) => {
            let (dx, dy) = (x - cx, y - cy);
            let mut ang = dy.atan2(dx);
            while ang < a0 {
                ang += std::f32::consts::TAU;
            }
            if ang <= a1 {
                ((dx * dx + dy * dy).sqrt() - r).abs() - hw
            } else {
                // Round caps at both ends.
                let end = |a: f32| {
                    let (ex, ey) = (cx + r * a.cos(), cy + r * a.sin());
                    ((x - ex).powi(2) + (y - ey).powi(2)).sqrt()
                };
                end(a0).min(end(a1)) - hw
            }
        }
    }
}

pub fn rasterize(icon: Icon, size: u32) -> Mask {
    let list = shapes(icon);
    let n = size as usize;
    let scale = 24.0 / size as f32;
    let mut a = vec![0u8; n * n];
    for py in 0..n {
        for px in 0..n {
            let x = (px as f32 + 0.5) * scale;
            let y = (py as f32 + 0.5) * scale;
            let d = list
                .iter()
                .map(|s| sdf(s, x, y))
                .fold(f32::INFINITY, f32::min);
            // Distance is in grid units; convert to pixels for a 1px AA ramp.
            let cov = (0.5 - d / scale).clamp(0.0, 1.0);
            a[py * n + px] = (cov * 255.0) as u8;
        }
    }
    Mask { w: n, h: n, a }
}

#[derive(Default)]
pub struct IconCache {
    masks: HashMap<(Icon, u32), Mask>,
}

impl IconCache {
    pub fn draw(&mut self, canvas: &mut Canvas, icon: Icon, x: i32, y: i32, size: u32, c: Color) {
        let m = self
            .masks
            .entry((icon, size))
            .or_insert_with(|| rasterize(icon, size));
        canvas.draw_mask(x, y, m, c, 255);
    }
}
