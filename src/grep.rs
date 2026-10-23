//! Search inside files, ripgrep-style, using ripgrep's own libraries.
//!
//! Folders are walked in parallel like ripgrep does (respecting `.gitignore`),
//! so results are always exact.  Whole drives in full-drive mode take their
//! file list from the drive index instead, which skips walking millions of
//! folders.  Either way, worker threads search the files as they are listed.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use globset::{Glob, GlobSet, GlobSetBuilder};
use grep_matcher::LineTerminator;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::sinks::Lossy;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder};
use ignore::{WalkBuilder, WalkState};

use crate::config::Config;
use crate::drive::{self, DriveIndex};

/// Longest line returned; longer ones (minified code, logs) are cut.
const MAX_LINE: usize = 400;

/// File types never worth reading for text, skipped unless globs or `binary`
/// say otherwise.  Binary files are also detected by content.
const BINARY_EXTENSIONS: &[&str] = &[
    "exe", "dll", "sys", "msi", "cab", "pdb", "lib", "obj", "o", "a", "so", "dylib", "class", "jar", "pyc", "zip",
    "7z", "rar", "gz", "tgz", "xz", "bz2", "zst", "iso", "img", "vhd", "vhdx", "vmdk", "jpg", "jpeg", "png", "gif",
    "bmp", "webp", "ico", "heic", "tif", "tiff", "psd", "mp3", "wav", "flac", "aac", "ogg", "m4a", "mp4", "mkv", "avi",
    "mov", "wmv", "webm", "ttf", "otf", "woff", "woff2", "pak", "bin", "dat", "db", "sqlite", "wmd",
];

#[derive(Clone, Debug, Default)]
pub struct GrepOptions {
    pub pattern: String,
    /// Files or folders to search.  A drive root (`C:\`) in full-drive mode is
    /// listed from the index; folders are walked unless `use_index`.
    pub paths: Vec<String>,
    pub ignore_case: bool,
    /// Treat the pattern as plain text, not a regular expression.
    pub fixed: bool,
    /// Only files whose name (or path, for globs with a slash) matches one of these.
    pub globs: Vec<String>,
    /// One result per matching file.
    pub files_only: bool,
    pub hidden: bool,
    /// Do not honour `.gitignore` and similar files when walking.
    pub no_ignore: bool,
    /// List folders on full-drive-mode drives from the index instead of walking them.
    pub use_index: bool,
    /// Also open files with binary extensions (binary content is still skipped).
    pub binary: bool,
    /// Stop after this many results (0 = no limit).
    pub limit: usize,
    pub timeout: Option<Duration>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct GrepMatch {
    pub path: String,
    pub line: u64,
    pub text: String,
}

/// Where the files came from, per searched path.
#[derive(Clone, Debug, serde::Serialize)]
pub struct GrepSource {
    pub path: String,
    /// "index", "walk" or "file".
    pub listed_by: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct GrepReport {
    pub matches: Vec<GrepMatch>,
    pub files_searched: usize,
    pub files_matched: usize,
    /// False when stopped by the limit or the timeout.
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped_by: Option<&'static str>,
    pub sources: Vec<GrepSource>,
    pub millis: f64,
}

enum Plan {
    File(String),
    Walk(String),
    Index(Box<DriveIndex>, String),
}

/// Which file names to open.
struct NameFilter {
    globs: Option<GlobSet>,
    binary: bool,
}

impl NameFilter {
    fn new(opts: &GrepOptions) -> Result<NameFilter> {
        let globs = if opts.globs.is_empty() {
            None
        } else {
            let mut b = GlobSetBuilder::new();
            for g in &opts.globs {
                b.add(Glob::new(&g.to_lowercase()).map_err(|e| anyhow!("bad glob {g:?}: {e}"))?);
            }
            Some(b.build()?)
        };
        Ok(NameFilter { globs, binary: opts.binary })
    }

    fn keep(&self, name: &[u8]) -> bool {
        match &self.globs {
            Some(set) => set.is_match(String::from_utf8_lossy(name).to_lowercase()),
            None => {
                self.binary
                    || name.rsplit(|&b| b == b'.').next().filter(|ext| ext.len() < name.len()).is_none_or(|ext| {
                        !BINARY_EXTENSIONS.iter().any(|known| known.as_bytes().eq_ignore_ascii_case(ext))
                    })
            }
        }
    }

