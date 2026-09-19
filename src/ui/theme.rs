use crate::gfx::{rgb, Color};

pub const BG: Color = rgb(0x12, 0x12, 0x12);
pub const SURFACE: Color = rgb(0x18, 0x18, 0x18);
pub const ELEVATED: Color = rgb(0x28, 0x28, 0x28);
pub const HIGHLIGHT: Color = rgb(0x33, 0x33, 0x33);
pub const ACCENT: Color = rgb(0x1E, 0xD7, 0x60);
pub const TEXT: Color = rgb(0xFF, 0xFF, 0xFF);
pub const SUBTEXT: Color = rgb(0xB3, 0xB3, 0xB3);
pub const DIM: Color = rgb(0x72, 0x72, 0x72);
pub const ERROR: Color = rgb(0xF1, 0x5E, 0x6C);
pub const WARN: Color = rgb(0xFF, 0xA4, 0x2B);
pub const PLACEHOLDER: Color = rgb(0x2E, 0x2E, 0x2E);
pub const LIKED_TOP: Color = rgb(0x45, 0x0A, 0xF5);
pub const LIKED_BOTTOM: Color = rgb(0x8E, 0xB8, 0xE5);

pub const TOP_BAR_H: i32 = 60;
pub const HINTS_H: i32 = 44;
pub const MINI_H: i32 = 84;

pub const HOME_ROW_H: i32 = 80;
pub const HOME_THUMB: u32 = 60;
pub const TRACK_ROW_H: i32 = 68;
pub const TRACK_THUMB: u32 = 48;
pub const NOW_COVER: u32 = 440;
pub const MINI_THUMB: u32 = 60;

pub const SIDE_PAD: i32 = 28;
