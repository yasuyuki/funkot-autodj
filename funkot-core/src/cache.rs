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
///
/// 10: `outro_bars` is now `outro_structure_bars + OUTRO_LEAD_BARS` on every
/// track (`analysis::outro_trigger_bars`). Version 9 entries carry triggers
/// from the old conditional rule, which collapsed onto the structural
/// boundary on tracks whose outro starts 64 bars from the end — the
/// transition ran inside the outro there.
/// 14: head/tail classify scores (`ClassifyScores`: z / z_ratio / half_ratio)
/// are stored on `TrackAnalysis` so threshold retunes need no re-decode.
pub const CACHE_VERSION: u32 = 14;

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
        if !analysis.intro_bars_manual
            && !analysis.outro_bars_manual
            && !analysis.outro_structure_bars_manual
        {
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
    let structure_m = a.outro_structure_bars_manual;
    let outro_structure_bars = if structure_m { a.outro_structure_bars } else { 0 };
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
        // Auto: recomputed from the fresh boundary next load.
        track_bars: 0,
        outro_bars,
        // Stripped alongside the other auto fields unless hand-set;
        // needs_reanalysis pulls in a fresh value (or a fresh
        // FALLBACK_BARS-based one) next load.
        outro_structure_bars,
        bars_estimated_low_confidence: true,
        intro_bars_low_confidence: !intro_m,
        outro_bars_low_confidence: !(outro_m || structure_m),
        intro_bars_manual: intro_m,
        outro_bars_manual: outro_m,
        outro_structure_bars_manual: structure_m,
        needs_reanalysis: true,
        // Placeholder until reanalysis; overwritten by `analyze`.
        is_funkot: true,
        classify_scores: None,
        rms_dbfs: TARGET_RMS_DBFS,
        gain_db: 0.0,
    };
}

/// Length of one bar in frames at the outro tempo.
fn outro_bar_len(a: &TrackAnalysis) -> u64 {
    (60.0 / a.outro_bpm * f64::from(a.sample_rate) * f64::from(BEATS_PER_BAR))
        .round()
        .max(1.0) as u64
}

/// Recompute `outro_start` from `outro_bars`, `outro_bpm`, `sample_rate` and `total_frames`.
fn recompute_outro_start(a: &mut TrackAnalysis) {
    let bar_len = outro_bar_len(a);
    a.outro_start = a
        .total_frames
        .saturating_sub(u64::from(a.outro_bars) * bar_len);
}

/// `track_bars`, or a stand-in for entries written before it was stored.
///
/// The fallback measures to the end of the *file* rather than the end of the
/// music, so it runs long on tracks with a silent tail. It only feeds the
/// "is the track long enough to hold the lead-in" bound, where being generous
/// just means the plain boundary-plus-lead-in rule applies.
fn track_bars_or_estimate(a: &TrackAnalysis) -> u32 {
    if a.track_bars > 0 {
        return a.track_bars;
    }
    (a.total_frames.saturating_sub(a.first_downbeat) / outro_bar_len(a)) as u32
}

/// Re-derive the mix trigger (and `outro_start`) from the structural boundary,
/// by the same rule `analysis::analyze` applies.
fn derive_outro_from_structure(a: &mut TrackAnalysis) {
    a.outro_bars = crate::analysis::outro_trigger_bars(
        a.outro_structure_bars,
        a.intro_bars,
        track_bars_or_estimate(a),
    );
    recompute_outro_start(a);
}

/// Keep `outro_structure_bars <= outro_bars` after a hand-edited trigger.
///
/// `analysis::analyze` holds that invariant by construction — the trigger is
/// the boundary plus a lead-in, so it can never sit closer to the file end.
/// A manual `outro_bars` skips that rule entirely, and shortening the outro
/// past the detected boundary would leave the boundary claiming the outro
/// starts *earlier* than the mix trigger the same entry advertises. That is
/// the field `labels`/`eval_sections` compare against hand-labeled ground
/// truth, so the tracks a user cared enough about to correct by hand would be
/// exactly the ones scored against a broken value.
///
/// Only ever lowers: a manual trigger placed further back adds room, and the
/// detected boundary is still the best estimate of where the outro begins.
fn clamp_structure_to_outro(a: &mut TrackAnalysis) {
    a.outro_structure_bars = a.outro_structure_bars.min(a.outro_bars);
}

