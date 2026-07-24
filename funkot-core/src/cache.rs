//! JSON analysis cache keyed by content hash.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::decode::AudioBuffer;
use crate::{
    Error, Result, TrackAnalysis, BEATS_PER_BAR, FALLBACK_BARS, NOMINAL_BPM, TARGET_RMS_DBFS,
};

/// Cache format version; bump when the analyzer changes incompatibly.
pub const CACHE_VERSION: u32 = 8;

const HASH_CHUNK: u64 = 64 * 1024;

/// Content hash of a file: SHA-256 over (file length as LE bytes + first 64 KiB + last 64 KiB), hex.
pub fn content_hash(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .map_err(|e| Error::Cache(format!("cannot open '{}' for hashing: {e}", path.display())))?;
    let len = file
        .metadata()
        .map_err(|e| Error::Cache(format!("cannot stat '{}': {e}", path.display())))?
        .len();

    let mut hasher = Sha256::new();
    hasher.update(len.to_le_bytes());

    let first_len = HASH_CHUNK.min(len) as usize;
    let mut buf = vec![0u8; HASH_CHUNK as usize];
    if first_len > 0 {
        file.read_exact(&mut buf[..first_len])
            .map_err(|e| Error::Cache(format!("cannot read start of '{}': {e}", path.display())))?;
        hasher.update(&buf[..first_len]);
    }

    if len > HASH_CHUNK {
        let last_len = HASH_CHUNK.min(len) as usize;
        // When len <= 128 KiB the last window overlaps the first; still hash as specified.
        let start = len.saturating_sub(HASH_CHUNK);
        file.seek(SeekFrom::Start(start))
            .map_err(|e| Error::Cache(format!("cannot seek in '{}': {e}", path.display())))?;
        file.read_exact(&mut buf[..last_len])
            .map_err(|e| Error::Cache(format!("cannot read end of '{}': {e}", path.display())))?;
        hasher.update(&buf[..last_len]);
    }

    Ok(hex_encode(&hasher.finalize()))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

fn cache_path(cache_dir: &Path, hash: &str) -> std::path::PathBuf {
    cache_dir.join(format!("{hash}.json"))
}

/// Load a cached analysis. `None` if missing, unreadable, corrupt JSON, or version mismatch.
pub fn load(cache_dir: &Path, hash: &str) -> Option<TrackAnalysis> {
    let path = cache_path(cache_dir, hash);
    let data = fs::read_to_string(&path).ok()?;
    let analysis: TrackAnalysis = serde_json::from_str(&data).ok()?;
    if analysis.version != CACHE_VERSION {
        return None;
    }
    Some(analysis)
}

/// Store analysis as pretty JSON. Creates `cache_dir` if needed.
pub fn store(cache_dir: &Path, hash: &str, analysis: &TrackAnalysis) -> Result<()> {
    fs::create_dir_all(cache_dir).map_err(|e| {
        Error::Cache(format!(
            "cannot create cache dir '{}': {e}",
            cache_dir.display()
        ))
    })?;
    let path = cache_path(cache_dir, hash);
    let json = serde_json::to_string_pretty(analysis)
        .map_err(|e| Error::Cache(format!("serialize analysis: {e}")))?;
    fs::write(&path, json)
        .map_err(|e| Error::Cache(format!("cannot write cache '{}': {e}", path.display())))?;
    Ok(())
}

/// Counts from [`purge_auto`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PurgeStats {
    pub deleted: usize,
    pub cleared: usize,
    pub skipped: usize,
}

/// Delete cache JSON with no manual intro/outro flags; strip auto fields from the rest.
///
/// Kept entries retain manual `intro_bars` / `outro_bars` and set `needs_reanalysis`
/// so the next [`get_or_analyze`] recomputes everything else.
pub fn purge_auto(cache_dir: &Path) -> Result<PurgeStats> {
    let mut stats = PurgeStats::default();
    if !cache_dir.is_dir() {
        return Ok(stats);
    }
    let entries = fs::read_dir(cache_dir).map_err(|e| {
        Error::Cache(format!(
            "cannot read cache dir '{}': {e}",
            cache_dir.display()
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::Cache(format!("cache dir entry: {e}")))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            stats.skipped += 1;
            continue;
        }
        let data = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => {
                stats.skipped += 1;
                continue;
            }
        };
        let mut analysis: TrackAnalysis = match serde_json::from_str(&data) {
            Ok(a) => a,
            Err(_) => {
                stats.skipped += 1;
                continue;
            }
        };
        if analysis.version != CACHE_VERSION {
            stats.skipped += 1;
            continue;
        }
        if !analysis.intro_bars_manual && !analysis.outro_bars_manual {
            fs::remove_file(&path).map_err(|e| {
                Error::Cache(format!("cannot delete cache '{}': {e}", path.display()))
            })?;
            stats.deleted += 1;
            continue;
        }
        strip_auto_fields(&mut analysis);
        let json = serde_json::to_string_pretty(&analysis)
            .map_err(|e| Error::Cache(format!("serialize analysis: {e}")))?;
        fs::write(&path, json)
            .map_err(|e| Error::Cache(format!("cannot write cache '{}': {e}", path.display())))?;
        stats.cleared += 1;
    }
    Ok(stats)
}

