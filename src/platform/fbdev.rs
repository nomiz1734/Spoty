//! Direct framebuffer output (/dev/fb0) plus backlight control through the
//! Allwinner /dev/disp driver.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;

use crate::config::Config;
use crate::gfx::Canvas;

use super::Screen;

const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;
const FBIOGET_FSCREENINFO: libc::c_ulong = 0x4602;
const FBIOPAN_DISPLAY: libc::c_ulong = 0x4606;
const FBIOBLANK: libc::c_ulong = 0x4611;
const FBIO_WAITFORVSYNC: libc::c_ulong = 0x4004_4620;
const FB_BLANK_UNBLANK: libc::c_ulong = 0;
const FB_BLANK_POWERDOWN: libc::c_ulong = 4;

const DISP_LCD_SET_BRIGHTNESS: libc::c_ulong = 0x102;
const DISP_LCD_GET_BRIGHTNESS: libc::c_ulong = 0x103;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct FbBitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct FbVarScreeninfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: FbBitfield,
    green: FbBitfield,
    blue: FbBitfield,
    transp: FbBitfield,
    nonstd: u32,
    activate: u32,
    height: u32,
    width: u32,
    accel_flags: u32,
    pixclock: u32,
    left_margin: u32,
    right_margin: u32,
    upper_margin: u32,
    lower_margin: u32,
    hsync_len: u32,
    vsync_len: u32,
    sync: u32,
    vmode: u32,
    rotate: u32,
    colorspace: u32,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FbFixScreeninfo {
    id: [u8; 16],
    smem_start: libc::c_ulong,
    smem_len: u32,
    type_: u32,
    type_aux: u32,
    visual: u32,
    xpanstep: u16,
    ypanstep: u16,
    ywrapstep: u16,
    line_length: u32,
    mmio_start: libc::c_ulong,
    mmio_len: u32,
    accel: u32,
    capabilities: u16,
    reserved: [u16; 2],
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PixFmt {
    /// 32-bit with red at bit 16 (XRGB / ARGB little endian) — native canvas layout.
    Xrgb { alpha: bool },
    /// 32-bit with red at bit 0.
    Xbgr { alpha: bool },
    Rgb565,
}

pub struct FbScreen {
    file: File,
    map: *mut u8,
    map_len: usize,
    var: FbVarScreeninfo,
    stride: usize,
    fmt: PixFmt,
    rotate: u32,
    logical: (usize, usize),
    double: bool,
    page: u32,
    vsync: bool,
    saved_brightness: Option<u32>,
    scratch: Vec<u32>,
}

// The mapping is only touched from the UI thread.
unsafe impl Send for FbScreen {}

impl FbScreen {
    pub fn open(cfg: &Config) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/fb0")
            .map_err(|e| format!("open /dev/fb0: {e}"))?;
        let fd = file.as_raw_fd();
        let mut var = FbVarScreeninfo::default();
        let mut fix: FbFixScreeninfo = unsafe { std::mem::zeroed() };
        unsafe {
            if libc::ioctl(fd, FBIOGET_VSCREENINFO as _, &mut var) < 0 {
                return Err("FBIOGET_VSCREENINFO failed".into());
            }
            if libc::ioctl(fd, FBIOGET_FSCREENINFO as _, &mut fix) < 0 {
                return Err("FBIOGET_FSCREENINFO failed".into());
            }
        }
        log::info!(
            "fb0: {}x{} virt {}x{} off {},{} bpp {} r{}/{} g{}/{} b{}/{} a{}/{} stride {} smem {}",
            var.xres,
            var.yres,
            var.xres_virtual,
            var.yres_virtual,
            var.xoffset,
            var.yoffset,
            var.bits_per_pixel,
            var.red.offset,
            var.red.length,
            var.green.offset,
            var.green.length,
            var.blue.offset,
            var.blue.length,
            var.transp.offset,
            var.transp.length,
            fix.line_length,
            fix.smem_len
        );
        let fmt = match var.bits_per_pixel {
            32 if var.red.offset == 0 => PixFmt::Xbgr {
                alpha: var.transp.length > 0,
            },
            32 => PixFmt::Xrgb {
                alpha: var.transp.length > 0,
            },
            16 => PixFmt::Rgb565,
            other => return Err(format!("unsupported framebuffer depth {other}")),
        };
        let stride = if fix.line_length > 0 {
            fix.line_length as usize
        } else {
            var.xres_virtual as usize * var.bits_per_pixel as usize / 8
        };
        let needed = stride * var.yres_virtual.max(var.yres) as usize;
        let map_len = (fix.smem_len as usize).max(needed);
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return Err("mmap /dev/fb0 failed".into());
        }
        let rotate = match cfg.rotate {
            90 | 180 | 270 => cfg.rotate,
            _ => 0,
        };
        let logical = if rotate == 90 || rotate == 270 {
            (var.yres as usize, var.xres as usize)
        } else {
            (var.xres as usize, var.yres as usize)
        };
        let double = cfg.fb_double_buffer && var.yres_virtual >= var.yres * 2;
        Ok(Self {
            file,
            map: map as *mut u8,
            map_len,
            var,
            stride,
            fmt,
            rotate,
            logical,
            double,
            page: 0,
            vsync: true,
            saved_brightness: None,
            scratch: Vec::new(),
        })
    }

    fn wait_vsync(&mut self) {
        if !self.vsync {
            return;
        }
        let arg: u32 = 0;
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), FBIO_WAITFORVSYNC as _, &arg) };
        if r < 0 {
            log::info!("fb0: FBIO_WAITFORVSYNC unsupported, drawing without vsync");
            self.vsync = false;
        }
    }

    /// Writes `src` (xres*yres pixels in physical orientation) to the given line offset.
    fn write_page(&mut self, src: &[u32], y_offset: usize) {
        let w = self.var.xres as usize;
        let h = self.var.yres as usize;
        let x_off = self.var.xoffset as usize;
        let bpp = if self.fmt == PixFmt::Rgb565 { 2 } else { 4 };
        if (y_offset + h) * self.stride > self.map_len {
            return;
        }
        for y in 0..h {
            let row = &src[y * w..(y + 1) * w];
            let dst = unsafe { self.map.add((y_offset + y) * self.stride + x_off * bpp) };
            match self.fmt {
                PixFmt::Xrgb { alpha: false } => unsafe {
                    std::ptr::copy_nonoverlapping(row.as_ptr(), dst as *mut u32, w);
                },
                PixFmt::Xrgb { alpha: true } => {
                    let d = unsafe { std::slice::from_raw_parts_mut(dst as *mut u32, w) };
                    for (o, &p) in d.iter_mut().zip(row) {
                        *o = p | 0xFF00_0000;
                    }
                }
                PixFmt::Xbgr { alpha } => {
                    let a = if alpha { 0xFF00_0000 } else { 0 };
                    let d = unsafe { std::slice::from_raw_parts_mut(dst as *mut u32, w) };
                    for (o, &p) in d.iter_mut().zip(row) {
                        *o = a | ((p & 0xFF) << 16) | (p & 0xFF00) | ((p >> 16) & 0xFF);
                    }
                }
                PixFmt::Rgb565 => {
                    let d = unsafe { std::slice::from_raw_parts_mut(dst as *mut u16, w) };
                    for (o, &p) in d.iter_mut().zip(row) {
                        *o = (((p >> 8) & 0xF800) | ((p >> 5) & 0x07E0) | ((p >> 3) & 0x001F))
                            as u16;
                    }
                }
            }
        }
    }

    fn rotated<'a>(&'a mut self, frame: &'a Canvas) -> &'a [u32] {
        if self.rotate == 0 {
            return &frame.px;
        }
        let (pw, ph) = (self.var.xres as usize, self.var.yres as usize);
        self.scratch.resize(pw * ph, 0);
        let (lw, lh) = (frame.w, frame.h);
        match self.rotate {
            180 => {
                for (i, &p) in frame.px.iter().enumerate() {
                    self.scratch[pw * ph - 1 - i] = p;
                }
            }
            90 => {
                // Logical (x, y) -> physical (ph-1-y... ) : rotate clockwise.
                for y in 0..lh {
                    for x in 0..lw {
                        let px = pw - 1 - y;
                        let py = x;
                        self.scratch[py * pw + px] = frame.px[y * lw + x];
                    }
                }
            }
            _ => {
                for y in 0..lh {
                    for x in 0..lw {
                        let px = y;
                        let py = ph - 1 - x;
                        self.scratch[py * pw + px] = frame.px[y * lw + x];
                    }
                }
            }
        }
        &self.scratch
    }

    fn disp_ioctl(cmd: libc::c_ulong, value: u32) -> Option<i32> {
        let disp = OpenOptions::new().read(true).write(true).open("/dev/disp").ok()?;
        let mut args: [libc::c_ulong; 4] = [0, value as libc::c_ulong, 0, 0];
        let r = unsafe { libc::ioctl(disp.as_raw_fd(), cmd as _, args.as_mut_ptr()) };
        (r >= 0).then_some(r)
    }
}