    /// Globs containing a separator match the whole path; the rest match the name.
    fn keep_path(&self, path: &Path) -> bool {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.keep(name.as_bytes())
            || self.globs.as_ref().is_some_and(|set| set.is_match(path.to_string_lossy().to_lowercase()))
    }
}

/// Decide how each path is listed.
fn plan(cfg: &Config, opts: &GrepOptions, sources: &mut Vec<GrepSource>) -> Vec<Plan> {
    let mut plans = Vec::new();
    for raw in &opts.paths {
        let path = crate::paths::norm(raw);
        if Path::new(&path).is_file() {
            sources.push(GrepSource { path: path.clone(), listed_by: "file", note: None });
            plans.push(Plan::File(path));
            continue;
        }
        let root = drive::normalize_root(&path).filter(|_| path.len() >= 2 && path.as_bytes()[1] == b':');
        let is_root = path.len() <= 3;
        let indexed = root.as_ref().is_some_and(|r| cfg.drives.iter().any(|d| d.eq_ignore_ascii_case(r)));
        if indexed && (is_root || opts.use_index) {
            let root = root.unwrap();
            let volume = drive::Volume::open(&root).ok();
            match DriveIndex::open(&cfg.home, &root, volume.as_ref()) {
                Ok((index, _)) => {
                    let note = volume.is_none().then(|| {
                        "file list from the saved index (a few minutes old without administrator rights)".to_string()
                    });
                    sources.push(GrepSource { path: path.clone(), listed_by: "index", note });
                    plans.push(Plan::Index(Box::new(index), path));
                    continue;
                }
                Err(e) if is_root => {
                    sources.push(GrepSource {
                        path: path.clone(),
                        listed_by: "walk",
                        note: Some(format!("index unavailable ({e}); walking")),
                    });
                }
                Err(_) => sources.push(GrepSource { path: path.clone(), listed_by: "walk", note: None }),
            }
        } else {
            sources.push(GrepSource { path: path.clone(), listed_by: "walk", note: None });
        }
        plans.push(Plan::Walk(path));
    }
    plans
}

/// How far a running search has got.
#[derive(Clone, Copy, Debug)]
pub struct GrepProgress {
    pub files_searched: usize,
    pub matches: usize,
    pub elapsed: Duration,
}

/// Search file contents.  `on_match` sees each result as it is found (from
/// the calling thread); the report holds them all.
pub fn grep(cfg: &Config, opts: &GrepOptions, on_match: impl FnMut(&GrepMatch)) -> Result<GrepReport> {
    grep_with_progress(cfg, opts, on_match, |_| {})
}

/// [`grep`], also calling `on_progress` from the calling thread at least every
/// 200 ms while the search runs, so a long search on a cold disk can show that
/// it is still working.
pub fn grep_with_progress(
    cfg: &Config,
    opts: &GrepOptions,
    mut on_match: impl FnMut(&GrepMatch),
    mut on_progress: impl FnMut(&GrepProgress),
) -> Result<GrepReport> {
    let started = Instant::now();
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(opts.ignore_case)
        .fixed_strings(opts.fixed)
        .crlf(true)
        .build(&opts.pattern)
        .map_err(|e| anyhow!("bad pattern: {e}"))?;
    let filter = NameFilter::new(opts)?;
    let mut sources = Vec::new();
    let plans = plan(cfg, opts, &mut sources);
    if plans.is_empty() {
        return Err(anyhow!("nothing to search"));
    }

    let stop = Arc::new(AtomicBool::new(false));
    let searched = AtomicUsize::new(0);
    let (path_tx, path_rx) = bounded::<String>(4096);
    let (hit_tx, hit_rx) = bounded::<GrepMatch>(1024);
    let deadline = opts.timeout.map(|t| started + t);
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 16);

