//! Reads buttons straight from /dev/input/event* (TrimUI exposes the pad as a
//! gamepad with a hat for the D-pad, and power/volume on a separate keys device).

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::mpsc::Sender;

use crate::config::Config;
use crate::ui::UiMsg;

use super::Button;

const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_Z: u16 = 0x02;
const ABS_RZ: u16 = 0x05;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;
const KEY_POWER: u16 = 116;
const EVENT_SIZE: usize = 24; // struct input_event on 64-bit

const EVIOCGRAB: libc::c_ulong = 0x4004_4590;

fn eviocgname(len: usize) -> libc::c_ulong {
    0x8000_4506 | ((len as libc::c_ulong) << 16)
}
fn eviocgbit(ev: u16, len: usize) -> libc::c_ulong {
    0x8000_4520 | ev as libc::c_ulong | ((len as libc::c_ulong) << 16)
}
fn eviocgabs(abs: u16) -> libc::c_ulong {
    0x8018_4540 + abs as libc::c_ulong
}

fn default_keymap() -> HashMap<u16, Button> {
    use Button::*;
    HashMap::from([
        // TrimUI gamepad (Nintendo layout: A is the east button).
        (305, A),
        (304, B),
        (308, X),
        (307, Y),
        (310, L1),
        (311, R1),
        (312, L2),
        (313, R2),
        (314, Select),
        (315, Start),
        (316, Menu),
        (139, Menu),
        (1, Menu), // KEY_ESC on some firmwares
        (KEY_POWER, Power),
        (115, VolUp),
        (114, VolDown),
        // D-pad reported as keys on some kernels.
        (103, Up),
        (108, Down),
        (105, Left),
        (106, Right),
        (544, Up),
        (545, Down),
        (546, Left),
        (547, Right),
        // Stick presses (BTN_THUMBL / BTN_THUMBR).
        (317, L3),
        (318, R3),
    ])
}

struct Device {
    file: std::fs::File,
    name: String,
    grabbed: bool,
    trigger_max: HashMap<u16, i32>,
    hat: [i32; 2],
    triggers: HashMap<u16, bool>,
    /// Left stick ranges (min, max) for ABS_X / ABS_Y, and its current direction.
    stick_range: [Option<(i32, i32)>; 2],
    stick: [i32; 2],
}

fn has_key(fd: i32, code: u16) -> bool {
    let mut bits = [0u8; 96];
    let r = unsafe { libc::ioctl(fd, eviocgbit(EV_KEY, bits.len()) as _, bits.as_mut_ptr()) };
    r >= 0 && (bits[code as usize / 8] >> (code % 8)) & 1 == 1
}

fn abs_range(fd: i32, abs: u16) -> Option<(i32, i32)> {
    let mut info = [0i32; 6]; // value, min, max, fuzz, flat, resolution
    let r = unsafe { libc::ioctl(fd, eviocgabs(abs) as _, info.as_mut_ptr()) };
    (r >= 0 && info[2] > info[1]).then_some((info[1], info[2]))
}

fn abs_max(fd: i32, abs: u16) -> Option<i32> {
    abs_range(fd, abs).map(|r| r.1)
}

fn open_devices(cfg: &Config) -> Vec<Device> {
    let mut devices = Vec::new();
    for i in 0..16 {
        let path = format!("/dev/input/event{i}");
        let Ok(file) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
        else {
            continue;
        };
        let fd = file.as_raw_fd();
        let mut name_buf = [0u8; 128];
        unsafe { libc::ioctl(fd, eviocgname(name_buf.len()) as _, name_buf.as_mut_ptr()) };
        let name = String::from_utf8_lossy(&name_buf)
            .trim_end_matches('\0')
            .to_string();
        let mut grabbed = false;
        if cfg.grab_power_button && has_key(fd, KEY_POWER) {
            grabbed = unsafe { libc::ioctl(fd, EVIOCGRAB as _, 1 as libc::c_int) } == 0;
        }
        let mut trigger_max = HashMap::new();
        for abs in [ABS_Z, ABS_RZ] {
            if let Some(max) = abs_max(fd, abs) {
                trigger_max.insert(abs, max);
            }
        }
        let stick_range = [abs_range(fd, ABS_X), abs_range(fd, ABS_Y)];
        log::info!("input: {path} \"{name}\" grabbed={grabbed} stick={stick_range:?}");
        devices.push(Device {
            file,
            name,
            grabbed,
            trigger_max,
            hat: [0, 0],
            triggers: HashMap::new(),
            stick_range,
            stick: [0, 0],
        });
    }
    devices
}

