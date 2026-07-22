//! JSON analysis cache keyed by content hash.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::decode::AudioBuffer;
use crate::{Error, Result, TrackAnalysis};

/// Cache format version; bump when the analyzer changes incompatibly.
pub const CACHE_VERSION: u32 = 4;

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
