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

fn validate_hash(hash: &str) -> Result<()> {
    if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        Ok(())
    } else {
        Err(Error::Cache("invalid cache hash: expected 64 lowercase ASCII hex digits".into()))
    }
}

fn cache_path(cache_dir: &Path, hash: &str) -> Result<std::path::PathBuf> {
    validate_hash(hash)?;
    Ok(cache_dir.join(format!("{hash}.json")))
}

/// A normal cache miss is distinct from an obsolete entry and from a read error.
#[derive(Debug)]
pub enum CacheLookup {
    Hit(TrackAnalysis),
    Missing,
    VersionMismatch(u32),
}

#[derive(Debug)]
enum ReadError {
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl ReadError {
    fn diagnostic(&self, path: &Path) -> Error {
        match self {
            Self::Io(e) => Error::Cache(format!("cannot read cache '{}': {e}", path.display())),
            Self::Json(e) => Error::Cache(format!("corrupt cache JSON '{}': {e}", path.display())),
        }
    }
}

fn read_entry(path: &Path) -> std::result::Result<CacheLookup, ReadError> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(CacheLookup::Missing),
        Err(e) => return Err(ReadError::Io(e)),
    };
    let mut analysis: TrackAnalysis = serde_json::from_slice(&data).map_err(ReadError::Json)?;
    if analysis.version != CACHE_VERSION {
        return Ok(CacheLookup::VersionMismatch(analysis.version));
    }
    if let Some(ref scores) = analysis.classify_scores {
        analysis.is_funkot = scores.verdict();
    }
    Ok(CacheLookup::Hit(analysis))
}

/// Read with diagnostics: missing/obsolete entries are ordinary outcomes; invalid
/// hashes, corrupt JSON and I/O failures are errors identifying the operation.
/// This performs file I/O and must not run in an audio callback.
pub fn load_checked(cache_dir: &Path, hash: &str) -> Result<CacheLookup> {
    let path = cache_path(cache_dir, hash)?;
    read_entry(&path).map_err(|e| e.diagnostic(&path))
}

/// Load a cached analysis, retaining the original quiet `Option` cache-miss API.
/// Use [`load_checked`] for diagnostics. Analysis entry points report corrupt
/// caches when repairing them and propagate I/O errors without running analysis;
/// this compatibility lookup is also used by polling callers and does not log.
/// Stored classify scores are re-evaluated against the current verdict rule.
pub fn load(cache_dir: &Path, hash: &str) -> Option<TrackAnalysis> {
    match load_checked(cache_dir, hash) {
        Ok(CacheLookup::Hit(analysis)) => Some(analysis),
        _ => None,
    }
}

fn load_for_analysis(cache_dir: &Path, hash: &str) -> Result<Option<TrackAnalysis>> {
    let path = cache_path(cache_dir, hash)?;
    match read_entry(&path) {
        Ok(CacheLookup::Hit(a)) => Ok(Some(a)),
        Ok(_) => Ok(None),
        Err(ReadError::Json(e)) => {
            log::warn!("reanalyzing corrupt cache JSON '{}': {e}", path.display());
            Ok(None)
        }
        Err(e) => Err(e.diagnostic(&path)),
    }
}

const WRITE_LOCK: &str = ".write.lock";

/// A separate, persistent inode/handle serializes cooperating threads/processes.
/// Never remove/replace this file while this cache can be in use. JSON readers
/// need no lock. All of this I/O belongs on control/loader threads, not render.
struct CacheWriter {
    _lock: fs::File,
}

impl CacheWriter {
    fn acquire(cache_dir: &Path) -> Result<Self> {
        fs::create_dir_all(cache_dir).map_err(|e| {
            Error::Cache(format!("cannot create cache dir '{}': {e}", cache_dir.display()))
        })?;
        let path = cache_dir.join(WRITE_LOCK);
        let file = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
            .open(&path)
            .map_err(|e| Error::Cache(format!("cannot open cache lock '{}': {e}", path.display())))?;
        file.lock()
            .map_err(|e| Error::Cache(format!("cannot lock cache '{}': {e}", path.display())))?;
        Ok(Self { _lock: file })
    }

