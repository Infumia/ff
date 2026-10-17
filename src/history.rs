//! Persistent query history and "frecency" (frequency + recency) of picked
//! files, stored in a small text file:
//!
//! ```text
//! q<TAB>query
//! f<TAB>count<TAB>last-picked-unix-seconds<TAB>absolute path
//! ```
//!
//! Location: `$FF_HISTORY`, else `%LOCALAPPDATA%\ff\history` on Windows,
//! `$XDG_DATA_HOME/ff/history` or `~/.local/share/ff/history` elsewhere.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_QUERIES: usize = 200;
const MAX_PICKS: usize = 1000;
/// Upper bound of the ranking bonus; a strong fuzzy match is worth ~100-200.
const MAX_BONUS: i64 = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pick {
    count: u32,
    last: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    path: Option<PathBuf>,
    /// Oldest first, without duplicates.
    queries: Vec<String>,
    picks: HashMap<String, Pick>,
}

pub fn default_path() -> Option<PathBuf> {
    if let Some(p) = env::var_os("FF_HISTORY").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let base = if cfg!(windows) {
        env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        env::var_os("XDG_DATA_HOME")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| Path::new(&h).join(".local").join("share")))
    };
    base.map(|b| b.join("ff").join("history"))
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl History {
    /// Loads the history at `path`; a missing or unreadable file gives an
    /// empty history that will still be saved there.
    pub fn load(path: Option<PathBuf>) -> Self {
        let text = path.as_ref().and_then(|p| fs::read_to_string(p).ok()).unwrap_or_default();
        History { path, ..Self::parse(&text) }
    }

    pub fn parse(text: &str) -> Self {
        let mut h = History::default();
        for line in text.lines() {
            let mut parts = line.splitn(2, '\t');
            match (parts.next(), parts.next()) {
                (Some("q"), Some(q)) => h.add_query(q),
                (Some("f"), Some(rest)) => {
                    let mut f = rest.splitn(3, '\t');
                    if let (Some(Ok(count)), Some(Ok(last)), Some(path)) =
                        (f.next().map(str::parse), f.next().map(str::parse), f.next())
                    {
                        h.picks.insert(path.to_string(), Pick { count, last });
                    }
                }
                _ => {}
            }
        }
        h
    }

    pub fn serialize(&self) -> String {
        let mut out = String::new();
        for q in &self.queries {
            out.push_str(&format!("q\t{q}\n"));
        }
        let mut picks: Vec<_> = self.picks.iter().collect();
        picks.sort_by(|a, b| b.1.last.cmp(&a.1.last).then(a.0.cmp(b.0)));
        for (path, p) in picks.into_iter().take(MAX_PICKS) {
            out.push_str(&format!("f\t{}\t{}\t{}\n", p.count, p.last, path));
        }
        out
    }

    pub fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, self.serialize())
    }

    /// Oldest first.
    pub fn queries(&self) -> &[String] {
        &self.queries
    }

    pub fn add_query(&mut self, query: &str) {
        let q = query.trim();
        if q.is_empty() || q.contains(['\n', '\t']) {
            return;
        }
        self.queries.retain(|old| old != q);
        self.queries.push(q.to_string());
        if self.queries.len() > MAX_QUERIES {
            self.queries.remove(0);
        }
    }

    /// Records that `abs_path` was picked at time `now`.
    pub fn record_pick(&mut self, abs_path: &str, now: u64) {
        if abs_path.contains(['\n', '\t']) {
            return;
        }
        let pick = self.picks.entry(abs_path.to_string()).or_insert(Pick { count: 0, last: now });
        pick.count = pick.count.saturating_add(1);
        pick.last = now;
    }

    /// Ranking bonus for a file: more picks and more recent picks score
    /// higher, capped at `MAX_BONUS`.
    pub fn frecency(&self, abs_path: &str, now: u64) -> i64 {
        let Some(p) = self.picks.get(abs_path) else { return 0 };
        let age = now.saturating_sub(p.last);
        const HOUR: u64 = 3_600;
        const DAY: u64 = 24 * HOUR;
        let weight = if age <= HOUR {
            8
        } else if age <= DAY {
            4
        } else if age <= 7 * DAY {
            2
        } else if age <= 30 * DAY {
            1
        } else {
            0
        };
        (i64::from(p.count.min(16)) * weight).min(MAX_BONUS)
    }

    /// Bonuses for picked files under `root`, keyed by the path as `ff`
    /// displays it (relative to `.`, or prefixed with `root`).
    pub fn boosts_under(&self, root: &Path, now: u64) -> HashMap<String, i64> {
        let Ok(abs_root) = root.canonicalize() else { return HashMap::new() };
        let abs_root = strip_verbatim(&abs_root);
        self.picks
            .keys()
            .filter_map(|abs| {
                let rel = Path::new(abs).strip_prefix(&abs_root).ok()?;
                let shown = if root == Path::new(".") { rel.to_path_buf() } else { root.join(rel) };
                let bonus = self.frecency(abs, now);
                (bonus > 0).then(|| (shown.to_string_lossy().into_owned(), bonus))
            })
            .collect()
    }
}