/// Re-apply hand-edited intro/outro bars onto a fresh analysis.
///
/// A hand-set structural boundary wins over a hand-set trigger: the two say
/// the same thing, the setters keep them exclusive, and the boundary is the
/// one the trigger is derived from.
pub fn apply_manual_overrides(manual: &TrackAnalysis, mut fresh: TrackAnalysis) -> TrackAnalysis {
    if manual.intro_bars_manual {
        fresh.intro_bars = manual.intro_bars;
        fresh.intro_bars_manual = true;
        fresh.intro_bars_low_confidence = false;
    }
    if manual.outro_structure_bars_manual {
        fresh.outro_structure_bars = manual.outro_structure_bars;
        fresh.outro_structure_bars_manual = true;
        fresh.outro_bars_manual = false;
        fresh.outro_bars_low_confidence = false;
        // Derived from the boundary the *user* set, but against this analysis'
        // fresh intro and length — the same inputs `analyze` would have used.
        derive_outro_from_structure(&mut fresh);
    } else if manual.outro_bars_manual {
        fresh.outro_bars = manual.outro_bars;
        fresh.outro_bars_manual = true;
        fresh.outro_bars_low_confidence = false;
        recompute_outro_start(&mut fresh);
        clamp_structure_to_outro(&mut fresh);
    }
    fresh.bars_estimated_low_confidence =
        fresh.intro_bars_low_confidence || fresh.outro_bars_low_confidence;
    fresh.needs_reanalysis = false;
    fresh
}

/// Hand-edit `intro_bars` and/or `outro_bars` on a cached entry and persist it.
///
/// The side left as `None` is untouched, including its `*_manual` flag.
/// `needs_reanalysis` is preserved as-is (unlike [`apply_manual_overrides`],
/// which always clears it): editing bar counts by hand doesn't change
/// whether the rest of the auto-analyzed fields are complete.
pub fn set_manual_bars(
    cache_dir: &Path,
    hash: &str,
    intro: Option<u32>,
    outro: Option<u32>,
) -> Result<TrackAnalysis> {
    let mut analysis = load(cache_dir, hash)
        .ok_or_else(|| Error::Cache(format!("no cache entry for hash '{hash}'")))?;
    if let Some(n) = intro {
        analysis.intro_bars = n;
        analysis.intro_bars_manual = true;
        analysis.intro_bars_low_confidence = false;
    }
    if let Some(n) = outro {
        analysis.outro_bars = n;
        analysis.outro_bars_manual = true;
        analysis.outro_bars_low_confidence = false;
        // Pinning the trigger by hand takes the outro edge out of the
        // boundary's control, so an earlier boundary edit no longer applies.
        analysis.outro_structure_bars_manual = false;
        recompute_outro_start(&mut analysis);
        clamp_structure_to_outro(&mut analysis);
    }
    analysis.bars_estimated_low_confidence =
        analysis.intro_bars_low_confidence || analysis.outro_bars_low_confidence;
    store(cache_dir, hash, &analysis)?;
    Ok(analysis)
}

