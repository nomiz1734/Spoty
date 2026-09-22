//! Running in the background: Spoty hands the screen and the buttons back to
//! the system menu while the music keeps playing, and comes back when it is
//! launched again.
//!
//! launch.sh starts the player in a session of its own, then runs
//! `spoty --attach`, which stands in for the app in the launcher: it asks the
//! player to take the screen and waits until the player gives it back.
//! Launching Spoty while it plays in the background runs `--attach` again.
//!
//! The two talk over TCP on 127.0.0.1, on a port the player writes to
//! `PORT_FILE`, one line per message. The launcher sends `show`; the player
//! answers `shown` once it is on screen, then `bg` (back to the system menu,
//! music still playing) or `quit`, always after it has let go of the screen.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::ui::UiMsg;

/// How long a player that was just started may take to open its port.
const START_WAIT: Duration = Duration::from_secs(20);
/// How long a running player may take to come back on screen before it
/// counts as hung (launch.sh then starts a fresh one).
const SHOW_WAIT: Duration = Duration::from_secs(10);
/// How long to wait for the player to start again after it stopped while on
/// screen (an update being installed, or a rollback after a crash).
const RESTART_WAIT: Duration = Duration::from_secs(60);

fn port_file() -> PathBuf {
    if cfg!(unix) {
        PathBuf::from("/tmp/spoty.port")
    } else {
        std::env::temp_dir().join("spoty.port")
    }
}

/// Whether launch.sh's loop that restarts the player (after an update or a
/// crash) is still running.
#[cfg(unix)]
fn supervisor_alive() -> bool {
    std::fs::read_to_string("/tmp/spoty-serve.pid")
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .is_some_and(|pid| pid > 0 && unsafe { libc::kill(pid, 0) } == 0)
}

#[cfg(not(unix))]
fn supervisor_alive() -> bool {
    false
}

// ------------------------------------------------------------------ player

/// A launcher waiting while the player is on screen.
pub struct Holder(TcpStream);

impl Holder {
    fn say(&mut self, msg: &str) {
        let _ = self.0.write_all(format!("{msg}\n").as_bytes());
        let _ = self.0.flush();
    }

    /// The player is on screen.
    pub fn shown(&mut self) {
        self.say("shown");
    }

    /// The player gave the screen back: to the background, or for good.
    pub fn release(mut self, quit: bool) {
        self.say(if quit { "quit" } else { "bg" });
    }
}

/// Listens for launchers. Returns false if it cannot, in which case the app
/// has no way back on screen and must not go to the background.
pub fn listen(tx: Sender<UiMsg>) -> bool {
    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("background: cannot listen: {e}");
            return false;
        }
    };
    let port = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(_) => return false,
    };
    if let Err(e) = std::fs::write(port_file(), port.to_string()) {
        log::warn!("background: cannot write {}: {e}", port_file().display());
        return false;
    }
    let spawned = std::thread::Builder::new()
        .name("attach".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(stream) = conn else { continue };
                if wants_show(&stream) && tx.send(UiMsg::Show(Holder(stream))).is_err() {
                    return;
                }
            }
        });
    spawned.is_ok()
}

fn wants_show(stream: &TcpStream) -> bool {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let Ok(read) = stream.try_clone() else { return false };
    let mut line = String::new();
    let ok = BufReader::new(read).read_line(&mut line).is_ok() && line.trim() == "show";
    let _ = stream.set_read_timeout(None);
    ok
}

/// A holder and the launcher's end of it, without the port file: for the
/// screenshot tool and tests.
pub fn pair() -> std::io::Result<(Holder, TcpStream)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let launcher = TcpStream::connect(listener.local_addr()?)?;
    let (player, _) = listener.accept()?;
    Ok((Holder(player), launcher))
}

/// Removes the port file on the way out, so the next launch starts afresh.
pub fn forget() {
    let _ = std::fs::remove_file(port_file());
}

// ---------------------------------------------------------------- launcher

enum Session {
    /// The player went to the background or quit.
    Released,
    /// The connection dropped without a word: the player stopped.
    Lost,
    /// The player took the request but never came on screen.
    Hung,
}

fn connect() -> Option<TcpStream> {
    let port: u16 = std::fs::read_to_string(port_file()).ok()?.trim().parse().ok()?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()
}

fn session(stream: TcpStream) -> Session {
    let Ok(mut write) = stream.try_clone() else { return Session::Lost };
    if write.write_all(b"show\n").is_err() {
        return Session::Lost;
    }
    let _ = stream.set_read_timeout(Some(SHOW_WAIT));
    let mut read = BufReader::new(stream);
    let mut line = String::new();
    match read.read_line(&mut line) {
        Ok(0) => return Session::Lost,
        Ok(_) if line.trim() == "shown" => {}
        Ok(_) => return Session::Lost,
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            return Session::Hung
        }
        Err(_) => return Session::Lost,
    }
    let _ = read.get_ref().set_read_timeout(None);
    loop {
        line.clear();
        match read.read_line(&mut line) {
            Ok(0) | Err(_) => return Session::Lost,
            Ok(_) if matches!(line.trim(), "bg" | "quit") => return Session::Released,
            Ok(_) => {}
        }
    }
}

/// `spoty --attach [--wait]`, run by launch.sh in the launcher's place: puts
/// the running player on screen and returns once it has left it again.
/// `wait` gives a player that is only starting time to open its port.
///
/// Exit codes: 0 the player left the screen (or quit), 1 no player is
/// running, 2 a player is running but did not come back on screen.
pub fn attach(wait: bool) -> i32 {
    let mut deadline = Instant::now() + if wait { START_WAIT } else { Duration::ZERO };
    let mut attached = false;
    loop {
        let stream = loop {
            if let Some(s) = connect() {
                break Some(s);
            }
            if Instant::now() >= deadline || (attached && !supervisor_alive()) {
                break None;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let Some(stream) = stream else {
            return if attached { 0 } else { 1 };
        };
        match session(stream) {
            Session::Released => return 0,
            Session::Hung => return 2,
            Session::Lost => {
                attached = true;
                deadline = Instant::now() + RESTART_WAIT;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A player on another thread: answers `show`, then leaves the screen.
    #[test]
    fn attach_waits_until_the_player_leaves_the_screen() {
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(listen(tx));
        let player = std::thread::spawn(move || {
            let Ok(UiMsg::Show(mut h)) = rx.recv() else { panic!("no show") };
            h.shown();
            std::thread::sleep(Duration::from_millis(200));
            h.release(false);
        });
        let t0 = Instant::now();
        assert_eq!(attach(false), 0);
        assert!(t0.elapsed() >= Duration::from_millis(200), "returned before the player left");
        player.join().unwrap();
        forget();
        assert_eq!(attach(false), 1, "no player once the port file is gone");
    }
}
