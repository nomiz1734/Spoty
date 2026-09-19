use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Level, LevelFilter, Log, Metadata, Record};

const MAX_LOG_BYTES: u64 = 512 * 1024;

struct FileLogger {
    file: Mutex<Option<File>>,
    level: LevelFilter,
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        if metadata.level() > self.level {
            return false;
        }
        // librespot is chatty at debug level; keep only our own debug output.
        let target = metadata.target();
        metadata.level() <= Level::Info || target.starts_with("spoty")
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let line = format!(
            "{:02}:{:02}:{:02} {:<5} [{}] {}\n",
            (secs / 3600) % 24,
            (secs / 60) % 60,
            secs % 60,
            record.level(),
            record.target(),
            record.args()
        );
        // On the device stderr goes to launch.log; keep it for loader errors only.
        #[cfg(any(feature = "desktop", not(target_os = "linux")))]
        eprint!("{line}");
        if let Ok(mut guard) = self.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.write_all(line.as_bytes());
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.flush();
            }
        }
    }
}

pub fn init(path: &Path) {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_LOG_BYTES {
            let _ = std::fs::rename(path, path.with_extension("log.old"));
        }
    }
    let file = OpenOptions::new().create(true).append(true).open(path).ok();
    let level = match std::env::var("SPOTY_LOG").as_deref() {
        Ok("debug") => LevelFilter::Debug,
        Ok("trace") => LevelFilter::Trace,
        _ => LevelFilter::Info,
    };
    let logger = Box::new(FileLogger {
        file: Mutex::new(file),
        level,
    });
    if log::set_boxed_logger(logger).is_ok() {
        log::set_max_level(level);
    }
}
