use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_SIZE: u64 = 1024 * 1024;
static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();

pub fn init(app_dir: &Path) {
    let log_dir = app_dir.join("logs");
    if fs::create_dir_all(&log_dir).is_err() {
        return;
    }
    let current = log_dir.join("capslockrs.log");
    let previous = log_dir.join("capslockrs.log.1");
    if current
        .metadata()
        .is_ok_and(|meta| meta.len() >= MAX_LOG_SIZE)
    {
        let _ = fs::remove_file(&previous);
        let _ = fs::rename(&current, &previous);
    }
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(current) {
        let _ = LOG_FILE.set(Mutex::new(file));
    }
}

pub fn log(message: impl AsRef<str>) {
    let Some(file) = LOG_FILE.get() else {
        return;
    };
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    if let Ok(mut file) = file.lock() {
        let _ = writeln!(file, "[{timestamp}] {}", message.as_ref());
        let _ = file.flush();
    }
}
