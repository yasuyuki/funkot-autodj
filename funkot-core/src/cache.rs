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

/// Hash the file, try load, else analyze `buffer` and store.
pub fn get_or_analyze(
    path: &Path,
    cache_dir: &Path,
    buffer: &AudioBuffer,
) -> Result<TrackAnalysis> {
    let hash = content_hash(path)?;
    if let Some(cached) = load(cache_dir, &hash) {
        return Ok(cached);
    }
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    let analysis = crate::analysis::analyze(buffer, file_name)?;
    store(cache_dir, &hash, &analysis)?;
    Ok(analysis)
}

/// Cache hit, else provisional markers — never runs the analyzer.
///
/// Returns `(analysis, used_provisional)`. Used so the first live track can
/// start without waiting on analysis; subsequent prepares / offline render
/// still use [`get_or_analyze`].
pub fn get_cached_or_provisional(
    path: &Path,
    cache_dir: &Path,
    buffer: &AudioBuffer,
) -> Result<(TrackAnalysis, bool)> {
    let hash = content_hash(path)?;
    if let Some(cached) = load(cache_dir, &hash) {
        return Ok((cached, false));
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
        rms_dbfs: TARGET_RMS_DBFS,
        gain_db: 0.0,
    }
}