/// Keep only manually protected bar counts; mark for reanalysis.
fn strip_auto_fields(a: &mut TrackAnalysis) {
    let intro_bars = if a.intro_bars_manual { a.intro_bars } else { 0 };
    let outro_bars = if a.outro_bars_manual { a.outro_bars } else { 0 };
    let intro_m = a.intro_bars_manual;
    let outro_m = a.outro_bars_manual;
    *a = TrackAnalysis {
        version: CACHE_VERSION,
        file_name: a.file_name.clone(),
        sample_rate: a.sample_rate,
        total_frames: a.total_frames,
        intro_bpm: 0.0,
        outro_bpm: 0.0,
        first_downbeat: 0,
        outro_start: 0,
        intro_bars,
        outro_bars,
        bars_estimated_low_confidence: true,
        intro_bars_low_confidence: !intro_m,
        outro_bars_low_confidence: !outro_m,
        intro_bars_manual: intro_m,
        outro_bars_manual: outro_m,
        needs_reanalysis: true,
        rms_dbfs: TARGET_RMS_DBFS,
        gain_db: 0.0,
    };
}

/// Re-apply hand-edited intro/outro bars onto a fresh analysis.
pub fn apply_manual_overrides(manual: &TrackAnalysis, mut fresh: TrackAnalysis) -> TrackAnalysis {
    if manual.intro_bars_manual {
        fresh.intro_bars = manual.intro_bars;
        fresh.intro_bars_manual = true;
        fresh.intro_bars_low_confidence = false;
    }
    if manual.outro_bars_manual {
        fresh.outro_bars = manual.outro_bars;
        fresh.outro_bars_manual = true;
        fresh.outro_bars_low_confidence = false;
        let bar_len = (60.0 / fresh.outro_bpm * f64::from(fresh.sample_rate) * f64::from(BEATS_PER_BAR))
            .round()
            .max(1.0) as u64;
        fresh.outro_start = fresh
            .total_frames
            .saturating_sub(u64::from(fresh.outro_bars) * bar_len);
    }
    fresh.bars_estimated_low_confidence =
        fresh.intro_bars_low_confidence || fresh.outro_bars_low_confidence;
    fresh.needs_reanalysis = false;
    fresh
}

/// Hash the file, try load, else analyze `buffer` and store.
///
/// Complete caches are returned as-is. Incomplete caches (`needs_reanalysis`)
/// and misses are (re)analyzed; manual bar flags from an incomplete cache are kept.
pub fn get_or_analyze(
    path: &Path,
    cache_dir: &Path,
    buffer: &AudioBuffer,
) -> Result<TrackAnalysis> {
    let hash = content_hash(path)?;
    let prior = load(cache_dir, &hash);
    if let Some(cached) = prior.as_ref() {
        if !cached.needs_reanalysis {
            return Ok(cached.clone());
        }
    }
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    let mut analysis = crate::analysis::analyze(buffer, file_name)?;
    if let Some(cached) = prior.as_ref() {
        if cached.intro_bars_manual || cached.outro_bars_manual {
            analysis = apply_manual_overrides(cached, analysis);
        }
    }
    store(cache_dir, &hash, &analysis)?;
    Ok(analysis)
}

/// Hash the file, try load, else analyze `buffer` and store.
///
/// Same as [`get_or_analyze`], but returns whether a fresh analysis ran
/// (`true` = analyzed or reanalyzed; `false` = complete cache hit).
pub fn fill_missing(
    path: &Path,
    cache_dir: &Path,
    buffer: &AudioBuffer,
) -> Result<(TrackAnalysis, bool)> {
    let hash = content_hash(path)?;
    if let Some(cached) = load(cache_dir, &hash) {
        if !cached.needs_reanalysis {
            return Ok((cached, false));
        }
    }
    let analysis = get_or_analyze(path, cache_dir, buffer)?;
    Ok((analysis, true))
}

/// Cache hit, else provisional markers — never runs the analyzer.
///
/// Returns `(analysis, used_provisional)`. Used so the first live track can
/// start without waiting on analysis; subsequent prepares / offline render
/// still use [`get_or_analyze`]. Incomplete caches count as a miss.
pub fn get_cached_or_provisional(
    path: &Path,
    cache_dir: &Path,
    buffer: &AudioBuffer,
) -> Result<(TrackAnalysis, bool)> {
    let hash = content_hash(path)?;
    if let Some(cached) = load(cache_dir, &hash) {
        if !cached.needs_reanalysis {
            return Ok((cached, false));
        }
    }
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    Ok((provisional(buffer, file_name), true))
}

/// Nominal-BPM / [`FALLBACK_BARS`] stand-in until a real analysis is cached.
pub fn provisional(buffer: &AudioBuffer, file_name: &str) -> TrackAnalysis {
    let sr = f64::from(buffer.sample_rate);
    let bar_len = (60.0 / NOMINAL_BPM * sr * f64::from(BEATS_PER_BAR))
        .round()
        .max(1.0) as u64;
    let total_bars = (buffer.frames / bar_len) as u32;
    // Don't claim a 64-bar outro on a shorter file (would put outro_start at 0).
    let section_bars = FALLBACK_BARS.min(total_bars / 3).max(1);
    let outro_start = buffer
        .frames
        .saturating_sub(u64::from(section_bars) * bar_len);
    TrackAnalysis {
        version: CACHE_VERSION,
        file_name: file_name.to_string(),
        sample_rate: buffer.sample_rate,
        total_frames: buffer.frames,
        intro_bpm: NOMINAL_BPM,
        outro_bpm: NOMINAL_BPM,
        first_downbeat: 0,
        outro_start,
        intro_bars: section_bars,
        outro_bars: section_bars,
        bars_estimated_low_confidence: true,
        intro_bars_low_confidence: true,
        outro_bars_low_confidence: true,
        intro_bars_manual: false,
        outro_bars_manual: false,
        needs_reanalysis: false,
        rms_dbfs: TARGET_RMS_DBFS,
        gain_db: 0.0,
    }
}
