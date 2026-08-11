//! Locating the real-audio test set.
//!
//! The masters are third-party releases: they are not in the repository, and
//! the copies under `testdata/` are gitignored. Tests that need them are
//! written to skip when they are absent, so the set can live anywhere — a
//! working copy next to the repo, or a mounted music library.
//!
//! Resolution order for [`dir`]: `FUNKOT_TESTDATA_DIR` → `<repo>/testdata`.
//!
//! Lookup is extension-agnostic ([`track`]). The same master is FLAC in a
//! local `testdata/` and ALAC (`.m4a`) in the library, and both are lossless,
//! so either satisfies these tests. Note that they are *not* interchangeable
//! as cache or label keys: [`crate::cache::content_hash`] hashes file bytes,
//! so re-pointing this at a different container invalidates `labels.tsv` and
//! every `funkot-cache` entry.

use std::path::PathBuf;

/// Container extensions probed by [`track`], in order.
const EXTENSIONS: [&str; 3] = ["flac", "m4a", "alac"];

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
/// Returns `None` when the set is not present, which is the signal for the
/// optional regressions to skip rather than fail.
pub fn track(name: &str) -> Option<PathBuf> {
    let dir = dir();
    let stem = strip_container_extension(name);
    EXTENSIONS
        .iter()
        .map(|ext| dir.join(format!("{stem}.{ext}")))
        .find(|path| path.is_file())
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
}
