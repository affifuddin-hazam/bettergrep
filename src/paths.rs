//! Path normalisation, comparison keys, the data directory and ignore rules.

use std::path::Path;

/// Name of the per-folder index file.
pub const INDEX_FILENAME: &str = ".bettergrep.json";
/// Suffix of the temporary file written before the index is atomically replaced.
pub const INDEX_TMP_SUFFIX: &str = ".bettergrep-tmp";

/// Absolute, normalised path that keeps its original case (`C:\Users\Me\file.txt`).
pub fn norm(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut s = strip_verbatim(&abs.to_string_lossy()).replace('/', "\\");
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    s
}

/// `\\?\C:\x` -> `C:\x`, `\\?\UNC\srv\share` -> `\\srv\share`.
pub fn strip_verbatim(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.to_string()
    }
}

/// Case-folded comparison key (Windows paths are case-insensitive).
pub fn key(path: impl AsRef<Path>) -> String {
    norm(path).to_lowercase()
}

/// Parent folder of a normalised path (the path itself for a root).
pub fn parent(path: &str) -> String {
    match Path::new(path).parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_string_lossy().into_owned(),
        _ => path.to_string(),
    }
}

pub fn parent_key(path: &str) -> String {
    parent(&norm(path)).to_lowercase()
}

pub fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string())
}

/// `[low, high)` key range covering every descendant of `dir_key`, usable with an index.
pub fn descendant_range(dir_key: &str) -> (String, String) {
    let base = format!("{}\\", dir_key.trim_end_matches('\\'));
    let high = format!("{}]", &base[..base.len() - 1]); // ']' sorts right after '\'
    (base, high)
}

pub fn is_within(path_key: &str, dir_key: &str) -> bool {
    let (low, high) = descendant_range(dir_key);
    path_key == dir_key || (path_key >= low.as_str() && path_key < high.as_str())
}

/// Data directory: `%BETTERGREP_HOME%` or `%LOCALAPPDATA%\bettergrep`.
pub fn data_dir() -> String {
    if let Ok(home) = std::env::var("BETTERGREP_HOME")
        && !home.is_empty()
    {
        return norm(home);
    }
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| {
        let profile = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
        format!(r"{profile}\AppData\Local")
    });
    norm(Path::new(&base).join("bettergrep"))
}

/// Shell-style wildcard match (`*`, `?`, `[abc]`, `[!abc]`); `*` also matches separators.
pub fn fnmatch(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() {
            match p[pi] {
                '*' => {
                    star = Some(pi);
                    mark = ti;
                    pi += 1;
                    continue;
                }
                '?' => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                '[' => match class_match(&p, pi, t[ti]) {
                    Some((true, next)) => {
                        pi = next;
                        ti += 1;
                        continue;
                    }
                    Some((false, _)) => {}
                    None if t[ti] == '[' => {
                        pi += 1;
                        ti += 1;
                        continue;
                    }
                    None => {}
                },
                c if c == t[ti] => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some(s) => {
                pi = s + 1;
                mark += 1;
                ti = mark;
            }
            None => return false,
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Match `c` against the `[...]` class starting at `p[start]`; returns (matched, index after `]`).
fn class_match(p: &[char], start: usize, c: char) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let negate = i < p.len() && (p[i] == '!' || p[i] == '^');
    if negate {
        i += 1;
    }
    let mut matched = false;
    let first = i;
    while i < p.len() && (p[i] != ']' || i == first) {
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            if p[i] <= c && c <= p[i + 2] {
                matched = true;
            }
            i += 3;
        } else {
            if p[i] == c {
                matched = true;
            }
            i += 1;
        }
    }
    if i >= p.len() {
        return None; // unterminated class: treat '[' literally
    }
    Some((matched != negate, i + 1))
}

/// Decides which paths are never recorded.
///
/// Patterns without `/` are matched against every path component (so
/// `node_modules` hides a whole tree and `*.tmp` hides temp files); patterns with
/// `/` are matched against the whole path written with forward slashes.
/// Matching is case-insensitive.
#[derive(Clone, Debug, Default)]
pub struct IgnoreRules {
    name_patterns: Vec<String>,
    path_patterns: Vec<String>,
    excluded: Vec<String>,
}

impl IgnoreRules {
    pub fn new<S: AsRef<str>>(patterns: &[S], excluded_dirs: &[S]) -> Self {
        let mut rules = IgnoreRules::default();
        let always = [INDEX_FILENAME.to_string(), format!("*{INDEX_TMP_SUFFIX}")];
        for pattern in always.iter().map(String::as_str).chain(patterns.iter().map(AsRef::as_ref)) {
            let pattern = pattern.trim().replace('\\', "/").to_lowercase();
            if pattern.is_empty() {
                continue;
            }
            if pattern.contains('/') {
                rules.path_patterns.push(pattern);
            } else {
                rules.name_patterns.push(pattern);
            }
        }
        rules.excluded = excluded_dirs.iter().map(|d| key(d.as_ref())).collect();
        rules
    }

    pub fn is_ignored(&self, path: &str) -> bool {
        let k = key(path);
        if self.excluded.iter().any(|d| is_within(&k, d)) {
            return true;
        }
        let slashed = k.replace('\\', "/");
        if slashed
            .split('/')
            .filter(|part| !part.is_empty())
            .any(|part| self.name_patterns.iter().any(|p| fnmatch(p, part)))
        {
            return true;
        }
        self.path_patterns.iter().any(|p| fnmatch(p, &slashed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_matching() {
        assert!(fnmatch("*.tmp", "x.tmp"));
        assert!(fnmatch("~$*", "~$draft.docx"));
        assert!(fnmatch("*/cache/*", "c:/app/cache/blob"));
        assert!(fnmatch("file[0-9].txt", "file7.txt"));
        assert!(!fnmatch("file[!0-9].txt", "file7.txt"));
        assert!(!fnmatch("*.tmp", "x.tmpx"));
        assert!(fnmatch("a*b*c", "aXXbYYc"));
    }

    #[test]
    fn keys_and_ranges() {
        assert_eq!(key(r"C:\Users\Me\..\Me\File.TXT"), r"c:\users\me\file.txt");
        assert!(is_within(r"c:\a\b\c.txt", r"c:\a"));
        assert!(!is_within(r"c:\ab\c.txt", r"c:\a"));
        assert_eq!(strip_verbatim(r"\\?\C:\x"), r"C:\x");
        assert_eq!(parent(r"C:\a\b.txt"), r"C:\a");
    }

    #[test]
    fn ignore_rules() {
        let rules = IgnoreRules::new(&["*.tmp", "node_modules", "*/cache/*"], &[r"C:\Data\bettergrep"]);
        assert!(rules.is_ignored(r"C:\x\A.TMP"));
        assert!(rules.is_ignored(r"C:\proj\node_modules\a\b.js"));
        assert!(rules.is_ignored(r"C:\app\cache\blob"));
        assert!(rules.is_ignored(r"C:\Data\bettergrep\bettergrep.db"));
        assert!(rules.is_ignored(r"C:\x\.bettergrep.json"));
        assert!(!rules.is_ignored(r"C:\x\report.pdf"));
    }
}