    fn write(&self, path: &Path, analysis: &TrackAnalysis) -> Result<()> {
        let mut file = tempfile::Builder::new().prefix(".cache-").suffix(".tmp")
            .tempfile_in(path.parent().expect("cache entry has a parent"))
            .map_err(|e| Error::Cache(format!("cannot create cache temporary file: {e}")))?;
        // Write directly to an exclusively created sibling; no target is changed
        // until persist. Dropping either file or PersistError cleans up only ours.
        #[cfg(test)]
        if WRITE_FAILURE.with(|f| f.get() == Some(WriteStage::DuringWrite)) {
            use std::io::Write;
            file.write_all(b"{").unwrap();
            fail_write(WriteStage::DuringWrite)?;
        }
        serde_json::to_writer_pretty(&mut file, analysis)
            .map_err(|e| Error::Cache(format!("cannot serialize/write cache '{}': {e}", path.display())))?;
        file.as_file().sync_all()
            .map_err(|e| Error::Cache(format!("cannot sync cache '{}': {e}", path.display())))?;
        #[cfg(test)]
        fail_write(WriteStage::BeforeReplace)?;
        file.persist(path)
            .map_err(|e| Error::Cache(format!("cannot replace cache '{}': {e}", path.display())))?;
        // Contents were synced, but the directory was not. This guarantees atomic
        // visibility, not persistence of the rename across a power failure.
        Ok(())
    }
}

/// Reconcile against the manual state observed *under the write lock*, including
/// explicit clears and the exclusive outro modes. Auto completion state is fresh.
fn reconcile_manual(latest: &TrackAnalysis, mut incoming: TrackAnalysis) -> TrackAnalysis {
    let needs_reanalysis = incoming.needs_reanalysis;
    incoming.intro_bars_manual = false;
    incoming.outro_bars_manual = false;
    incoming.outro_structure_bars_manual = false;
    if !latest.intro_bars_manual && !latest.outro_bars_manual && !latest.outro_structure_bars_manual {
        return incoming;
    }
    let mut merged = apply_manual_overrides(latest, incoming);
    merged.needs_reanalysis = needs_reanalysis;
    merged
}

fn commit_analysis(cache_dir: &Path, hash: &str, analysis: &TrackAnalysis, repair: bool) -> Result<TrackAnalysis> {
    let path = cache_path(cache_dir, hash)?;
    let writer = CacheWriter::acquire(cache_dir)?;
    let merged = match read_entry(&path) {
        Ok(CacheLookup::Hit(latest)) => reconcile_manual(&latest, analysis.clone()),
        Ok(CacheLookup::Missing | CacheLookup::VersionMismatch(_)) => analysis.clone(),
        // Reanalysis has already diagnosed the damaged cache. Only that path may
        // repair it; an arbitrary store must not silently destroy unreadable data.
        Err(ReadError::Json(_)) if repair => analysis.clone(),
        Err(e) => return Err(e.diagnostic(&path)),
    };
    writer.write(&path, &merged)?;
    Ok(merged)
}