pub fn spawn(cfg: &Config, tx: Sender<UiMsg>) {
    let mut keymap = default_keymap();
    for (code, name) in &cfg.keymap {
        match (code.parse::<u16>(), Button::from_name(name)) {
            (Ok(c), Some(b)) => {
                keymap.insert(c, b);
            }
            _ => log::warn!("keymap entry {code} -> {name} ignored"),
        }
    }
    let cfg = cfg.clone();
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || run(cfg, keymap, tx))
        .expect("spawn input thread");
}

fn run(cfg: Config, keymap: HashMap<u16, Button>, tx: Sender<UiMsg>) {
    let mut devices = open_devices(&cfg);
    if devices.is_empty() {
        log::error!("input: no /dev/input/event* devices could be opened");
        return;
    }
    let send = |b: Button, pressed: bool| tx.send(UiMsg::Input(b, pressed)).is_ok();
    let mut buf = [0u8; EVENT_SIZE * 64];
    loop {
        let mut fds: Vec<libc::pollfd> = devices
            .iter()
            .map(|d| libc::pollfd {
                fd: d.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
        if r < 0 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        for (idx, pfd) in fds.iter().enumerate() {
            if pfd.revents & libc::POLLIN == 0 {
                continue;
            }
            let dev = &mut devices[idx];
            let n = match dev.file.read(&mut buf) {
                Ok(n) => n,
                Err(_) => continue,
            };
            for ev in buf[..n].chunks_exact(EVENT_SIZE) {
                let kind = u16::from_le_bytes([ev[16], ev[17]]);
                let code = u16::from_le_bytes([ev[18], ev[19]]);
                let value = i32::from_le_bytes([ev[20], ev[21], ev[22], ev[23]]);
                match kind {
                    EV_KEY => {
                        if value == 2 {
                            continue; // kernel autorepeat; the UI does its own
                        }
                        let Some(&b) = keymap.get(&code) else {
                            log::debug!("input: unmapped key {code} on {}", dev.name);
                            continue;
                        };
                        // Volume keys belong to the system unless we own the device.
                        if matches!(b, Button::VolUp | Button::VolDown) && !dev.grabbed {
                            continue;
                        }
                        if !send(b, value != 0) {
                            return;
                        }
                    }
                    EV_ABS if code == ABS_HAT0X || code == ABS_HAT0Y => {
                        let axis = (code - ABS_HAT0X) as usize;
                        let old = dev.hat[axis];
                        let new = value.signum();
                        if old == new {
                            continue;
                        }
                        dev.hat[axis] = new;
                        let (neg, pos) = if axis == 0 {
                            (Button::Left, Button::Right)
                        } else {
                            (Button::Up, Button::Down)
                        };
                        if old < 0 {
                            send(neg, false);
                        } else if old > 0 {
                            send(pos, false);
                        }
                        if new < 0 {
                            send(neg, true);
                        } else if new > 0 {
                            send(pos, true);
                        }
                    }
                    EV_ABS if code == ABS_X || code == ABS_Y => {
                        // Left stick works like the D-pad, with hysteresis so it doesn't chatter.
                        let axis = (code - ABS_X) as usize;
                        let Some((min, max)) = dev.stick_range[axis] else { continue };
                        let center = (min + max) as f32 / 2.0;
                        let half = ((max - min) as f32 / 2.0).max(1.0);
                        let norm = (value as f32 - center) / half;
                        let old = dev.stick[axis];
                        let new = if norm.abs() > 0.55 {
                            norm.signum() as i32
                        } else if norm.abs() < 0.35 {
                            0
                        } else {
                            old
                        };
                        if new == old {
                            continue;
                        }
                        dev.stick[axis] = new;
                        let (neg, pos) = if axis == 0 {
                            (Button::Left, Button::Right)
                        } else {
                            (Button::Up, Button::Down)
                        };
                        if old < 0 {
                            send(neg, false);
                        } else if old > 0 {
                            send(pos, false);
                        }
                        if new < 0 {
                            send(neg, true);
                        } else if new > 0 {
                            send(pos, true);
                        }
                    }
                    EV_ABS if code == ABS_Z || code == ABS_RZ => {
                        let max = dev.trigger_max.get(&code).copied().unwrap_or(255);
                        let pressed = value > max / 2;
                        let was = dev.triggers.insert(code, pressed).unwrap_or(false);
                        if was != pressed {
                            let b = if code == ABS_Z { Button::L2 } else { Button::R2 };
                            send(b, pressed);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
