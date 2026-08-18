// Mirrors daemon/src/log.js: one truncated-on-start log file plus stdout,
// lines capped at 800 chars, stamped HH:MM:SS.mmm (UTC, matching the JS
// version's `toISOString().slice(11, 23)`).

use chrono::Utc;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();

pub fn init(log_dir: &Path) {
    if let Err(err) = fs::create_dir_all(log_dir) {
        eprintln!("log: could not create {}: {err}", log_dir.display());
        return;
    }
    match OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(log_dir.join("daemon.log"))
    {
        Ok(file) => {
            let _ = LOG_FILE.set(Mutex::new(file));
        }
        Err(err) => eprintln!("log: could not open daemon.log: {err}"),
    }
}

fn stamp() -> String {
    Utc::now().format("%H:%M:%S%.3f").to_string()
}

pub fn log(tag: &str, msg: impl AsRef<str>) {
    let line = format!("{} {} {}", stamp(), tag, msg.as_ref());
    let line: String = line.chars().take(800).collect();
    println!("{line}");
    if let Some(m) = LOG_FILE.get() {
        if let Ok(mut f) = m.lock() {
            let _ = writeln!(f, "{line}");
        }
    }
}