    let mut matches = Vec::new();
    let mut files_matched = 0;
    let mut last_path = String::new();
    let mut stopped_by = None;
    std::thread::scope(|s| {
        // Producers: list files into the path channel.
        let producer = {
            let (stop, filter, plans) = (&stop, &filter, &plans);
            s.spawn(move || {
                for p in plans {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    list(p, opts, filter, stop, &path_tx);
                }
            })
        };
        // Workers: search each listed file.
        for _ in 0..workers {
            let (rx, tx, matcher, stop, searched) =
                (path_rx.clone(), hit_tx.clone(), matcher.clone(), &stop, &searched);
            s.spawn(move || search_files(rx, tx, &matcher, stop, searched, opts.files_only));
        }
        drop(path_rx);
        drop(hit_tx);
        // Collect on this thread.
        loop {
            let wait = deadline.map_or(Duration::from_millis(200), |d| d.saturating_duration_since(Instant::now()));
            match hit_rx.recv_timeout(wait.min(Duration::from_millis(200))) {
                // Workers may still be finishing a file after the limit is reached.
                Ok(_) if stopped_by == Some("limit") => {}
                Ok(m) => {
                    if m.path != last_path {
                        files_matched += 1;
                        last_path.clone_from(&m.path);
                    }
                    on_match(&m);
                    matches.push(m);
                    if opts.limit > 0 && matches.len() >= opts.limit && stopped_by.is_none() {
                        stopped_by = Some("limit");
                        stop.store(true, Ordering::Relaxed);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if deadline.is_some_and(|d| Instant::now() >= d) && stopped_by.is_none() {
                stopped_by = Some("timeout");
                stop.store(true, Ordering::Relaxed);
            }
            on_progress(&GrepProgress {
                files_searched: searched.load(Ordering::Relaxed),
                matches: matches.len(),
                elapsed: started.elapsed(),
            });
        }
        let _ = producer.join();
    });
    // The loop can end before reporting the last files searched; report the totals.
    on_progress(&GrepProgress {
        files_searched: searched.load(Ordering::Relaxed),
        matches: matches.len(),
        elapsed: started.elapsed(),
    });
    // Files can interleave between workers; count distinct ones.
    if !opts.files_only {
        let mut paths: Vec<&str> = matches.iter().map(|m| m.path.as_str()).collect();
        paths.sort_unstable();
        paths.dedup();
        files_matched = paths.len();
    }
    Ok(GrepReport {
        matches,
        files_searched: searched.load(Ordering::Relaxed),
        files_matched,
        complete: stopped_by.is_none(),
        stopped_by,
        sources,
        millis: started.elapsed().as_secs_f64() * 1000.0,
    })
}

/// Send the files of one planned path to the workers.
fn list(plan: &Plan, opts: &GrepOptions, filter: &NameFilter, stop: &AtomicBool, tx: &Sender<String>) {
    let send = |path: String| !stop.load(Ordering::Relaxed) && tx.send(path).is_ok();
    match plan {
        Plan::File(path) => {
            send(path.clone());
        }
        Plan::Index(index, dir) => {
            index.for_each_file_under(dir, opts.hidden, |name| filter.keep(name), send);
        }
        Plan::Walk(dir) => {
            let walker = WalkBuilder::new(dir)
                .hidden(!opts.hidden)
                .ignore(!opts.no_ignore)
                .git_ignore(!opts.no_ignore)
                .git_global(!opts.no_ignore)
                .git_exclude(!opts.no_ignore)
                .parents(!opts.no_ignore)
                .build_parallel();
            walker.run(|| {
                let tx = tx.clone();
                Box::new(move |entry| {
                    if stop.load(Ordering::Relaxed) {
                        return WalkState::Quit;
                    }
                    let Ok(entry) = entry else { return WalkState::Continue };
                    if entry.file_type().is_some_and(|t| t.is_file())
                        && filter.keep_path(entry.path())
                        && tx.send(entry.path().to_string_lossy().into_owned()).is_err()
                    {
                        return WalkState::Quit;
                    }
                    WalkState::Continue
                })
            });
        }
    }
}

fn search_files(
    rx: Receiver<String>,
    tx: Sender<GrepMatch>,
    matcher: &RegexMatcher,
    stop: &AtomicBool,
    searched: &AtomicUsize,
    files_only: bool,
) {
    let mut searcher: Searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_terminator(LineTerminator::crlf())
        .line_number(true)
        .build();
    for path in rx {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        searched.fetch_add(1, Ordering::Relaxed);
        let _ = searcher.search_path(
            matcher,
            &path,
            Lossy(|line, text| {
                let text = text.trim_end_matches(['\r', '\n']);
                let cut = text.char_indices().nth(MAX_LINE).map_or(text.len(), |(i, _)| i);
                let m = GrepMatch { path: path.clone(), line, text: text[..cut].to_string() };
                Ok(tx.send(m).is_ok() && !files_only && !stop.load(Ordering::Relaxed))
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(pattern: &str, dir: &Path) -> GrepOptions {
        GrepOptions { pattern: pattern.into(), paths: vec![dir.to_string_lossy().into_owned()], ..Default::default() }
    }

    fn found(report: &GrepReport) -> Vec<(String, u64)> {
        let mut v: Vec<_> = report
            .matches
            .iter()
            .map(|m| (Path::new(&m.path).file_name().unwrap().to_string_lossy().into_owned(), m.line))
            .collect();
        v.sort();
        v
    }

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir_all(p.join("src")).unwrap();
        std::fs::write(p.join("src/main.rs"), "fn main() {\r\n    println!(\"Hello\");\r\n}\r\n").unwrap();
        std::fs::write(p.join("notes.txt"), "hello world\nnothing here\nHELLO again\n").unwrap();
        std::fs::write(p.join("blob.dat"), b"hello\0binary").unwrap();
        std::fs::write(p.join("data.bin.txt"), b"hello\0\0\0 binary content").unwrap();
        dir
    }

    #[test]
    fn finds_lines_with_numbers_and_skips_binary_files() {
        let dir = tree();
        let cfg = Config::default();
        let r = grep(&cfg, &GrepOptions { ignore_case: true, ..opts("hello", dir.path()) }, |_| {}).unwrap();
        assert_eq!(found(&r), [("main.rs".into(), 2), ("notes.txt".into(), 1), ("notes.txt".into(), 3)]);
        assert_eq!((r.files_matched, r.complete), (2, true));
        let strict = grep(&cfg, &opts("hello", dir.path()), |_| {}).unwrap();
        assert_eq!(found(&strict), [("notes.txt".into(), 1)]);
    }

    #[test]
    fn line_ends_and_regexes() {
        let dir = tree();
        let r = grep(&Config::default(), &opts(r"^\}$", dir.path()), |_| {}).unwrap();
        assert_eq!(found(&r), [("main.rs".into(), 3)], "CRLF lines match `$`");
        assert_eq!(r.matches[0].text, "}");
        let fixed = GrepOptions { fixed: true, ..opts("println!(", dir.path()) };
        assert_eq!(grep(&Config::default(), &fixed, |_| {}).unwrap().matches.len(), 1);
        assert!(grep(&Config::default(), &opts("(", dir.path()), |_| {}).is_err(), "bad regex is an error");
    }

    #[test]
    fn globs_files_only_and_limits() {
        let dir = tree();
        let cfg = Config::default();
        let rs = GrepOptions { globs: vec!["*.RS".into()], ignore_case: true, ..opts("hello", dir.path()) };
        assert_eq!(found(&grep(&cfg, &rs, |_| {}).unwrap()), [("main.rs".into(), 2)]);
        let files = GrepOptions { files_only: true, ignore_case: true, ..opts("hello", dir.path()) };
        assert_eq!(grep(&cfg, &files, |_| {}).unwrap().matches.len(), 2);
        let limited = GrepOptions { limit: 1, ignore_case: true, ..opts("hello", dir.path()) };
        let r = grep(&cfg, &limited, |_| {}).unwrap();
        assert_eq!((r.matches.len(), r.complete, r.stopped_by), (1, false, Some("limit")));
    }

    #[test]
    fn gitignored_files_are_skipped_unless_asked() {
        let dir = tree();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "notes.txt\n").unwrap();
        let cfg = Config::default();
        assert!(found(&grep(&cfg, &opts("world", dir.path()), |_| {}).unwrap()).is_empty());
        let all = GrepOptions { no_ignore: true, ..opts("world", dir.path()) };
        assert_eq!(found(&grep(&cfg, &all, |_| {}).unwrap()), [("notes.txt".into(), 1)]);
    }

    #[test]
    fn reports_progress_until_done() {
        let dir = tree();
        let mut last = None;
        let r =
            grep_with_progress(&Config::default(), &opts("hello", dir.path()), |_| {}, |p| last = Some(*p)).unwrap();
        let last = last.expect("progress is reported at least once");
        assert_eq!((last.files_searched, last.matches), (r.files_searched, r.matches.len()));
    }
}