/// Store analysis as complete pretty JSON, preserving the latest existing manual
/// state even if `analysis` is an older snapshot. Use the edit APIs for changing
/// manual state. On a miss this initializes from the supplied trusted snapshot.
/// Creates `cache_dir` if needed; never removes the previous file before replace.
/// See `docs/local-data.md` for the trusted-directory and durability boundaries.
pub fn store(cache_dir: &Path, hash: &str, analysis: &TrackAnalysis) -> Result<()> {
    commit_analysis(cache_dir, hash, analysis, false).map(|_| ())
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
    match fs::metadata(cache_dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(stats),
        Err(e) => return Err(Error::Cache(format!("cannot stat cache dir: {e}"))),
        Ok(_) => {}
    }
    let writer = CacheWriter::acquire(cache_dir)?;
    let entries = fs::read_dir(cache_dir).map_err(|e| {
        Error::Cache(format!(
            "cannot read cache dir '{}': {e}",
            cache_dir.display()
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::Cache(format!("cache dir entry: {e}")))?;
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()) == Some(WRITE_LOCK) {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            stats.skipped += 1;
            continue;
        }
        let Some(hash) = path.file_stem().and_then(|n| n.to_str()) else {
            stats.skipped += 1;
            continue;
        };
        if validate_hash(hash).is_err() {
            stats.skipped += 1;
            continue;
        }
        let mut analysis = match read_entry(&path) {
            Ok(CacheLookup::Hit(a)) => a,
            Ok(_) => { stats.skipped += 1; continue; }
            Err(ReadError::Json(e)) => {
                log::warn!("purge skipped corrupt cache '{}': {e}", path.display());
                stats.skipped += 1;
                continue;
            }
            Err(e) => return Err(e.diagnostic(&path)),
        };
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
        writer.write(&path, &analysis)?;
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
        fresh.outro_structure_bars_manual = false;
        fresh.outro_bars_low_confidence = false;
        recompute_outro_start(&mut fresh);
        clamp_structure_to_outro(&mut fresh);
    }
    fresh.bars_estimated_low_confidence =
        fresh.intro_bars_low_confidence || fresh.outro_bars_low_confidence;
    fresh.needs_reanalysis = false;
    fresh
}

fn require_entry(path: &Path) -> Result<TrackAnalysis> {
    match read_entry(path).map_err(|e| e.diagnostic(path))? {
        CacheLookup::Hit(a) => Ok(a),
        CacheLookup::Missing => Err(Error::Cache(format!("no cache entry '{}'", path.display()))),
        CacheLookup::VersionMismatch(version) => Err(Error::Cache(format!(
            "cache '{}' has version {version}, expected {CACHE_VERSION}", path.display()
        ))),
    }
}

/// Edit intro and/or structural outro in one transaction, including undo.
/// `None` leaves that side untouched. `Some` updates its value and sets the
/// manual flag to `manual`; false restores a value without pinning it. Structural
/// edits always derive the trigger and clear its manual flag. This preserves
/// `needs_reanalysis`, and does no analysis. Call only outside audio callbacks.
pub fn edit_bars(
    cache_dir: &Path,
    hash: &str,
    intro: Option<u32>,
    outro_structure: Option<u32>,
    manual: bool,
) -> Result<TrackAnalysis> {
    let path = cache_path(cache_dir, hash)?;
    let writer = CacheWriter::acquire(cache_dir)?;
    let mut analysis = require_entry(&path)?;
    if let Some(n) = intro {
        analysis.intro_bars = n;
        analysis.intro_bars_manual = manual;
        analysis.intro_bars_low_confidence = false;
    }
    if let Some(n) = outro_structure {
        analysis.outro_structure_bars = n;
        analysis.outro_structure_bars_manual = manual;
        analysis.outro_bars_manual = false;
        analysis.outro_bars_low_confidence = false;
        derive_outro_from_structure(&mut analysis);
    }
    analysis.bars_estimated_low_confidence =
        analysis.intro_bars_low_confidence || analysis.outro_bars_low_confidence;
    writer.write(&path, &analysis)?;
    Ok(analysis)
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
    let path = cache_path(cache_dir, hash)?;
    let writer = CacheWriter::acquire(cache_dir)?;
    let mut analysis = require_entry(&path)?;
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
    writer.write(&path, &analysis)?;
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
    let path = cache_path(cache_dir, hash)?;
    let writer = CacheWriter::acquire(cache_dir)?;
    let mut analysis = require_entry(&path)?;
    analysis.outro_structure_bars = bars;
    analysis.outro_structure_bars_manual = true;
    analysis.outro_bars_manual = false;
    analysis.outro_bars_low_confidence = false;
    derive_outro_from_structure(&mut analysis);
    analysis.bars_estimated_low_confidence =
        analysis.intro_bars_low_confidence || analysis.outro_bars_low_confidence;
    writer.write(&path, &analysis)?;
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
    let prior = load_for_analysis(cache_dir, &hash)?;
    if let Some(cached) = prior.as_ref() {
        if !cached.needs_reanalysis {
            return Ok(cached.clone());
        }
    }
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    let analysis = crate::analysis::analyze(buffer, file_name)?;
    // Analysis is expensive and stays outside the write lock. Re-read manual
    // state at commit time rather than resurrecting our initial snapshot.
    #[cfg(test)]
    BEFORE_ANALYSIS_COMMIT.with(|hook| {
        if let Some(before_commit) = hook.borrow_mut().take() { before_commit(); }
    });
    commit_analysis(cache_dir, &hash, &analysis, true)
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriteStage { DuringWrite, BeforeReplace }

#[cfg(test)]
thread_local! {
    static BEFORE_ANALYSIS_COMMIT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static WRITE_FAILURE: std::cell::Cell<Option<WriteStage>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn fail_write(stage: WriteStage) -> Result<()> {
    WRITE_FAILURE.with(|f| {
        if f.get() == Some(stage) {
            f.set(None);
            Err(Error::Cache(format!("injected cache write failure: {stage:?}")))
        } else { Ok(()) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClassifyScores;
    use std::sync::atomic::{AtomicU64, Ordering};

    const TEST_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

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
        let hash = TEST_HASH;
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
        let hash = TEST_HASH;
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
        let hash = TEST_HASH;
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
        let hash = TEST_HASH;
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
        let hash = TEST_HASH;
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
        let hash = TEST_HASH;
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
        let hash = TEST_HASH;
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
        let hash = TEST_HASH;
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
        let hash = TEST_HASH;
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
        let err = set_manual_bars(dir.path(), TEST_HASH, Some(4), None).unwrap_err();
        match err {
            Error::Cache(msg) => assert!(msg.contains("no cache entry")),
            other => panic!("expected Error::Cache, got {other:?}"),
        }
    }

    /// Scores below the live cut must flip `is_funkot` on load even when the
    /// JSON still says true (threshold retune without CACHE_VERSION bump).
    #[test]
    fn load_reapplies_verdict_from_classify_scores() {
        let dir = TempDir::new("verdict-reload");
        let hash = TEST_HASH;
        let mut analysis = sample_analysis();
        analysis.is_funkot = true;
        analysis.classify_scores = Some(ClassifyScores {
            head_z: 9.0,
            head_z_ratio: 1.0,
            head_half_ratio: 0.8,
            tail_z: 9.0,
            tail_z_ratio: 1.0,
            tail_half_ratio: 0.8,
        });
        store(dir.path(), hash, &analysis).unwrap();

        let loaded = load(dir.path(), hash).expect("load");
        assert!(
            !loaded.is_funkot,
            "z=9.0 is below CLASSIFY_MIN_Z 10.7; stored true must not win"
        );
        assert!(loaded.classify_scores.is_some());
    }
    #[test]
    fn invalid_hash_never_accesses_or_creates_a_path() {
        let dir = TempDir::new("invalid-hash");
        let cache = dir.path().join("cache");
        let outside = dir.path().join("outside.json");
        let original = serde_json::to_vec(&sample_analysis()).unwrap();
        fs::write(&outside, &original).unwrap();
        let long = "a".repeat(65);
        let absolute = dir.path().join("outside").to_string_lossy().into_owned();
        for hash in ["", "../outside", "..\\outside", "/outside", "C:\\outside", "C:/outside",
                     "a/b", "a\\b", "abc", &long, &"g".repeat(64), &"A".repeat(64), &absolute] {
            assert!(load_checked(&cache, hash).unwrap_err().to_string().contains("invalid cache hash"));
            assert!(load(&cache, hash).is_none());
            assert!(store(&cache, hash, &sample_analysis()).is_err());
            assert!(set_manual_bars(&cache, hash, Some(1), None).is_err());
            assert!(set_manual_structure_bars(&cache, hash, 1).is_err());
            assert!(edit_bars(&cache, hash, Some(1), Some(1), false).is_err());
        }
        assert!(!cache.exists(), "validation must precede any directory/lock creation");
        assert_eq!(fs::read(outside).unwrap(), original);
    }

    #[test]
    fn checked_load_distinguishes_miss_version_corruption_and_io() {
        let dir = TempDir::new("read-errors");
        let path = cache_path(dir.path(), TEST_HASH).unwrap();
        assert!(matches!(load_checked(dir.path(), TEST_HASH).unwrap(), CacheLookup::Missing));
        let mut old = sample_analysis();
        old.version = CACHE_VERSION - 1;
        fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        assert!(matches!(load_checked(dir.path(), TEST_HASH).unwrap(), CacheLookup::VersionMismatch(13)));
        assert!(set_manual_bars(dir.path(), TEST_HASH, Some(1), None).unwrap_err().to_string().contains("version 13"));
        fs::write(&path, "{broken").unwrap();
        assert!(load_checked(dir.path(), TEST_HASH).unwrap_err().to_string().contains("corrupt cache JSON"));
        assert!(store(dir.path(), TEST_HASH, &sample_analysis()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{broken");
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(load_checked(dir.path(), TEST_HASH).unwrap_err().to_string().contains("cannot read cache"));
        assert!(store(dir.path(), TEST_HASH, &sample_analysis()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn permission_failure_is_not_a_miss_or_successful_overwrite() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("permissions");
        let path = cache_path(dir.path(), TEST_HASH).unwrap();
        store(dir.path(), TEST_HASH, &sample_analysis()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0)).unwrap();
        // Root can bypass mode bits; do not falsely claim permission coverage.
        if fs::read(&path).is_ok() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            eprintln!("SKIP permission denial: process can bypass Unix mode bits");
            return;
        }
        let error = load_checked(dir.path(), TEST_HASH).unwrap_err();
        assert!(error.to_string().contains("cannot read cache"));
        assert!(store(dir.path(), TEST_HASH, &sample_analysis()).is_err());
        let source = dir.path().join("source.wav");
        fs::write(&source, "permission identity").unwrap();
        let hash = content_hash(&source).unwrap();
        let inaccessible = cache_path(dir.path(), &hash).unwrap();
        fs::write(&inaccessible, serde_json::to_vec(&sample_analysis()).unwrap()).unwrap();
        fs::set_permissions(&inaccessible, fs::Permissions::from_mode(0)).unwrap();
        // Empty audio would fail analysis with a different error. The I/O error
        // must win before the analyzer sees it.
        let empty = crate::decode::AudioBuffer { samples: vec![], sample_rate: 44_100, frames: 0 };
        let error = get_or_analyze(&source, dir.path(), &empty).unwrap_err();
        assert!(error.to_string().contains("cannot read cache"), "{error}");
        fs::set_permissions(&inaccessible, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(load(dir.path(), TEST_HASH).unwrap(), sample_analysis());
    }

    #[test]
    fn failed_write_and_replace_leave_old_json_and_other_temporary_files() {
        let dir = TempDir::new("atomic-failure");
        let path = cache_path(dir.path(), TEST_HASH).unwrap();
        let old = sample_analysis();
        store(dir.path(), TEST_HASH, &old).unwrap();
        let bytes = fs::read(&path).unwrap();
        let other = dir.path().join(".cache-other-writer.tmp");
        fs::write(&other, "owned by another writer").unwrap();
        let mut new = old.clone();
        new.file_name = "replacement.wav".into();
        for stage in [WriteStage::DuringWrite, WriteStage::BeforeReplace] {
            WRITE_FAILURE.with(|f| f.set(Some(stage)));
            assert!(store(dir.path(), TEST_HASH, &new).unwrap_err().to_string().contains("injected"));
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(fs::read_to_string(&other).unwrap(), "owned by another writer");
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3, "only entry, lock, other temp");
        }
        // Exercise a real persist error too, without removing an existing target.
        let writer = CacheWriter::acquire(dir.path()).unwrap();
        let destination = dir.path().join("blocked.json");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("sentinel"), "keep").unwrap();
        assert!(writer.write(&destination, &new).unwrap_err().to_string().contains("cannot replace"));
        assert_eq!(fs::read_to_string(destination.join("sentinel")).unwrap(), "keep");
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 4);
    }

    #[test]
    fn concurrent_store_and_read_publish_only_complete_snapshots() {
        use std::sync::{Arc, Barrier};
        let dir = TempDir::new("atomic-readers");
        let a = sample_analysis();
        let mut b = a.clone();
        b.file_name = "b".repeat(16_384);
        store(dir.path(), TEST_HASH, &a).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        std::thread::scope(|scope| {
            for snapshot in [&a, &b] {
                let barrier = barrier.clone();
                let cache = dir.path();
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..32 { store(cache, TEST_HASH, snapshot).unwrap(); }
                });
            }
            barrier.wait();
            for _ in 0..128 {
                let value = match load_checked(dir.path(), TEST_HASH).unwrap() {
                    CacheLookup::Hit(value) => value,
                    other => panic!("replacement exposed a gap: {other:?}"),
                };
                assert!(value == a || value == b, "reader observed a partial/mixed snapshot");
            }
        });
    }

    #[test]
    fn stale_store_keeps_latest_manual_edit_and_exclusive_mode() {
        let dir = TempDir::new("stale-store");
        let stale = sample_analysis();
        store(dir.path(), TEST_HASH, &stale).unwrap();
        edit_bars(dir.path(), TEST_HASH, Some(12), Some(32), true).unwrap();
        store(dir.path(), TEST_HASH, &stale).unwrap();
        let saved = load(dir.path(), TEST_HASH).unwrap();
        assert!(saved.intro_bars_manual && saved.outro_structure_bars_manual);
        assert_eq!((saved.intro_bars, saved.outro_structure_bars, saved.outro_bars), (12, 32, 48));
        set_manual_bars(dir.path(), TEST_HASH, None, Some(24)).unwrap();
        store(dir.path(), TEST_HASH, &saved).unwrap();
        let saved = load(dir.path(), TEST_HASH).unwrap();
        assert!(saved.outro_bars_manual && !saved.outro_structure_bars_manual);
        assert_eq!((saved.outro_bars, saved.outro_structure_bars), (24, 24));
        edit_bars(dir.path(), TEST_HASH, Some(8), Some(16), false).unwrap();
        store(dir.path(), TEST_HASH, &saved).unwrap();
        let saved = load(dir.path(), TEST_HASH).unwrap();
        assert!(!saved.intro_bars_manual && !saved.outro_bars_manual && !saved.outro_structure_bars_manual);
    }

    #[test]
    fn reanalysis_commits_and_returns_manual_edits_made_after_its_snapshot() {
        let dir = TempDir::new("reanalyze-race");
        let source = dir.path().join("track.wav");
        fs::write(&source, "synthetic source identity").unwrap();
        let hash = content_hash(&source).unwrap();
        store(dir.path(), &hash, &sample_analysis()).unwrap();
        set_manual_bars(dir.path(), &hash, Some(8), None).unwrap();
        purge_auto(dir.path()).unwrap();
        let cache = dir.path().to_path_buf();
        let key = hash.clone();
        BEFORE_ANALYSIS_COMMIT.with(|hook| *hook.borrow_mut() = Some(Box::new(move || {
            edit_bars(&cache, &key, Some(12), Some(16), true).unwrap();
        })));
        let buffer = crate::testutil::synth_track(180.0, 16, 16, 16, 44_100);
        let result = get_or_analyze(&source, dir.path(), &buffer).unwrap();
        assert!(result.intro_bars_manual && result.outro_structure_bars_manual);
        assert_eq!((result.intro_bars, result.outro_structure_bars), (12, 16));
        assert!(!result.needs_reanalysis);
        assert_eq!(result, load(dir.path(), &hash).unwrap());
    }

    #[test]
    fn purge_waits_for_manual_transaction_and_keeps_its_latest_state() {
        let dir = TempDir::new("purge-race");
        store(dir.path(), TEST_HASH, &sample_analysis()).unwrap();
        let writer = CacheWriter::acquire(dir.path()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let cache = dir.path();
            let purge = scope.spawn(move || {
                tx.send(()).unwrap();
                purge_auto(cache).unwrap()
            });
            rx.recv().unwrap();
            let mut edited = sample_analysis();
            edited.intro_bars = 12;
            edited.intro_bars_manual = true;
            writer.write(&cache_path(cache, TEST_HASH).unwrap(), &edited).unwrap();
            drop(writer);
            let stats = purge.join().unwrap();
            assert_eq!((stats.deleted, stats.cleared, stats.skipped), (0, 1, 0));
        });
        let result = load(dir.path(), TEST_HASH).unwrap();
        assert!(result.intro_bars_manual && result.needs_reanalysis);
        assert_eq!(result.intro_bars, 12);
    }

    #[test]
    #[ignore = "subprocess helper for multiprocess_manual_updates; not a standalone test"]
    fn process_writer() {
        use std::io::Write;
        let dir = std::path::PathBuf::from(std::env::var_os("FUNKOT_CACHE_TEST_DIR").expect("test dir"));
        let side = std::env::var("FUNKOT_CACHE_TEST_SIDE").unwrap();
        println!("CACHE_WRITER_READY");
        std::io::stdout().flush().unwrap();
        if side == "intro" {
            set_manual_bars(&dir, TEST_HASH, Some(12), None).unwrap();
        } else {
            set_manual_structure_bars(&dir, TEST_HASH, 32).unwrap();
        }
    }

    #[test]
    fn multiprocess_manual_updates_preserve_both_sides() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let dir = TempDir::new("processes");
        store(dir.path(), TEST_HASH, &sample_analysis()).unwrap();
        let lock = CacheWriter::acquire(dir.path()).unwrap();
        // Separate handles must conflict even within this process.
        let second = fs::OpenOptions::new().read(true).write(true).open(dir.path().join(WRITE_LOCK)).unwrap();
        assert!(matches!(second.try_lock(), Err(fs::TryLockError::WouldBlock)));
        let mut children = Vec::new();
        for side in ["intro", "structure"] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "cache::tests::process_writer", "--ignored", "--nocapture"])
                .env("FUNKOT_CACHE_TEST_DIR", dir.path()).env("FUNKOT_CACHE_TEST_SIDE", side)
                .stdout(Stdio::piped()).spawn().unwrap();
            let stdout = child.stdout.take().unwrap();
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0, "child exited before readiness");
                if line.contains("CACHE_WRITER_READY") { break; }
            }
            children.push((child, reader));
        }
        drop(lock);
        for (mut child, mut reader) in children {
            let mut output = String::new();
            reader.read_to_string(&mut output).unwrap();
            assert!(child.wait().unwrap().success(), "child failed: {output}");
        }
        let result = load(dir.path(), TEST_HASH).unwrap();
        assert!(result.intro_bars_manual && result.outro_structure_bars_manual);
        assert_eq!((result.intro_bars, result.outro_structure_bars), (12, 32));
    }

}
