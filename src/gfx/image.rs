//! JPEG decoding and resampling for cover art.

use super::canvas::{rgb, Color, Image, RgbaImage};

/// Decodes a JPEG or PNG cover and scales it to `size` x `size`.
pub fn decode_image(bytes: &[u8], size: usize) -> Result<Image, String> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        let rgba = decode_png(bytes)?;
        // Flatten any transparency onto a dark grey.
        let bg = 0x282828u32;
        let px = rgba
            .px
            .iter()
            .map(|&p| {
                let a = p >> 24;
                if a == 255 {
                    p & 0xFF_FFFF
                } else {
                    super::canvas::lerp_color(bg, p & 0xFF_FFFF, a as f32 / 255.0)
                }
            })
            .collect();
        let img = Image {
            w: rgba.w,
            h: rgba.h,
            px,
        };
        let side = img.w.min(img.h);
        let c = crop(&img, (img.w - side) / 2, (img.h - side) / 2, side, side);
        Ok(resize(&c, size, size))
    } else {
        decode_cover(bytes, size)
    }
}

/// Decodes a PNG into 0xAARRGGBB pixels.
pub fn decode_png(bytes: &[u8]) -> Result<RgbaImage, String> {
    let mut dec = ::png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(::png::Transformations::EXPAND | ::png::Transformations::STRIP_16);
    let mut reader = dec.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("png too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let data = &buf[..info.buffer_size()];
    let px: Vec<u32> = match info.color_type {
        ::png::ColorType::Rgba => data
            .chunks_exact(4)
            .map(|c| (c[3] as u32) << 24 | rgb(c[0], c[1], c[2]))
            .collect(),
        ::png::ColorType::Rgb => data
            .chunks_exact(3)
            .map(|c| 0xFF00_0000 | rgb(c[0], c[1], c[2]))
            .collect(),
        ::png::ColorType::GrayscaleAlpha => data
            .chunks_exact(2)
            .map(|c| (c[1] as u32) << 24 | rgb(c[0], c[0], c[0]))
            .collect(),
        ::png::ColorType::Grayscale => data
            .iter()
            .map(|&l| 0xFF00_0000 | rgb(l, l, l))
            .collect(),
        ::png::ColorType::Indexed => return Err("indexed png not expanded".into()),
    };
    Ok(RgbaImage {
        w: info.width as usize,
        h: info.height as usize,
        px,
    })
}

/// Decodes a JPEG and scales it to exactly `size` x `size` (covers are square;
/// non-square images are centre-cropped).
pub fn decode_cover(bytes: &[u8], size: usize) -> Result<Image, String> {
    let mut dec = jpeg_decoder::Decoder::new(bytes);
    dec.read_info().map_err(|e| e.to_string())?;
    // Let the decoder do the cheap DCT-domain downscale first (1/2, 1/4, 1/8).
    let _ = dec.scale(size as u16, size as u16);
    let data = dec.decode().map_err(|e| e.to_string())?;
    let info = dec.info().ok_or("no jpeg info")?;
    let (w, h) = (info.width as usize, info.height as usize);
    let px: Vec<u32> = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => data
            .chunks_exact(3)
            .map(|c| rgb(c[0], c[1], c[2]))
            .collect(),
        jpeg_decoder::PixelFormat::L8 => data.iter().map(|&l| rgb(l, l, l)).collect(),
        jpeg_decoder::PixelFormat::CMYK32 => data
            .chunks_exact(4)
            .map(|c| {
                let k = 255 - c[3] as u32;
                let f = |v: u8| ((255 - v as u32) * k / 255) as u8;
                rgb(f(c[0]), f(c[1]), f(c[2]))
            })
            .collect(),
        jpeg_decoder::PixelFormat::L16 => data
            .chunks_exact(2)
            .map(|c| rgb(c[0], c[0], c[0]))
            .collect(),
    };
    if px.len() < w * h {
        return Err("truncated jpeg".into());
    }
    let src = Image { w, h, px };
    let side = w.min(h);
    let cropped = crop(&src, (w - side) / 2, (h - side) / 2, side, side);
    Ok(resize(&cropped, size, size))
}

fn crop(img: &Image, x: usize, y: usize, w: usize, h: usize) -> Image {
    if x == 0 && y == 0 && w == img.w && h == img.h {
        return img.clone();
    }
    let mut px = Vec::with_capacity(w * h);
    for row in y..y + h {
        px.extend_from_slice(&img.px[row * img.w + x..row * img.w + x + w]);
    }
    Image { w, h, px }
}

/// Area-average when shrinking, bilinear when enlarging.
pub fn resize(img: &Image, tw: usize, th: usize) -> Image {
    if img.w == tw && img.h == th {
        return img.clone();
    }
    if tw <= img.w && th <= img.h {
        shrink(img, tw, th)
    } else {
        bilinear(img, tw, th)
    }
}

