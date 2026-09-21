use std::sync::mpsc::Sender;

use crate::config::Config;
use crate::gfx::Canvas;
use crate::ui::UiMsg;

#[cfg(feature = "desktop")]
mod desktop;
#[cfg(all(target_os = "linux", not(feature = "desktop")))]
mod evdev;
#[cfg(all(target_os = "linux", not(feature = "desktop")))]
mod fbdev;
mod power;

pub use power::{read_battery, Battery};

#[cfg_attr(not(any(target_os = "linux", feature = "desktop")), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Button {
    Up,
    Down,
    Left,
    Right,
    A,
    B,
    X,
    Y,
    L1,
    R1,
    L2,
    R2,
    Select,
    Start,
    Menu,
    Power,
    VolUp,
    VolDown,
    /// Left / right stick press.
    L3,
    R3,
    /// Right stick pushed in a direction (moves the caret in the search field).
    RsLeft,
    RsRight,
    RsUp,
    RsDown,
}

#[cfg_attr(not(all(target_os = "linux", not(feature = "desktop"))), allow(dead_code))]
impl Button {
    pub fn from_name(s: &str) -> Option<Button> {
        use Button::*;
        Some(match s.to_ascii_uppercase().as_str() {
            "UP" => Up,
            "DOWN" => Down,
            "LEFT" => Left,
            "RIGHT" => Right,
            "A" => A,
            "B" => B,
            "X" => X,
            "Y" => Y,
            "L1" | "L" => L1,
            "R1" | "R" => R1,
            "L2" => L2,
            "R2" => R2,
            "SELECT" => Select,
            "START" => Start,
            "MENU" => Menu,
            "POWER" => Power,
            "VOLUP" | "VOL+" => VolUp,
            "VOLDOWN" | "VOL-" => VolDown,
            "L3" => L3,
            "R3" => R3,
            "RS_LEFT" => RsLeft,
            "RS_RIGHT" => RsRight,
            "RS_UP" => RsUp,
            "RS_DOWN" => RsDown,
            _ => return None,
        })
    }
}

/// Where frames go.
pub trait Screen {
    fn size(&self) -> (usize, usize);
    fn present(&mut self, frame: &Canvas);
    fn set_backlight(&mut self, on: bool);
    /// Desktop windows read their keyboard on the UI thread. Returns false when closed.
    fn pump(&mut self, _tx: &Sender<UiMsg>) -> bool {
        true
    }
    /// True if `pump` must be called regularly even when idle.
    fn needs_polling(&self) -> bool {
        false
    }
}

pub fn open_screen(cfg: &Config) -> Result<Box<dyn Screen>, String> {
    let _ = cfg;
    #[cfg(feature = "desktop")]
    {
        return desktop::DesktopScreen::open().map(|s| Box::new(s) as Box<dyn Screen>);
    }
    #[cfg(all(target_os = "linux", not(feature = "desktop")))]
    {
        return fbdev::FbScreen::open(cfg).map(|s| Box::new(s) as Box<dyn Screen>);
    }
    #[allow(unreachable_code)]
    Err("this build has no display backend (build with --features desktop)".into())
}

/// Starts the hardware input reader (no-op on desktop, where the window delivers keys).
pub fn start_input(cfg: &Config, tx: Sender<UiMsg>) {
    let _ = (cfg, &tx);
    #[cfg(all(target_os = "linux", not(feature = "desktop")))]
    evdev::spawn(cfg, tx);
}