/// Hand-edit the *structural* outro boundary on a cached entry and persist it,
/// re-deriving `outro_bars` (the mix trigger) and `outro_start` from it.
///
/// This is the edit to expose to listeners: `outro_structure_bars` is where
/// the track actually collapses, which is what someone hears and can judge,
/// while the trigger is bookkeeping the analyzer derives from it — one fixed
/// lead-in ahead, so the transition finishes exactly where the outro begins.
/// Editing the trigger directly ([`set_manual_bars`]) severs that relation;
/// this keeps it.
///
/// Clears `outro_bars_manual`: the trigger is derived again from here on.
/// `needs_reanalysis` is preserved, as in [`set_manual_bars`].
pub fn set_manual_structure_bars(
    cache_dir: &Path,
    hash: &str,
    bars: u32,
) -> Result<TrackAnalysis> {
    let mut analysis = load(cache_dir, hash)
        .ok_or_else(|| Error::Cache(format!("no cache entry for hash '{hash}'")))?;
    analysis.outro_structure_bars = bars;
    analysis.outro_structure_bars_manual = true;
    analysis.outro_bars_manual = false;
    analysis.outro_bars_low_confidence = false;
    derive_outro_from_structure(&mut analysis);
    analysis.bars_estimated_low_confidence =
        analysis.intro_bars_low_confidence || analysis.outro_bars_low_confidence;
    store(cache_dir, hash, &analysis)?;
    Ok(analysis)
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
        if cached.intro_bars_manual
            || cached.outro_bars_manual
            || cached.outro_structure_bars_manual
        {
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
        track_bars: total_bars,
        outro_bars: section_bars,
        // Naive placeholder, no structural detection ran; mirrors outro_bars
        // like every other field here (all low-confidence by construction).
        outro_structure_bars: section_bars,
        bars_estimated_low_confidence: true,
        intro_bars_low_confidence: true,
        outro_bars_low_confidence: true,
        intro_bars_manual: false,
        outro_bars_manual: false,
        outro_structure_bars_manual: false,
        needs_reanalysis: false,
        is_funkot: true,
        classify_scores: None,
        rms_dbfs: TARGET_RMS_DBFS,
        gain_db: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn sample_analysis() -> TrackAnalysis {
        TrackAnalysis {
            version: CACHE_VERSION,
            file_name: "test.wav".to_string(),
            sample_rate: 44_100,
            total_frames: 10_000_000,
            intro_bpm: 150.0,
            outro_bpm: 150.0,
            first_downbeat: 0,
            outro_start: 0,
            intro_bars: 8,
            track_bars: 200,
            outro_bars: 16,
            outro_structure_bars: 16,
            bars_estimated_low_confidence: true,
            intro_bars_low_confidence: true,
            outro_bars_low_confidence: true,
            intro_bars_manual: false,
            outro_bars_manual: false,
            outro_structure_bars_manual: false,
            needs_reanalysis: false,
            is_funkot: true,
            classify_scores: None,
            rms_dbfs: TARGET_RMS_DBFS,
            gain_db: 0.0,
        }
    }

    /// Process-unique scratch dir under the system temp dir, cleaned up on drop.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "funkot-cache-test-{tag}-{}-{}-{n}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn set_manual_bars_outro_recomputes_outro_start() {
        let dir = TempDir::new("outro");
        let hash = "hash-outro";
        let analysis = sample_analysis();
        store(dir.path(), hash, &analysis).unwrap();

        let result = set_manual_bars(dir.path(), hash, None, Some(32)).unwrap();

        let bar_len = (60.0 / result.outro_bpm
            * f64::from(result.sample_rate)
            * f64::from(BEATS_PER_BAR))
        .round()
        .max(1.0) as u64;
        let expected_outro_start = result.total_frames - 32 * bar_len;

        assert_eq!(result.outro_bars, 32);
        assert!(result.outro_bars_manual);
        assert!(!result.outro_bars_low_confidence);
        assert_eq!(result.outro_start, expected_outro_start);

        let reloaded = load(dir.path(), hash).unwrap();
        assert_eq!(reloaded.outro_start, expected_outro_start);
        assert_eq!(reloaded.outro_bars, 32);
    }

    #[test]
    fn set_manual_bars_keeps_an_existing_manual_side() {
        let dir = TempDir::new("intro-manual");
        let hash = "hash-intro-manual";
        let mut analysis = sample_analysis();
        analysis.intro_bars = 12;
        analysis.intro_bars_manual = true;
        analysis.intro_bars_low_confidence = false;
        store(dir.path(), hash, &analysis).unwrap();

        let result = set_manual_bars(dir.path(), hash, None, Some(20)).unwrap();

        assert_eq!(result.intro_bars, 12);
        assert!(result.intro_bars_manual);
    }

    /// The dangerous direction: editing one side must not mark the *other* side
    /// manual. A wrongly-set flag pins an auto-detected value forever and is
    /// indistinguishable from real hand-editing afterwards.
    #[test]
    fn set_manual_bars_does_not_mark_the_other_side_manual() {
        let dir = TempDir::new("intro-auto");
        let hash = "hash-intro-auto";
        let analysis = sample_analysis(); // both *_manual are false
        store(dir.path(), hash, &analysis).unwrap();

        let result = set_manual_bars(dir.path(), hash, None, Some(20)).unwrap();
        assert_eq!(result.intro_bars, analysis.intro_bars);
        assert!(!result.intro_bars_manual);
        assert!(result.intro_bars_low_confidence);

        let result = set_manual_bars(dir.path(), hash, Some(48), None).unwrap();
        assert_eq!(result.outro_bars, 20);
        assert!(result.outro_bars_manual, "the earlier outro edit must survive");
        assert!(result.intro_bars_manual);
    }

    /// A hand-edited trigger is the one path that can put the structural
    /// boundary behind `outro_bars`, breaking the invariant `analyze` holds by
    /// construction — and it breaks it on exactly the hand-corrected tracks
    /// `eval_sections` cares most about.
    #[test]
    fn set_manual_bars_outro_clamps_the_structural_boundary() {
        let dir = TempDir::new("outro-clamp");
        let hash = "hash-outro-clamp";
        let mut analysis = sample_analysis();
        analysis.outro_bars = 48;
        analysis.outro_structure_bars = 32;
        store(dir.path(), hash, &analysis).unwrap();

        let result = set_manual_bars(dir.path(), hash, None, Some(24)).unwrap();

        assert_eq!(result.outro_bars, 24);
        assert_eq!(result.outro_structure_bars, 24);
        assert_eq!(load(dir.path(), hash).unwrap().outro_structure_bars, 24);
    }

    #[test]
    fn set_manual_bars_outro_does_not_raise_the_structural_boundary() {
        let dir = TempDir::new("outro-noraise");
        let hash = "hash-outro-noraise";
        let mut analysis = sample_analysis();
        analysis.outro_bars = 48;
        analysis.outro_structure_bars = 32;
        store(dir.path(), hash, &analysis).unwrap();

        let result = set_manual_bars(dir.path(), hash, None, Some(80)).unwrap();

        assert_eq!(result.outro_bars, 80);
        assert_eq!(
            result.outro_structure_bars, 32,
            "more room in front of the outro doesn't move where the outro starts"
        );
    }

    /// Reanalysis re-applies the stored manual trigger onto a *fresh* boundary,
    /// so the same clamp has to run there too.
    #[test]
    fn apply_manual_overrides_clamps_the_structural_boundary() {
        let mut manual = sample_analysis();
        manual.outro_bars = 24;
        manual.outro_bars_manual = true;

        let mut fresh = sample_analysis();
        fresh.outro_bars = 48;
        fresh.outro_structure_bars = 32;

        let result = apply_manual_overrides(&manual, fresh);

        assert_eq!(result.outro_bars, 24);
        assert_eq!(result.outro_structure_bars, 24);
    }

    #[test]
    fn set_manual_structure_bars_rederives_the_trigger() {
        let dir = TempDir::new("structure");
        let hash = "hash-structure";
        store(dir.path(), hash, &sample_analysis()).unwrap();

        let result = set_manual_structure_bars(dir.path(), hash, 32).unwrap();

        assert_eq!(result.outro_structure_bars, 32);
        assert!(result.outro_structure_bars_manual);
        assert_eq!(
            result.outro_bars, 48,
            "the analyzer's rule: boundary plus the 16-bar lead-in"
        );
        assert!(!result.outro_bars_low_confidence);

        let bar_len = (60.0 / result.outro_bpm
            * f64::from(result.sample_rate)
            * f64::from(BEATS_PER_BAR))
        .round()
        .max(1.0) as u64;
        assert_eq!(result.outro_start, result.total_frames - 48 * bar_len);
        assert_eq!(load(dir.path(), hash).unwrap(), result);
    }

    /// The two manual outro edits describe the same edge, so the later one has
    /// to win outright — leaving both flags set would make the next reanalysis
    /// pick between them.
    #[test]
    fn manual_outro_edits_replace_each_other() {
        let dir = TempDir::new("structure-excl");
        let hash = "hash-structure-excl";
        store(dir.path(), hash, &sample_analysis()).unwrap();

        set_manual_bars(dir.path(), hash, None, Some(24)).unwrap();
        let result = set_manual_structure_bars(dir.path(), hash, 32).unwrap();
        assert!(!result.outro_bars_manual, "the pinned trigger is gone");
        assert_eq!(result.outro_bars, 48);

        let result = set_manual_bars(dir.path(), hash, None, Some(24)).unwrap();
        assert!(
            !result.outro_structure_bars_manual,
            "the hand-set boundary is gone"
        );
        assert_eq!(result.outro_bars, 24);
        assert_eq!(result.outro_structure_bars, 24, "clamped to the trigger");
    }

    /// Reanalysis has to re-derive the trigger rather than restore the stored
    /// one: the fresh analysis may have found a different intro or length.
    #[test]
    fn apply_manual_overrides_rederives_from_a_hand_set_boundary() {
        let mut manual = sample_analysis();
        manual.outro_structure_bars = 16;
        manual.outro_structure_bars_manual = true;

        let mut fresh = sample_analysis();
        fresh.outro_structure_bars = 64;
        fresh.outro_bars = 80;

        let result = apply_manual_overrides(&manual, fresh);

        assert_eq!(result.outro_structure_bars, 16);
        assert!(result.outro_structure_bars_manual);
        assert_eq!(result.outro_bars, 32);
        assert!(!result.needs_reanalysis);
    }

    /// Entries written before `track_bars` existed still have to derive a
    /// trigger; the file-length estimate stands in for the stored value.
    #[test]
    fn set_manual_structure_bars_without_stored_track_bars() {
        let dir = TempDir::new("structure-old");
        let hash = "hash-structure-old";
        let mut analysis = sample_analysis();
        analysis.track_bars = 0;
        store(dir.path(), hash, &analysis).unwrap();

        let result = set_manual_structure_bars(dir.path(), hash, 32).unwrap();
        assert_eq!(result.outro_bars, 48);
    }

    /// `purge_auto` protects hand-edited bars. A hand-set boundary is one, and
    /// deleting the entry would throw the edit away silently.
    #[test]
    fn purge_auto_keeps_a_hand_set_boundary() {
        let dir = TempDir::new("structure-purge");
        let hash = "hash-structure-purge";
        store(dir.path(), hash, &sample_analysis()).unwrap();
        set_manual_structure_bars(dir.path(), hash, 32).unwrap();

        let stats = purge_auto(dir.path()).unwrap();

        assert_eq!(stats.deleted, 0);
        assert_eq!(stats.cleared, 1);
        let kept = load(dir.path(), hash).expect("entry survives the purge");
        assert_eq!(kept.outro_structure_bars, 32);
        assert!(kept.outro_structure_bars_manual);
        assert!(kept.needs_reanalysis);
    }

    #[test]
    fn set_manual_bars_missing_hash_errors() {
        let dir = TempDir::new("missing");
        let err = set_manual_bars(dir.path(), "does-not-exist", Some(4), None).unwrap_err();
        match err {
            Error::Cache(msg) => assert!(msg.contains("does-not-exist")),
            other => panic!("expected Error::Cache, got {other:?}"),
        }
    }
}
