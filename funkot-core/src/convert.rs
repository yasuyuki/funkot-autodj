//! ffmpeg-based fallback conversion for formats symphonia cannot decode
//! (Opus, WMA, AIFF, APE, …).
//!
//! The converted FLAC produced here is a disposable *derived* cache: the
//! track's identity ([`crate::cache::content_hash`], and therefore
//! `labels.tsv` / `funkot-cache` keys) is always computed from the original
//! source file, never from the converted one. Deleting everything under
//! [`cache_dir`] and re-running is always safe — it just costs another
//! ffmpeg pass.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Error, Result};

/// Directory holding converted FLAC files.
///
/// Resolution order: `FUNKOT_CONVERT_DIR` env var →
/// `$XDG_CACHE_HOME/funkot-autodj/converted` → `$HOME/.cache/funkot-autodj/converted`
/// → `std::env::temp_dir()/funkot-autodj-converted`.
///
/// Deliberately never next to the source file: music libraries are
/// sometimes mounted read-only.
pub fn cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("FUNKOT_CONVERT_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg)
                .join("funkot-autodj")
                .join("converted");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home)
                .join(".cache")
                .join("funkot-autodj")
                .join("converted");
        }
    }
    std::env::temp_dir().join("funkot-autodj-converted")
}

/// The path `source` would be converted to inside `dir`. May not exist yet.
pub fn converted_path(dir: &Path, source: &Path) -> Result<PathBuf> {
    let hash = crate::cache::content_hash(source)?;
    Ok(dir.join(format!("{hash}.flac")))
}

/// Convert `source` to FLAC via ffmpeg, reusing an existing conversion if present.
///
/// Returns the path to a converted FLAC file; never mutates `source`.
pub fn to_flac_cached(source: &Path) -> Result<PathBuf> {
    let dir = cache_dir();
    let target = converted_path(&dir, source)?;

    if let Ok(meta) = std::fs::metadata(&target) {
        if meta.len() > 0 {
            return Ok(target);
        }
    }

    std::fs::create_dir_all(&dir).map_err(|e| {
        Error::Decode(format!(
            "cannot create conversion cache dir '{}': {e}",
            dir.display()
        ))
    })?;

    let tmp = dir.join(format!(
        "{}.flac.tmp{}",
        target
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("convert"),
        std::process::id()
    ));
    // Clear any stale leftover from a previous crashed/killed run.
    let _ = std::fs::remove_file(&tmp);

    // -map 0:a:0: audio track only. Some files (e.g. FLAC/APE with an
    // embedded cover image) carry an attached-picture video stream; letting
    // ffmpeg mux that into the FLAC output breaks the conversion (observed:
    // "Could not find tag for codec h264" on a FLAC with an h264 cover
    // image). We don't need the art, so it's dropped deliberately.
    // -map_metadata 0: keep the source's tags on the converted file.
    // -f flac: the tmp file's name ends in `.tmp<pid>`, not `.flac` (so a
    // stale one is unambiguously a leftover and not a real converted file);
    // ffmpeg can't infer the container from that extension, so it's forced
    // explicitly.
    let run = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(source)
        .args(["-map", "0:a:0", "-map_metadata", "0", "-c:a", "flac", "-f", "flac"])
        .arg(&tmp)
        .output();

    let output = match run {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::UnsupportedFormat(format!(
                "'{}' needs ffmpeg to decode, but ffmpeg was not found on PATH; \
                 install ffmpeg to read this format",
                source.display()
            )));
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::Decode(format!(
                "failed to run ffmpeg for '{}': {e}",
                source.display()
            )));
        }
    };

    if !output.status.success() {
        let _ = std::fs::remove_file(&tmp);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(5).collect();
        let tail: String = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
        return Err(Error::Decode(format!(
            "ffmpeg failed converting '{}' to FLAC ({}): {tail}",
            source.display(),
            output.status
        )));
    }

    // Write-then-rename inside the same directory: a crash or a concurrent
    // conversion of the same file must never leave a partial FLAC sitting at
    // `target`, or a later run would silently decode a broken file.
    std::fs::rename(&tmp, &target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::Decode(format!(
            "cannot finalize converted file '{}': {e}",
            target.display()
        ))
    })?;

    Ok(target)
}