impl Screen for FbScreen {
    fn size(&self) -> (usize, usize) {
        self.logical
    }

    fn present(&mut self, frame: &Canvas) {
        let h = self.var.yres as usize;
        let src: *const [u32] = self.rotated(frame);
        // SAFETY: `src` points either into `frame` or `self.scratch`, neither of which is
        // modified by write_page.
        let src = unsafe { &*src };
        if self.double {
            let next = 1 - self.page;
            self.write_page(src, next as usize * h);
            let mut var = self.var;
            var.yoffset = next * self.var.yres;
            let r = unsafe { libc::ioctl(self.file.as_raw_fd(), FBIOPAN_DISPLAY as _, &var) };
            if r < 0 {
                log::warn!("fb0: pan failed, falling back to single buffer");
                self.double = false;
                self.write_page(src, self.var.yoffset as usize);
            } else {
                self.page = next;
            }
        } else {
            self.wait_vsync();
            self.write_page(src, self.var.yoffset as usize);
        }
    }

    fn set_backlight(&mut self, on: bool) {
        let fd = self.file.as_raw_fd();
        if on {
            let level = self.saved_brightness.take().unwrap_or(80);
            if Self::disp_ioctl(DISP_LCD_SET_BRIGHTNESS, level).is_none() {
                unsafe { libc::ioctl(fd, FBIOBLANK as _, FB_BLANK_UNBLANK) };
            }
        } else {
            let current = Self::disp_ioctl(DISP_LCD_GET_BRIGHTNESS, 0)
                .filter(|&v| v > 0)
                .map(|v| v as u32);
            self.saved_brightness = current.or(self.saved_brightness);
            if current.is_none() || Self::disp_ioctl(DISP_LCD_SET_BRIGHTNESS, 0).is_none() {
                unsafe { libc::ioctl(fd, FBIOBLANK as _, FB_BLANK_POWERDOWN) };
            }
        }
    }
}

impl Drop for FbScreen {
    fn drop(&mut self) {
        if self.saved_brightness.is_some() {
            self.set_backlight(true);
        }
        // Leave a black screen for the launcher.
        let lines = self.var.yres_virtual.max(self.var.yres) as usize;
        let len = (lines * self.stride).min(self.map_len);
        unsafe { std::ptr::write_bytes(self.map, 0, len) };
        if self.double && self.page != 0 {
            let mut var = self.var;
            var.yoffset = 0;
            unsafe { libc::ioctl(self.file.as_raw_fd(), FBIOPAN_DISPLAY as _, &var) };
        }
        unsafe { libc::munmap(self.map as *mut libc::c_void, self.map_len) };
    }
}