/// The absolute form of a displayed path, as stored in the history.
pub fn absolute(path: &str) -> Option<String> {
    let p = Path::new(path).canonicalize().ok()?;
    Some(strip_verbatim(&p).to_string_lossy().into_owned())
}

/// `canonicalize` on Windows returns `\\?\C:\...`; store the plain form.
fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    #[test]
    fn queries_are_deduplicated_and_capped() {
        let mut h = History::default();
        h.add_query("a");
        h.add_query("b");
        h.add_query(" a ");
        h.add_query("   ");
        assert_eq!(h.queries(), ["b", "a"]);
        for i in 0..MAX_QUERIES + 10 {
            h.add_query(&format!("q{i}"));
        }
        assert_eq!(h.queries().len(), MAX_QUERIES);
        assert_eq!(h.queries().last().unwrap(), &format!("q{}", MAX_QUERIES + 9));
    }

    #[test]
    fn round_trip() {
        let mut h = History::default();
        h.add_query("main !test");
        h.record_pick("/p/a.rs", 100);
        h.record_pick("/p/a.rs", 200);
        h.record_pick("/p/b c.rs", 50);
        let text = h.serialize();
        assert_eq!(text, "q\tmain !test\nf\t2\t200\t/p/a.rs\nf\t1\t50\t/p/b c.rs\n");
        assert_eq!(History::parse(&text), h);
    }

    #[test]
    fn parse_ignores_garbage() {
        let h = History::parse("junk\nf\tx\t1\t/a\nf\t1\nq\tok\n\n");
        assert_eq!(h.queries(), ["ok"]);
        assert!(h.picks.is_empty());
    }

    #[test]
    fn frecency_prefers_frequent_and_recent() {
        let mut h = History::default();
        let now = 100_000_000;
        h.record_pick("/recent", now - 60);
        h.record_pick("/old", now - 20 * 86_400);
        for _ in 0..3 {
            h.record_pick("/often", now - 2 * 86_400);
        }
        h.record_pick("/ancient", now - 400 * 86_400);
        assert_eq!(h.frecency("/recent", now), 8);
        assert_eq!(h.frecency("/old", now), 1);
        assert_eq!(h.frecency("/often", now), 6);
        assert_eq!(h.frecency("/ancient", now), 0);
        assert_eq!(h.frecency("/never", now), 0);
        for _ in 0..100 {
            h.record_pick("/hot", now);
        }
        assert_eq!(h.frecency("/hot", now), MAX_BONUS);
    }

    #[test]
    fn save_and_load_from_disk() {
        let t = TempDir::new("history");
        let path = t.path().join("sub").join("history");
        let mut h = History::load(Some(path.clone()));
        assert!(h.queries().is_empty());
        h.add_query("x");
        h.save().unwrap();
        assert_eq!(History::load(Some(path)).queries(), ["x"]);
        History::default().save().unwrap(); // no path: nothing to do
    }

    #[test]
    fn boosts_map_absolute_picks_to_displayed_paths() {
        let t = TempDir::new("history-boost");
        let file = t.write("src/a.rs", "");
        t.write("b.rs", "");
        let mut h = History::default();
        let now = 1_000;
        h.record_pick(&absolute(&file.to_string_lossy()).unwrap(), now);
        h.record_pick("/somewhere/else.rs", now);

        let root = t.path();
        let boosts = h.boosts_under(root, now);
        let shown = root.join("src").join("a.rs").to_string_lossy().into_owned();
        assert_eq!(boosts, HashMap::from([(shown, 8)]));
    }

    #[test]
    fn verbatim_prefix_is_removed() {
        assert_eq!(strip_verbatim(Path::new(r"\\?\C:\x")), PathBuf::from(r"C:\x"));
        assert_eq!(strip_verbatim(Path::new("/usr/x")), PathBuf::from("/usr/x"));
    }
}