/// `true` if `ffmpeg` can be spawned on PATH.
#[cfg(test)]
fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    /// Serializes tests that touch `FUNKOT_CONVERT_DIR` / `XDG_CACHE_HOME` /
    /// `HOME`, since `#[test]`s run on multiple threads within one process.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "funkot-convert-test-{tag}-{}-{}-{n}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_file(path: &Path, contents: &[u8]) {
        let mut f = std::fs::File::create(path).expect("create temp file");
        f.write_all(contents).expect("write temp file");
    }

    #[test]
    fn converted_path_is_stable_and_content_addressed() {
        let scratch = TempDir::new("naming");
        let dir = TempDir::new("naming-dir");

        let a = scratch.path().join("a.bin");
        let b = scratch.path().join("b.bin");
        write_file(&a, b"same content");
        write_file(&b, b"same content");

        let path_a1 = converted_path(dir.path(), &a).expect("hash a");
        let path_a2 = converted_path(dir.path(), &a).expect("hash a again");
        let path_b = converted_path(dir.path(), &b).expect("hash b");

        assert_eq!(path_a1, path_a2, "same source hashes to the same path");
        assert_eq!(
            path_a1, path_b,
            "identical content hashes to the same path regardless of file name"
        );

        let c = scratch.path().join("c.bin");
        write_file(&c, b"different content");
        let path_c = converted_path(dir.path(), &c).expect("hash c");
        assert_ne!(path_a1, path_c, "different content hashes differently");
    }

    #[test]
    fn to_flac_cached_reuses_an_existing_conversion_without_running_ffmpeg() {
        let _guard = ENV_LOCK.lock().unwrap();
        let scratch = TempDir::new("reuse-src");
        let dir = TempDir::new("reuse-dir");

        std::env::set_var("FUNKOT_CONVERT_DIR", dir.path());

        let source = scratch.path().join("source.bin");
        write_file(&source, b"arbitrary bytes, not real audio");

        let expected_target = converted_path(dir.path(), &source).expect("compute target path");
        let placeholder = b"placeholder content, not a real FLAC";
        write_file(&expected_target, placeholder);

        let result = to_flac_cached(&source).expect("reuse existing conversion");
        assert_eq!(result, expected_target);

        // Untouched: if ffmpeg had actually run, it would have overwritten
        // (or failed to overwrite and errored) this placeholder.
        let contents = std::fs::read(&expected_target).expect("read target");
        assert_eq!(contents, placeholder);

        std::env::remove_var("FUNKOT_CONVERT_DIR");
    }

    #[test]
    fn missing_ffmpeg_error_mentions_ffmpeg() {
        if ffmpeg_available() {
            eprintln!("skip: ffmpeg is present on PATH, this test only applies without it");
            return;
        }
        let _guard = ENV_LOCK.lock().unwrap();
        let scratch = TempDir::new("noffmpeg-src");
        let dir = TempDir::new("noffmpeg-dir");
        std::env::set_var("FUNKOT_CONVERT_DIR", dir.path());

        let source = scratch.path().join("source.bin");
        write_file(&source, b"arbitrary bytes, not real audio");

        let err = to_flac_cached(&source).expect_err("ffmpeg is not on PATH");
        let msg = err.to_string();
        assert!(
            msg.to_ascii_lowercase().contains("ffmpeg"),
            "error should mention ffmpeg: {msg}"
        );

        std::env::remove_var("FUNKOT_CONVERT_DIR");
    }

    #[test]
    fn ffmpeg_round_trip_decodes_an_unsupported_format() {
        if !ffmpeg_available() {
            eprintln!("skip: ffmpeg is not on PATH");
            return;
        }
        let _guard = ENV_LOCK.lock().unwrap();
        let scratch = TempDir::new("roundtrip-src");
        let dir = TempDir::new("roundtrip-dir");
        std::env::set_var("FUNKOT_CONVERT_DIR", dir.path());

        let wma = scratch.path().join("in.wma");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=3",
                "-c:a",
                "wmav2",
                "-metadata",
                "title=Funkot Convert Test",
            ])
            .arg(&wma)
            .status()
            .expect("spawn ffmpeg to build test fixture");
        assert!(status.success(), "failed to build test .wma fixture");

        let buf = crate::decode::decode_file(&wma).expect("decode via ffmpeg fallback");
        let expected_frames = 3 * buf.sample_rate as u64;
        let diff = buf.frames.abs_diff(expected_frames);
        assert!(
            diff < buf.sample_rate as u64 / 2,
            "frames {} not close to {} (3s at {} Hz)",
            buf.frames,
            expected_frames,
            buf.sample_rate
        );

        let cached = converted_path(dir.path(), &wma).expect("compute cached path");
        assert!(cached.is_file(), "converted FLAC should be cached on disk");

        std::env::remove_var("FUNKOT_CONVERT_DIR");
    }

    #[test]
    fn ffmpeg_conversion_preserves_metadata_tags() {
        if !ffmpeg_available() {
            eprintln!("skip: ffmpeg is not on PATH");
            return;
        }
        let _guard = ENV_LOCK.lock().unwrap();
        let scratch = TempDir::new("meta-src");
        let dir = TempDir::new("meta-dir");
        std::env::set_var("FUNKOT_CONVERT_DIR", dir.path());

        let wma = scratch.path().join("in.wma");
        let title = "Funkot Convert Test Title";
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=3",
                "-c:a",
                "wmav2",
                "-metadata",
            ])
            .arg(format!("title={title}"))
            .arg(&wma)
            .status()
            .expect("spawn ffmpeg to build test fixture");
        assert!(status.success(), "failed to build test .wma fixture");

        let converted = to_flac_cached(&wma).expect("convert to flac");

        // Read tags with symphonia, not ffprobe (this environment's ffmpeg
        // is a static build without ffprobe).
        let file = std::fs::File::open(&converted).expect("open converted flac");
        let mss = symphonia::core::io::MediaSourceStream::new(
            Box::new(file),
            Default::default(),
        );
        let mut hint = symphonia::core::formats::probe::Hint::new();
        hint.with_extension("flac");
        let mut format = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                symphonia::core::formats::FormatOptions::default(),
                symphonia::core::meta::MetadataOptions::default(),
            )
            .expect("probe converted flac");

        fn title_in(rev: &symphonia::core::meta::MetadataRevision) -> Option<String> {
            for tag in &rev.media.tags {
                if let Some(symphonia::core::meta::StandardTag::TrackTitle(v)) = &tag.std {
                    return Some(v.to_string());
                }
            }
            // Fall back to the raw key in case the FLAC reader doesn't map
            // this vorbis comment to a standard tag.
            for tag in &rev.media.tags {
                if tag.raw.key.eq_ignore_ascii_case("title") {
                    return Some(tag.raw.value.to_string());
                }
            }
            None
        }

        let mut metadata = format.metadata();
        let mut found_title = metadata.current().and_then(title_in);
        if found_title.is_none() {
            found_title = metadata.skip_to_latest().and_then(title_in);
        }

        std::env::remove_var("FUNKOT_CONVERT_DIR");

        assert_eq!(
            found_title.as_deref(),
            Some(title),
            "title tag should survive the ffmpeg conversion"
        );
    }
}
