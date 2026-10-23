//! Minimal logging: timestamped lines to `bettergrep.log` (rotated at 5 MB) and,
//! unless quiet, to stderr.  Deliberately dependency-free.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

struct Sink {
    file: Option<PathBuf>,
    stderr: bool,
    verbose: bool,
}

static SINK: Mutex<Sink> = Mutex::new(Sink { file: None, stderr: true, verbose: false });
const MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Send log lines to `<home>/bettergrep.log` (and stderr unless `quiet`).
pub fn init(home: &str, quiet: bool, verbose: bool) {
    let _ = std::fs::create_dir_all(home);
    let mut sink = SINK.lock().unwrap();
    sink.file = Some(PathBuf::from(home).join("bettergrep.log"));
    sink.stderr = !quiet;
    sink.verbose = verbose;
}

fn write(level: &str, msg: &str) {
    let sink = SINK.lock().unwrap();
    if level == "DEBUG" && !sink.verbose {
        return;
    }
    let line = format!("{} {level} {msg}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"));
    if sink.stderr {
        let _ = std::io::stderr().write_all(line.as_bytes());
    }
    if let Some(path) = &sink.file {
        if std::fs::metadata(path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
            let _ = std::fs::rename(path, path.with_extension("log.1"));
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

pub fn info(msg: &str) {
    write("INFO", msg);
}

pub fn warn(msg: &str) {
    write("WARNING", msg);
}

pub fn error(msg: &str) {
    write("ERROR", msg);
}

pub fn debug(msg: &str) {
    write("DEBUG", msg);
}