fn shrink(img: &Image, tw: usize, th: usize) -> Image {
    // Separable box filter with fractional edge weights.
    let horiz = shrink_axis(&img.px, img.w, img.h, tw, true);
    let px = shrink_axis(&horiz, tw, img.h, th, false);
    Image { w: tw, h: th, px }
}

fn shrink_axis(src: &[u32], w: usize, h: usize, target: usize, horizontal: bool) -> Vec<u32> {
    let (len, lines) = if horizontal { (w, h) } else { (h, w) };
    let (out_w, out_h) = if horizontal { (target, h) } else { (w, target) };
    let mut out = vec![0u32; out_w * out_h];
    let ratio = len as f32 / target as f32;
    for line in 0..lines {
        for t in 0..target {
            let start = t as f32 * ratio;
            let end = start + ratio;
            let mut acc = [0f32; 3];
            let mut wsum = 0f32;
            let mut i = start.floor() as usize;
            while (i as f32) < end && i < len {
                let wgt = (end.min(i as f32 + 1.0) - start.max(i as f32)).max(0.0);
                let p = if horizontal {
                    src[line * w + i]
                } else {
                    src[i * w + line]
                };
                acc[0] += ((p >> 16) & 0xFF) as f32 * wgt;
                acc[1] += ((p >> 8) & 0xFF) as f32 * wgt;
                acc[2] += (p & 0xFF) as f32 * wgt;
                wsum += wgt;
                i += 1;
            }
            let inv = if wsum > 0.0 { 1.0 / wsum } else { 0.0 };
            let c = rgb(
                (acc[0] * inv + 0.5) as u8,
                (acc[1] * inv + 0.5) as u8,
                (acc[2] * inv + 0.5) as u8,
            );
            let idx = if horizontal {
                line * out_w + t
            } else {
                t * out_w + line
            };
            out[idx] = c;
        }
    }
    out
}

fn bilinear(img: &Image, tw: usize, th: usize) -> Image {
    let mut px = vec![0u32; tw * th];
    let sx = img.w as f32 / tw as f32;
    let sy = img.h as f32 / th as f32;
    for y in 0..th {
        let fy = ((y as f32 + 0.5) * sy - 0.5).max(0.0);
        let y0 = (fy as usize).min(img.h - 1);
        let y1 = (y0 + 1).min(img.h - 1);
        let ty = fy - y0 as f32;
        for x in 0..tw {
            let fx = ((x as f32 + 0.5) * sx - 0.5).max(0.0);
            let x0 = (fx as usize).min(img.w - 1);
            let x1 = (x0 + 1).min(img.w - 1);
            let tx = fx - x0 as f32;
            let p = |xx: usize, yy: usize| img.px[yy * img.w + xx];
            let mix = |shift: u32| {
                let c = |v: u32| ((v >> shift) & 0xFF) as f32;
                let top = c(p(x0, y0)) * (1.0 - tx) + c(p(x1, y0)) * tx;
                let bot = c(p(x0, y1)) * (1.0 - tx) + c(p(x1, y1)) * tx;
                (top * (1.0 - ty) + bot * ty + 0.5) as u8
            };
            px[y * tw + x] = rgb(mix(16), mix(8), mix(0));
        }
    }
    Image { w: tw, h: th, px }
}

/// A muted accent colour taken from the image, for backgrounds (like Spotify's
/// now-playing gradient). Favours saturated pixels so grey borders don't win.
pub fn accent_color(img: &Image) -> Color {
    let mut acc = [0f32; 3];
    let mut wsum = 0f32;
    let step = (img.px.len() / 2048).max(1);
    for &p in img.px.iter().step_by(step) {
        let r = ((p >> 16) & 0xFF) as f32;
        let g = ((p >> 8) & 0xFF) as f32;
        let b = (p & 0xFF) as f32;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let sat = if max > 0.0 { (max - min) / max } else { 0.0 };
        let wgt = 0.05 + sat * sat * (max / 255.0);
        acc[0] += r * wgt;
        acc[1] += g * wgt;
        acc[2] += b * wgt;
        wsum += wgt;
    }
    if wsum <= 0.0 {
        return rgb(60, 60, 60);
    }
    let (mut r, mut g, mut b) = (acc[0] / wsum, acc[1] / wsum, acc[2] / wsum);
    // Normalise brightness into a range that keeps white text readable.
    let lum = 0.299 * r + 0.587 * g + 0.114 * b;
    let target = lum.clamp(70.0, 110.0);
    if lum > 1.0 {
        let k = target / lum;
        r *= k;
        g *= k;
        b *= k;
    }
    rgb(
        r.min(255.0) as u8,
        g.min(255.0) as u8,
        b.min(255.0) as u8,
    )
}
