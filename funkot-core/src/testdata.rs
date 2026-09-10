//! Locating the real-audio test set.
//!
//! The masters are third-party releases: they are not in the repository, and
//! the copies under `testdata/` are gitignored. Tests that need them are
//! written to skip when they are absent, so the set can live anywhere — a
//! working copy next to the repo, or a mounted music library.
//!
//! Resolution order for [`dir`]: `FUNKOT_TESTDATA_DIR` → `<repo>/testdata`.
//!
//! Lookup is extension-agnostic and directory-shape-agnostic ([`track`]): a
//! flat working copy and a music library filed by album both work. The same
//! master is FLAC in a local `testdata/` and ALAC (`.m4a`) in the library, and
//! both are lossless, so either satisfies these tests. Note that they are *not*
//! interchangeable as cache or label keys: [`crate::cache::content_hash`] hashes
//! file bytes, so re-pointing this at a different container invalidates
//! `labels.tsv` and every `funkot-cache` entry.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Container extensions probed by [`track`], in order.
const EXTENSIONS: [&str; 3] = ["flac", "m4a", "alac"];

/// Depth limit for the recursive scan. The library nests one or two levels
/// (label / album / track); this is slack, not a real constraint. It exists so
/// pointing `FUNKOT_TESTDATA_DIR` at a wrong, huge root fails fast.
const MAX_SCAN_DEPTH: usize = 4;

/// Directory holding the real-audio test set. May not exist.
pub fn dir() -> PathBuf {
    if let Ok(dir) = std::env::var("FUNKOT_TESTDATA_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    local_dir()
}

/// Locate one track by name, with or without a container extension.
///
/// Probes [`dir`] directly first, then falls back to a recursive scan of it —
/// a music library files tracks under per-album directories, so the direct
/// probe misses there. The scan runs once per process and is memoized; over a
/// mounted share it is the difference between one directory walk and one per
/// lookup.
///
/// Returns `None` when the set is not present, which is the signal for the
/// optional regressions to skip rather than fail.
pub fn track(name: &str) -> Option<PathBuf> {
    let dir = dir();
    let stem = strip_container_extension(name);

    if let Some(path) = EXTENSIONS
        .iter()
        .map(|ext| dir.join(format!("{stem}.{ext}")))
        .find(|path| path.is_file())
    {
        return Some(path);
    }

    let index = index();
    EXTENSIONS
        .iter()
        .find_map(|ext| index.get(&format!("{stem}.{ext}")))
        .cloned()
}

/// File name → path for every audio file under [`dir`], built on first use.
///
/// Later duplicates lose: with two copies of a master in the library, the one
/// found first wins and stays the answer for the whole process, so a run cannot
/// silently key half its work to one copy and half to the other.
fn index() -> &'static HashMap<String, PathBuf> {
    static INDEX: OnceLock<HashMap<String, PathBuf>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut out = HashMap::new();
        scan(&dir(), 0, &mut out);
        out
    })
}

fn scan(dir: &Path, depth: usize, out: &mut HashMap<String, PathBuf>) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            scan(&path, depth + 1, out);
            continue;
        }
        let is_audio = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
        if !is_audio {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            out.entry(name.to_string()).or_insert(path);
        }
    }
}

/// This checkout's own `testdata/`, for anything a test *writes*: analysis
/// caches, clips kept for listening.
///
/// Deliberately not [`dir`]: a music library is often a read-only mount, and
/// derived data belongs to the checkout that produced it. Same reasoning as
/// [`crate::convert::cache_dir`].
pub fn local_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("testdata")
}

/// Drop a trailing container extension, and only that.
///
/// Track names carry dots of their own ("03. KazuyaP - Monitoring Db"), so
/// splitting on the last dot unconditionally would eat part of the title.
fn strip_container_extension(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, ext)) if EXTENSIONS.contains(&ext) => stem,
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_only_known_container_extensions() {
        assert_eq!(strip_container_extension("IVY.flac"), "IVY");
        assert_eq!(strip_container_extension("IVY.m4a"), "IVY");
        assert_eq!(strip_container_extension("IVY"), "IVY");
        // A dotted title must survive intact, both with and without a container.
        assert_eq!(
            strip_container_extension("03. KazuyaP - Monitoring Db"),
            "03. KazuyaP - Monitoring Db"
        );
        assert_eq!(
            strip_container_extension("03. KazuyaP - Monitoring Db.flac"),
            "03. KazuyaP - Monitoring Db"
        );
        // Not a container we probe: leave it alone rather than guess.
        assert_eq!(strip_container_extension("labels.tsv"), "labels.tsv");
    }

    #[test]
    fn missing_track_is_none_not_a_panic() {
        assert!(track("no such track at all").is_none());
    }

    #[test]
    fn scan_finds_tracks_filed_under_album_directories() {
        let root = std::env::temp_dir().join(format!("funkot-testdata-scan-{}", std::process::id()));
        let album = root.join("Some Label").join("Some Album");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("03. Artist - Title.m4a"), b"").unwrap();
        std::fs::write(album.join("cover.jpg"), b"").unwrap();
        // Deeper than MAX_SCAN_DEPTH: must not be indexed.
        let deep = root.join("a").join("b").join("c").join("d").join("e");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("Too Deep.m4a"), b"").unwrap();

        let mut index = HashMap::new();
        scan(&root, 0, &mut index);
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!(
            index.get("03. Artist - Title.m4a"),
            Some(&album.join("03. Artist - Title.m4a"))
        );
        assert!(!index.contains_key("cover.jpg"));
        assert!(!index.contains_key("Too Deep.m4a"));
    }
}
