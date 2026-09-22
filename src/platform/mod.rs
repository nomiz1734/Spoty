use std::sync::atomic::{AtomicBool, Ordering};
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

/// The screen, which the app can hand back to the system menu while the music
/// keeps playing (`release`) and take again later (`acquire`). While released
/// nothing is drawn.
pub struct Display {
    open: Opener,
    inner: Option<Box<dyn Screen>>,
    size: (usize, usize),
}

type Opener = Box<dyn Fn() -> Result<Box<dyn Screen>, String> + Send>;

impl Display {
    pub fn open(cfg: &Config) -> Result<Self, String> {
        let cfg = cfg.clone();
        Self::with(Box::new(move || open_screen(&cfg)))
    }

    /// A display over whatever `open` makes (a fake screen in tests).
    pub fn with(open: Opener) -> Result<Self, String> {
        let inner = open()?;
        Ok(Self {
            open,
            size: inner.size(),
            inner: Some(inner),
        })
    }

    /// Lets go of the screen: the backlight comes back on, the framebuffer is
    /// left black and on its first page, ready for the system menu.
    pub fn release(&mut self) {
        self.inner = None;
    }

    pub fn acquire(&mut self) -> Result<(), String> {
        if self.inner.is_none() {
            self.inner = Some((self.open)()?);
        }
        Ok(())
    }
}

impl Screen for Display {
    fn size(&self) -> (usize, usize) {
        self.size
    }

    fn present(&mut self, frame: &Canvas) {
        if let Some(s) = self.inner.as_mut() {
            s.present(frame);
        }
    }

    fn set_backlight(&mut self, on: bool) {
        if let Some(s) = self.inner.as_mut() {
            s.set_backlight(on);
        }
    }

    fn pump(&mut self, tx: &Sender<UiMsg>) -> bool {
        self.inner.as_mut().map_or(true, |s| s.pump(tx))
    }

    fn needs_polling(&self) -> bool {
        self.inner.as_ref().is_some_and(|s| s.needs_polling())
    }
}

fn open_screen(cfg: &Config) -> Result<Box<dyn Screen>, String> {
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

static INPUT_ON: AtomicBool = AtomicBool::new(true);

/// Stops (false) or resumes reading the buttons. While Spoty runs in the
/// background they belong to the system menu and the games, power button
/// included.
pub fn set_input(on: bool) {
    INPUT_ON.store(on, Ordering::Relaxed);
}

#[cfg_attr(not(all(target_os = "linux", not(feature = "desktop"))), allow(dead_code))]
fn input_on() -> bool {
    INPUT_ON.load(Ordering::Relaxed)
}
