//! Desktop window for developing the UI on a PC.
//! Keys: arrows = D-pad, Z = A, X = B, A = Y, S = X, Q/W = L1/R1, 1/2 = L2/R2,
//! Enter = Start, Right Shift = Select, Esc = Menu, P = Power, +/- = volume.

use std::sync::mpsc::Sender;

use minifb::{Key, KeyRepeat, Window, WindowOptions};

use crate::gfx::Canvas;
use crate::ui::UiMsg;

use super::{Button, Screen};

const W: usize = 1024;
const H: usize = 768;

pub struct DesktopScreen {
    window: Window,
    backlight: bool,
}

impl DesktopScreen {
    pub fn open() -> Result<Self, String> {
        let mut window = Window::new("Spoty", W, H, WindowOptions::default())
            .map_err(|e| e.to_string())?;
        window.set_target_fps(60);
        Ok(Self {
            window,
            backlight: true,
        })
    }
}

fn map_key(k: Key) -> Option<Button> {
    Some(match k {
        Key::Up => Button::Up,
        Key::Down => Button::Down,
        Key::Left => Button::Left,
        Key::Right => Button::Right,
        Key::Z => Button::A,
        Key::X | Key::Backspace => Button::B,
        Key::A => Button::Y,
        Key::S => Button::X,
        Key::Q => Button::L1,
        Key::W => Button::R1,
        Key::Key1 => Button::L2,
        Key::Key2 => Button::R2,
        Key::Enter => Button::Start,
        Key::RightShift => Button::Select,
        Key::Escape => Button::Menu,
        Key::P => Button::Power,
        Key::Equal | Key::NumPadPlus => Button::VolUp,
        Key::Minus | Key::NumPadMinus => Button::VolDown,
        _ => return None,
    })
}

impl Screen for DesktopScreen {
    fn size(&self) -> (usize, usize) {
        (W, H)
    }

    fn present(&mut self, frame: &Canvas) {
        if self.backlight {
            let _ = self.window.update_with_buffer(&frame.px, frame.w, frame.h);
        } else {
            let black = vec![0u32; W * H];
            let _ = self.window.update_with_buffer(&black, W, H);
        }
    }

    fn set_backlight(&mut self, on: bool) {
        self.backlight = on;
    }

    fn pump(&mut self, tx: &Sender<UiMsg>) -> bool {
        self.window.update();
        for k in self.window.get_keys_pressed(KeyRepeat::No) {
            if let Some(b) = map_key(k) {
                let _ = tx.send(UiMsg::Input(b, true));
            }
        }
        for k in self.window.get_keys_released() {
            if let Some(b) = map_key(k) {
                let _ = tx.send(UiMsg::Input(b, false));
            }
        }
        self.window.is_open()
    }

    fn needs_polling(&self) -> bool {
        true
    }
}
