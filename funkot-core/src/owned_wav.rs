//! Claimed development WAVs: written by gen_synth / phase_ab / --gen-test-fixtures,
//! reclaimed by those same tools once RECLAIM_AFTER has passed since the write.
//!
//! A WAV `x.wav` is claimed iff a sidecar `.x.wav.owned` sits next to it and describes the
//! exact file (dev, ino, len, mtime_ns). Only `write_owned` creates sidecars, and only inside a
//! resolved `Root`. Sweep acts on sidecars only. A WAV with no sidecar is unknown and is never
//! touched. This covers every legacy WAV, every `--render` output, and anything a human placed
//! there.
//!
//! To keep a claimed WAV: delete its `.owned` sidecar. Rerunning the generator renews the lease.

use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long a claimed WAV survives after it was written. The only retention knob.
///
/// 8 days. Weak, single-episode evidence (phase_ab, 2026-08): render 08-03, last recorded
/// reuse 08-04, manual deletion 08-11. Observed reuse under a day. 8 d matches the human's
/// own disposable judgment. Too short costs a regeneration, never data.
pub const RECLAIM_AFTER: Duration = Duration::from_secs(8 * 24 * 60 * 60);

/// The only directories that may hold claims, relative to the checkout root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Root {
    Synth,
    PhaseAb,
    Fixtures,
}

impl Root {
    pub const ALL: [Root; 3] = [Root::Synth, Root::PhaseAb, Root::Fixtures];

    fn rel(self) -> &'static str {
        match self {
            Root::Synth => "testdata/synth",
            Root::PhaseAb => "testdata/phase_ab",
            Root::Fixtures => "funkot-core/tests/fixtures",
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Root::Synth => "synth",
            Root::PhaseAb => "phase_ab",
            Root::Fixtures => "fixtures",
        }
    }

    fn from_tag(tag: &str) -> Option<Root> {
        match tag {
            "synth" => Some(Root::Synth),
            "phase_ab" => Some(Root::PhaseAb),
            "fixtures" => Some(Root::Fixtures),
            _ => None,
        }
    }
}

/// A checkout whose roots may be claimed and swept.
pub struct Checkout {
    base: Result<Base, Refusal>,
}

struct Base {
    repo: PathBuf,
    dev: u64,
    masters: Option<PathBuf>,
}

/// Handle returned by [`Checkout::open`]: sweeps once, then allows [`Opened::write_owned`].
pub struct Opened<'a> {
    checkout: &'a Checkout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Refusal {
    #[allow(dead_code)] // constructed on non-unix targets
    Unsupported,
    Symlink(PathBuf),
    OtherMount(PathBuf),
    NestedRepo(PathBuf),
    OverlapsMasters(PathBuf),
    Io(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RootState {
    Absent,
    Refused(Refusal),
}

struct Resolved {
    root: Root,
    path: PathBuf,
}

/// Outcome of `write_owned`, displayed next to "wrote …".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claimed {
    Yes { root: Root },
    No(NotClaimed),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotClaimed {
    OutsideRoots,
    RootRefused(String),
    NotRegularAfterWrite,
}

impl fmt::Display for Claimed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Claimed::Yes { root } => write!(f, "claimed ({})", root.tag()),
            Claimed::No(NotClaimed::OutsideRoots) => write!(f, "unclaimed (outside roots)"),
            Claimed::No(NotClaimed::RootRefused(r)) => write!(f, "unclaimed ({r})"),
            Claimed::No(NotClaimed::NotRegularAfterWrite) => {
                write!(f, "unclaimed (not a regular file)")
            }
        }
    }
}

impl Checkout {
    /// The checkout this binary was built from (`CARGO_MANIFEST_DIR/..`).
    pub fn this() -> Checkout {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        Checkout::at(&repo)
    }

    /// A checkout rooted at `repo`. For tests with a temp tree.
    pub fn at(repo: &Path) -> Checkout {
        Checkout::at_inner(repo, masters_from_env())
    }

    fn at_inner(repo: &Path, masters: Option<PathBuf>) -> Checkout {
        #[cfg(not(unix))]
        {
            let _ = (repo, masters);
            return Checkout {
                base: Err(Refusal::Unsupported),
            };
        }
        #[cfg(unix)]
        {
            match open_base(repo, masters) {
                Ok(b) => Checkout { base: Ok(b) },
                Err(r) => Checkout { base: Err(r) },
            }
        }
    }

    /// Sweep once (one stderr summary line), then return a handle for writes.
    pub fn open(&self, now: SystemTime) -> Opened<'_> {
        let report = sweep(self, now);
        eprintln!("{report}");
        Opened { checkout: self }
    }

    fn resolve(&self, root: Root) -> Result<Resolved, RootState> {
        let base = match &self.base {
            Ok(b) => b,
            Err(r) => return Err(RootState::Refused(r.clone())),
        };
        resolve_root(base, root)
    }

    /// Which root `dir` names, even when that root is currently refused.
    fn root_of(&self, dir: &Path) -> Option<Root> {
        let base = self.base.as_ref().ok()?;
        for root in Root::ALL {
            if dir_matches_root(base, root, dir) {
                return Some(root);
            }
        }
        None
    }
}

impl Opened<'_> {
    /// Write `dir/name` through `write` (temp+rename) and claim it when `dir` is a resolved root.
    pub fn write_owned<T, E>(
        &self,
        dir: &Path,
        name: &str,
        now: SystemTime,
        write: impl FnOnce(&Path) -> Result<T, E>,
    ) -> Result<(T, Claimed), E>
    where
        E: From<io::Error>,
    {
        write_owned(self.checkout, dir, name, now, write)
    }
}

/// Write `dir/name` through `write` and claim it when `dir` is a resolved root.
///
/// Order:
///   1. remove `.name.owned` if present (disown)
///   2. write via temp path, rename into place
///   3. if `root_of(dir)` resolves: lstat the file, write sidecar via temp+rename
pub fn write_owned<T, E>(
    checkout: &Checkout,
    dir: &Path,
    name: &str,
    now: SystemTime,
    write: impl FnOnce(&Path) -> Result<T, E>,
) -> Result<(T, Claimed), E>
where
    E: From<io::Error>,
{
    if name.contains('/') || name.contains('\\') || name.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "owned_wav: bad name").into());
    }

    let final_path = dir.join(name);
    let side = dir.join(sidecar_name(name));
    let _ = std::fs::remove_file(&side);

    let tmp = dir.join(format!(".{name}.partial"));
    let _ = std::fs::remove_file(&tmp);
    let value = match write(&tmp) {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    std::fs::rename(&tmp, &final_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e
    })?;

    let claimed = claim_after_write(checkout, dir, name, &final_path, now);
    Ok((value, claimed))
}

fn claim_after_write(
    checkout: &Checkout,
    dir: &Path,
    name: &str,
    final_path: &Path,
    now: SystemTime,
) -> Claimed {
    let Some(root) = checkout.root_of(dir) else {
        return Claimed::No(NotClaimed::OutsideRoots);
    };
    match checkout.resolve(root) {
        Err(RootState::Absent) => Claimed::No(NotClaimed::OutsideRoots),
        Err(RootState::Refused(r)) => Claimed::No(NotClaimed::RootRefused(refusal_tag(&r))),
        Ok(resolved) => {
            #[cfg(not(unix))]
            {
                let _ = (final_path, name, now, resolved);
                return Claimed::No(NotClaimed::RootRefused(refusal_tag(&Refusal::Unsupported)));
            }
            #[cfg(unix)]
            {
                match observe_path(final_path) {
                    None => Claimed::No(NotClaimed::NotRegularAfterWrite),
                    Some(obs) if !obs.regular => Claimed::No(NotClaimed::NotRegularAfterWrite),
                    Some(obs) => {
                        let claim = Claim {
                            root,
                            file: name.to_string(),
                            claimed: now,
                            id: obs.id,
                        };
                        match write_sidecar(&resolved.path, &claim) {
                            Ok(()) => Claimed::Yes { root },
                            Err(e) => Claimed::No(NotClaimed::RootRefused(format!("io: {e}"))),
                        }
                    }
                }
            }
        }
    }
}

fn dir_matches_root(base: &Base, root: Root, dir: &Path) -> bool {
    let expected = base.repo.join(root.rel());
    if dir == expected {
        return true;
    }
    match (canonical_existing(dir), canonical_existing(&expected)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn refusal_tag(r: &Refusal) -> String {
    match r {
        Refusal::Unsupported => "unsupported".into(),
        Refusal::Symlink(_) => "symlink".into(),
        Refusal::OtherMount(_) => "other-mount".into(),
        Refusal::NestedRepo(_) => "nested-repo".into(),
        Refusal::OverlapsMasters(_) => "overlaps-masters".into(),
        Refusal::Io(s) => format!("io: {s}"),
    }
}

fn sweep(checkout: &Checkout, now: SystemTime) -> SweepReport {
    let mut report = SweepReport::default();
    for root in Root::ALL {
        match checkout.resolve(root) {
            Err(RootState::Absent) => report.roots.push((root, RootReport::Absent)),
            Err(RootState::Refused(r)) => report.roots.push((root, RootReport::Refused(r))),
            Ok(resolved) => {
                let swept = sweep_resolved(&resolved, now);
                report.roots.push((root, RootReport::Swept(swept)));
            }
        }
    }
    report
}

#[derive(Debug, Default)]
struct SweepReport {
    roots: Vec<(Root, RootReport)>,
}

#[derive(Debug)]
enum RootReport {
    Absent,
    Refused(Refusal),
    Swept(SweptRoot),
}

#[derive(Debug, Default)]
struct SweptRoot {
    reclaimed: Vec<(String, u64)>,
    kept: Vec<(String, KeepReason)>,
    orphan_claims_dropped: u32,
    unknown_wavs: u32,
    unknown_bytes: u64,
    errors: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeepReason {
    InWindow,
    ClockBehind,
    Mismatch,
    NotRegular,
    Hardlinked,
    WrongRoot,
}

impl SweepReport {
    #[cfg(test)]
    fn reclaimed(&self) -> Vec<&str> {
        let mut out = Vec::new();
        for (_, rr) in &self.roots {
            if let RootReport::Swept(s) = rr {
                for (name, _) in &s.reclaimed {
                    out.push(name.as_str());
                }
            }
        }
        out
    }

    /// True when nothing was reclaimed, dropped, or errored.
    #[cfg(test)]
    fn is_noop(&self) -> bool {
        for (_, rr) in &self.roots {
            if let RootReport::Swept(s) = rr {
                if !s.reclaimed.is_empty()
                    || s.orphan_claims_dropped > 0
                    || !s.errors.is_empty()
                    || s.kept.iter().any(|(_, k)| *k == KeepReason::Mismatch)
                {
                    return false;
                }
            }
        }
        true
    }
}

impl fmt::Display for SweepReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut reclaimed_n = 0u64;
        let mut reclaimed_bytes = 0u64;
        let mut kept_in = 0u64;
        let mut unknown_n = 0u64;
        let mut unknown_bytes = 0u64;
        let mut refused: Vec<String> = Vec::new();
        for (root, rr) in &self.roots {
            match rr {
                RootReport::Refused(r) => {
                    refused.push(format!("{}:{}", root.tag(), refusal_tag(r)));
                }
                RootReport::Absent => {}
                RootReport::Swept(s) => {
                    reclaimed_n += s.reclaimed.len() as u64;
                    reclaimed_bytes += s.reclaimed.iter().map(|(_, b)| *b).sum::<u64>();
                    kept_in += s
                        .kept
                        .iter()
                        .filter(|(_, k)| *k == KeepReason::InWindow)
                        .count() as u64;
                    unknown_n += s.unknown_wavs as u64;
                    unknown_bytes += s.unknown_bytes;
                }
            }
        }
        let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
        write!(
            f,
            "owned_wav: reclaimed {reclaimed_n} ({:.1} MB) older than 8d; kept {kept_in} in-window, {unknown_n} unknown ({:.1} MB)",
            mb(reclaimed_bytes),
            mb(unknown_bytes)
        )?;
        if !refused.is_empty() {
            write!(f, "; refused: {}", refused.join(","))?;
        }
        Ok(())
    }
}

fn sweep_resolved(resolved: &Resolved, now: SystemTime) -> SweptRoot {
    let mut out = SweptRoot::default();
    let entries = match std::fs::read_dir(&resolved.path) {
        Ok(e) => e,
        Err(e) => {
            out.errors
                .push((".".into(), format!("read_dir: {e}")));
            return out;
        }
    };

    let mut sidecars: Vec<PathBuf> = Vec::new();
    let mut wavs: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_file() && !meta.file_type().is_symlink() {
            // Still consider regular-file metadata via symlink_metadata for type.
        }
        if name.starts_with('.') && name.ends_with(".owned") {
            if meta.file_type().is_file() {
                sidecars.push(path);
            }
            continue;
        }
        if name.ends_with(".wav") && !name.starts_with('.') && meta.file_type().is_file() {
            wavs.push(path);
        }
    }

    let mut claimed_wavs = std::collections::HashSet::new();
    for side in sidecars {
        let side_name = side
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let body = match std::fs::read_to_string(&side) {
            Ok(b) => b,
            Err(e) => {
                out.errors.push((side_name, e.to_string()));
                continue;
            }
        };
        let Some(claim) = parse_claim(&side_name, &body) else {
            // Garbage sidecar: not a claim. Leave it; count WAV as unknown below.
            continue;
        };
        claimed_wavs.insert(claim.file.clone());
        let wav_path = resolved.path.join(&claim.file);
        let observed = observe_path(&wav_path);
        let verdict = decide(&claim, resolved.root, observed, now);
        match verdict {
            Verdict::Reclaim => {
                let bytes = observed.map(|o| o.id.len).unwrap_or(0);
                match apply_reclaim(&wav_path, &side, &claim) {
                    Ok(true) => out.reclaimed.push((claim.file, bytes)),
                    Ok(false) => out.kept.push((claim.file, KeepReason::Mismatch)),
                    Err(e) => out.errors.push((claim.file, e.to_string())),
                }
            }
            Verdict::DropOrphanClaim => {
                let _ = std::fs::remove_file(&side);
                out.orphan_claims_dropped += 1;
            }
            Verdict::Keep(KeepReason::Mismatch) => {
                let _ = std::fs::remove_file(&side);
                out.kept.push((claim.file, KeepReason::Mismatch));
            }
            Verdict::Keep(reason) => {
                out.kept.push((claim.file, reason));
            }
        }
    }

    for wav in wavs {
        let name = wav
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if claimed_wavs.contains(&name) {
            continue;
        }
        // Still claimed if a valid sidecar survived for it.
        let side = wav
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(sidecar_name(&name));
        if side.is_file() {
            if let Ok(body) = std::fs::read_to_string(&side) {
                if parse_claim(
                    side.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                    &body,
                )
                .is_some()
                {
                    continue;
                }
            }
        }
        let len = std::fs::metadata(&wav).map(|m| m.len()).unwrap_or(0);
        out.unknown_wavs += 1;
        out.unknown_bytes += len;
    }

    out
}

/// Tombstone rename, restat, unlink only on identity match, else rename back. Then drop sidecar.
fn apply_reclaim(wav_path: &Path, side: &Path, claim: &Claim) -> io::Result<bool> {
    #[cfg(not(unix))]
    {
        let _ = (wav_path, side, claim);
        return Ok(false);
    }
    #[cfg(unix)]
    {
        if !wav_path.exists() {
            let _ = std::fs::remove_file(side);
            return Ok(true);
        }
        let tomb = wav_path.with_file_name(format!(
            ".{}.reclaiming",
            wav_path.file_name().and_then(|n| n.to_str()).unwrap_or("wav")
        ));
        let _ = std::fs::remove_file(&tomb);
        std::fs::rename(wav_path, &tomb)?;
        let deleted = match observe_path(&tomb) {
            Some(obs) if obs.regular && obs.id == claim.id => match std::fs::remove_file(&tomb) {
                Ok(()) => true,
                Err(e) => {
                    let _ = std::fs::rename(&tomb, wav_path);
                    return Err(e);
                }
            },
            _ => {
                let _ = std::fs::rename(&tomb, wav_path);
                false
            }
        };
        if deleted {
            let _ = std::fs::remove_file(side);
        }
        Ok(deleted)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Claim {
    root: Root,
    file: String,
    claimed: SystemTime,
    id: FileId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileId {
    dev: u64,
    ino: u64,
    len: u64,
    mtime_ns: i128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Observed {
    regular: bool,
    nlink: u64,
    id: FileId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Reclaim,
    DropOrphanClaim,
    Keep(KeepReason),
}

fn decide(claim: &Claim, here: Root, observed: Option<Observed>, now: SystemTime) -> Verdict {
    if claim.root != here {
        return Verdict::Keep(KeepReason::WrongRoot);
    }
    let Some(obs) = observed else {
        return Verdict::DropOrphanClaim;
    };
    if !obs.regular {
        return Verdict::Keep(KeepReason::NotRegular);
    }
    if obs.id != claim.id {
        return Verdict::Keep(KeepReason::Mismatch);
    }
    if obs.nlink > 1 {
        return Verdict::Keep(KeepReason::Hardlinked);
    }
    if now < claim.claimed {
        return Verdict::Keep(KeepReason::ClockBehind);
    }
    match now.duration_since(claim.claimed) {
        Ok(age) if age <= RECLAIM_AFTER => Verdict::Keep(KeepReason::InWindow),
        Ok(_) => Verdict::Reclaim,
        Err(_) => Verdict::Keep(KeepReason::ClockBehind),
    }
}

fn sidecar_name(wav: &str) -> String {
    format!(".{wav}.owned")
}

fn wav_name_from_sidecar(sidecar_name: &str) -> Option<&str> {
    let s = sidecar_name.strip_prefix('.')?;
    s.strip_suffix(".owned")
}

fn parse_claim(sidecar_fname: &str, body: &str) -> Option<Claim> {
    let file_from_name = wav_name_from_sidecar(sidecar_fname)?;
    let line = body.trim();
    let rest = line.strip_prefix("funkot-owned-wav v1 ")?;
    let mut root = None;
    let mut file = None;
    let mut claimed = None;
    let mut dev = None;
    let mut ino = None;
    let mut len = None;
    let mut mtime_ns = None;
    for part in rest.split_whitespace() {
        let (k, v) = part.split_once('=')?;
        match k {
            "root" => root = Root::from_tag(v),
            "file" => {
                if v.contains('/') || v.contains('\\') {
                    return None;
                }
                file = Some(v.to_string());
            }
            "claimed" => {
                let secs: u64 = v.parse().ok()?;
                claimed = Some(UNIX_EPOCH + Duration::from_secs(secs));
            }
            "dev" => dev = v.parse().ok(),
            "ino" => ino = v.parse().ok(),
            "len" => len = v.parse().ok(),
            "mtime_ns" => mtime_ns = v.parse().ok(),
            _ => return None,
        }
    }
    let file = file?;
    if file != file_from_name {
        return None;
    }
    Some(Claim {
        root: root?,
        file,
        claimed: claimed?,
        id: FileId {
            dev: dev?,
            ino: ino?,
            len: len?,
            mtime_ns: mtime_ns?,
        },
    })
}

fn render_claim(claim: &Claim) -> String {
    let claimed = claim
        .claimed
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!(
        "funkot-owned-wav v1 root={} file={} claimed={} dev={} ino={} len={} mtime_ns={}\n",
        claim.root.tag(),
        claim.file,
        claimed,
        claim.id.dev,
        claim.id.ino,
        claim.id.len,
        claim.id.mtime_ns
    )
}

fn write_sidecar(dir: &Path, claim: &Claim) -> io::Result<()> {
    let final_side = dir.join(sidecar_name(&claim.file));
    let tmp = dir.join(format!(".{}.owned.partial", claim.file));
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, render_claim(claim))?;
    std::fs::rename(&tmp, &final_side).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e
    })
}

fn masters_from_env() -> Option<PathBuf> {
    let dir = std::env::var_os("FUNKOT_TESTDATA_DIR")?;
    if dir.is_empty() {
        return None;
    }
    let p = PathBuf::from(dir);
    canonical_existing(&p).ok()
}

#[cfg(unix)]
fn open_base(repo: &Path, masters: Option<PathBuf>) -> Result<Base, Refusal> {
    let meta = std::fs::symlink_metadata(repo).map_err(|e| Refusal::Io(e.to_string()))?;
    if meta.file_type().is_symlink() {
        return Err(Refusal::Symlink(repo.to_path_buf()));
    }
    if !meta.is_dir() {
        return Err(Refusal::Io(format!("{} is not a directory", repo.display())));
    }
    use std::os::unix::fs::MetadataExt;
    let repo_canon = canonical_existing(repo).map_err(|e| Refusal::Io(e.to_string()))?;
    Ok(Base {
        repo: repo_canon,
        dev: meta.dev(),
        masters,
    })
}

fn resolve_root(base: &Base, root: Root) -> Result<Resolved, RootState> {
    #[cfg(not(unix))]
    {
        let _ = (base, root);
        return Err(RootState::Refused(Refusal::Unsupported));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mut cur = base.repo.clone();
        let rel = Path::new(root.rel());
        let comps: Vec<Component> = rel.components().collect();
        for (i, comp) in comps.iter().enumerate() {
            match comp {
                Component::Normal(s) => cur.push(s),
                _ => {
                    return Err(RootState::Refused(Refusal::Io(format!(
                        "bad root path {}",
                        root.rel()
                    ))));
                }
            }
            let meta = match std::fs::symlink_metadata(&cur) {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::NotFound && i + 1 == comps.len() => {
                    return Err(RootState::Absent);
                }
                Err(e) => {
                    return Err(RootState::Refused(Refusal::Io(e.to_string())));
                }
            };
            if meta.file_type().is_symlink() {
                return Err(RootState::Refused(Refusal::Symlink(cur)));
            }
            if i + 1 == comps.len() {
                if !meta.is_dir() {
                    return Err(RootState::Refused(Refusal::Io(format!(
                        "{} is not a directory",
                        cur.display()
                    ))));
                }
                if meta.dev() != base.dev {
                    return Err(RootState::Refused(Refusal::OtherMount(cur)));
                }
                let git = cur.join(".git");
                if std::fs::symlink_metadata(&git).is_ok() {
                    return Err(RootState::Refused(Refusal::NestedRepo(cur)));
                }
                if let Some(masters) = &base.masters {
                    if overlaps(&cur, masters) {
                        return Err(RootState::Refused(Refusal::OverlapsMasters(cur)));
                    }
                }
                let path = canonical_existing(&cur).map_err(|e| RootState::Refused(Refusal::Io(e.to_string())))?;
                return Ok(Resolved { root, path });
            }
            if !meta.is_dir() {
                return Err(RootState::Refused(Refusal::Io(format!(
                    "{} is not a directory",
                    cur.display()
                ))));
            }
            if meta.dev() != base.dev {
                return Err(RootState::Refused(Refusal::OtherMount(cur)));
            }
        }
        Err(RootState::Absent)
    }
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}

fn canonical_existing(path: &Path) -> io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

#[cfg(unix)]
fn observe_path(path: &Path) -> Option<Observed> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).ok()?;
    let regular = meta.file_type().is_file();
    Some(Observed {
        regular,
        nlink: meta.nlink(),
        id: FileId {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime_ns: (meta.mtime() as i128) * 1_000_000_000 + (meta.mtime_nsec() as i128),
        },
    })
}

#[cfg(not(unix))]
fn observe_path(_path: &Path) -> Option<Observed> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    const T0_SECS: u64 = 1_800_000_000;

    fn t0() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(T0_SECS)
    }

    fn temp_repo(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "funkot-owned-wav-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(p.join("testdata/synth")).unwrap();
        fs::create_dir_all(p.join("testdata/phase_ab")).unwrap();
        fs::create_dir_all(p.join("funkot-core/tests/fixtures")).unwrap();
        p
    }

    fn write_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
        fs::write(path, bytes)
    }

    #[test]
    fn owned_expired_is_deleted() {
        let repo = temp_repo("expired");
        let co = Checkout::at(&repo);
        let dir = repo.join("testdata/synth");
        let name = "a.wav";
        write_owned(&co, &dir, name, t0(), |p| write_bytes(p, b"RIFF-wav")).unwrap();
        assert!(dir.join(name).is_file());
        assert!(dir.join(sidecar_name(name)).is_file());

        let report = sweep(&co, t0() + RECLAIM_AFTER + Duration::from_secs(1));
        assert_eq!(report.reclaimed(), vec!["a.wav"]);
        assert!(!dir.join(name).exists());
        assert!(!dir.join(sidecar_name(name)).exists());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn owned_in_window_is_kept() {
        let repo = temp_repo("inwindow");
        let co = Checkout::at(&repo);
        let dir = repo.join("testdata/synth");
        write_owned(&co, &dir, "b.wav", t0(), |p| write_bytes(p, b"wav")).unwrap();
        let report = sweep(&co, t0() + RECLAIM_AFTER - Duration::from_secs(1));
        assert!(report.reclaimed().is_empty());
        let RootReport::Swept(s) = &report.roots.iter().find(|(r, _)| *r == Root::Synth).unwrap().1
        else {
            panic!("expected swept");
        };
        assert_eq!(s.kept, vec![("b.wav".into(), KeepReason::InWindow)]);
        assert!(dir.join("b.wav").is_file());
        assert!(dir.join(sidecar_name("b.wav")).is_file());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn active_writer_survives_because_disowned() {
        let repo = temp_repo("active");
        let co = Checkout::at(&repo);
        let dir = repo.join("testdata/synth");
        let name = "live.wav";
        // Pre-create an old claim.
        write_owned(&co, &dir, name, t0(), |p| write_bytes(p, b"old")).unwrap();
        let far = t0() + Duration::from_secs(100 * 24 * 60 * 60);
        let ((), claimed) = write_owned(&co, &dir, name, t0(), |p| {
            let mid = sweep(&co, far);
            assert!(
                mid.reclaimed().is_empty(),
                "in-progress write must not be reclaimed: {:?}",
                mid.reclaimed()
            );
            write_bytes(p, b"new-content")
        })
        .unwrap();
        assert!(matches!(claimed, Claimed::Yes { .. }));
        assert_eq!(fs::read(dir.join(name)).unwrap(), b"new-content");
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn rewritten_after_claim_keeps_wav_drops_sidecar() {
        let repo = temp_repo("rewrite");
        let co = Checkout::at(&repo);
        let dir = repo.join("testdata/synth");
        write_owned(&co, &dir, "c.wav", t0(), |p| write_bytes(p, b"orig")).unwrap();
        {
            use std::io::Write;
            let mut f = fs::OpenOptions::new()
                .append(true)
                .open(dir.join("c.wav"))
                .unwrap();
            f.write_all(b"X").unwrap();
        }
        let report = sweep(&co, t0() + RECLAIM_AFTER + Duration::from_secs(1));
        assert!(report.reclaimed().is_empty());
        let RootReport::Swept(s) = &report.roots.iter().find(|(r, _)| *r == Root::Synth).unwrap().1
        else {
            panic!("expected swept");
        };
        assert_eq!(s.kept, vec![("c.wav".into(), KeepReason::Mismatch)]);
        assert!(dir.join("c.wav").is_file());
        assert!(!dir.join(sidecar_name("c.wav")).exists());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn explicit_keep_by_deleting_sidecar() {
        let repo = temp_repo("explicit");
        let co = Checkout::at(&repo);
        let dir = repo.join("testdata/synth");
        write_owned(&co, &dir, "keep.wav", t0(), |p| write_bytes(p, b"keepme")).unwrap();
        fs::remove_file(dir.join(sidecar_name("keep.wav"))).unwrap();
        let report = sweep(&co, t0() + RECLAIM_AFTER + Duration::from_secs(1));
        assert!(report.reclaimed().is_empty());
        let RootReport::Swept(s) = &report.roots.iter().find(|(r, _)| *r == Root::Synth).unwrap().1
        else {
            panic!("expected swept");
        };
        assert_eq!(s.unknown_wavs, 1);
        assert_eq!(fs::read(dir.join("keep.wav")).unwrap(), b"keepme");
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn unknown_legacy_wav_is_kept() {
        let repo = temp_repo("legacy");
        let co = Checkout::at(&repo);
        let dir = repo.join("testdata/synth");
        fs::write(dir.join("old.wav"), b"legacy").unwrap();
        let report = sweep(&co, t0() + RECLAIM_AFTER + Duration::from_secs(1));
        let RootReport::Swept(s) = &report.roots.iter().find(|(r, _)| *r == Root::Synth).unwrap().1
        else {
            panic!("expected swept");
        };
        assert_eq!(s.unknown_wavs, 1);
        assert_eq!(s.unknown_bytes, 6);
        assert!(dir.join("old.wav").is_file());
        let _ = fs::remove_dir_all(&repo);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_root_is_refused() {
        let repo = temp_repo("symlink-root");
        let elsewhere = repo.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("target.wav"), b"safe").unwrap();
        let synth = repo.join("testdata/synth");
        fs::remove_dir_all(&synth).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &synth).unwrap();

        let co = Checkout::at(&repo);
        let ((), claimed) =
            write_owned(&co, &synth, "x.wav", t0(), |p| write_bytes(p, b"nope")).unwrap();
        assert!(matches!(
            claimed,
            Claimed::No(NotClaimed::RootRefused(ref s)) if s == "symlink"
        ));
        assert!(!elsewhere.join(sidecar_name("x.wav")).exists());
        assert_eq!(fs::read(elsewhere.join("target.wav")).unwrap(), b"safe");

        let report = sweep(&co, t0());
        let rr = &report.roots.iter().find(|(r, _)| *r == Root::Synth).unwrap().1;
        assert!(matches!(rr, RootReport::Refused(Refusal::Symlink(_))));
        let _ = fs::remove_dir_all(&repo);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_wav_at_claimed_name_is_kept_not_regular() {
        let repo = temp_repo("symlink-wav");
        let co = Checkout::at(&repo);
        let dir = repo.join("testdata/synth");
        write_owned(&co, &dir, "s.wav", t0(), |p| write_bytes(p, b"real")).unwrap();
        let target = dir.join("other.bin");
        fs::write(&target, b"tgt").unwrap();
        fs::remove_file(dir.join("s.wav")).unwrap();
        std::os::unix::fs::symlink(&target, dir.join("s.wav")).unwrap();

        let report = sweep(&co, t0() + RECLAIM_AFTER + Duration::from_secs(1));
        let RootReport::Swept(s) = &report.roots.iter().find(|(r, _)| *r == Root::Synth).unwrap().1
        else {
            panic!("expected swept");
        };
        assert_eq!(s.kept, vec![("s.wav".into(), KeepReason::NotRegular)]);
        assert!(dir.join("s.wav").exists());
        assert!(dir.join(sidecar_name("s.wav")).is_file());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn nested_repo_is_refused() {
        let repo = temp_repo("nested");
        let phase = repo.join("testdata/phase_ab");
        fs::create_dir_all(phase.join(".git")).unwrap();
        let co = Checkout::at(&repo);
        let report = sweep(&co, t0());
        let rr = &report
            .roots
            .iter()
            .find(|(r, _)| *r == Root::PhaseAb)
            .unwrap()
            .1;
        assert!(matches!(rr, RootReport::Refused(Refusal::NestedRepo(_))));
        let ((), claimed) =
            write_owned(&co, &phase, "n.wav", t0(), |p| write_bytes(p, b"x")).unwrap();
        assert!(matches!(
            claimed,
            Claimed::No(NotClaimed::RootRefused(ref s)) if s == "nested-repo"
        ));
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn masters_overlap_is_refused() {
        let repo = temp_repo("masters");
        let synth = fs::canonicalize(repo.join("testdata/synth")).unwrap();
        let co = Checkout::at_inner(&repo, Some(synth.clone()));
        let report = sweep(&co, t0());
        let rr = &report.roots.iter().find(|(r, _)| *r == Root::Synth).unwrap().1;
        assert!(matches!(rr, RootReport::Refused(Refusal::OverlapsMasters(_))));
        let ((), claimed) =
            write_owned(&co, &synth, "m.wav", t0(), |p| write_bytes(p, b"x")).unwrap();
        assert!(matches!(
            claimed,
            Claimed::No(NotClaimed::RootRefused(ref s)) if s == "overlaps-masters"
        ));
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn other_mount_is_refused_via_injected_dev() {
        let repo = temp_repo("mount");
        let mut co = Checkout::at(&repo);
        if let Ok(base) = &mut co.base {
            base.dev = base.dev.wrapping_add(1);
        }
        let report = sweep(&co, t0());
        let rr = &report.roots.iter().find(|(r, _)| *r == Root::Synth).unwrap().1;
        assert!(matches!(rr, RootReport::Refused(Refusal::OtherMount(_))));
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn sidecar_moved_to_another_root_is_wrong_root() {
        let repo = temp_repo("wrongroot");
        let co = Checkout::at(&repo);
        let synth = repo.join("testdata/synth");
        let phase = repo.join("testdata/phase_ab");
        write_owned(&co, &synth, "w.wav", t0(), |p| write_bytes(p, b"body")).unwrap();
        fs::copy(synth.join("w.wav"), phase.join("w.wav")).unwrap();
        fs::copy(
            synth.join(sidecar_name("w.wav")),
            phase.join(sidecar_name("w.wav")),
        )
        .unwrap();
        let report = sweep(&co, t0() + RECLAIM_AFTER + Duration::from_secs(1));
        // synth copy expired → reclaimed
        assert!(report.reclaimed().contains(&"w.wav"));
        let RootReport::Swept(phase_s) = &report
            .roots
            .iter()
            .find(|(r, _)| *r == Root::PhaseAb)
            .unwrap()
            .1
        else {
            panic!("expected swept");
        };
        assert_eq!(
            phase_s.kept,
            vec![("w.wav".into(), KeepReason::WrongRoot)]
        );
        assert!(phase.join("w.wav").is_file());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn protected_neighbours_unchanged() {
        let repo = temp_repo("neighbours");
        let fixtures = repo.join("funkot-core/tests/fixtures");
        let golden = fixtures.join("golden.json");
        let readme = fixtures.join("README.md");
        fs::write(&golden, b"{\"v\":1}").unwrap();
        fs::write(&readme, b"readme").unwrap();
        fs::create_dir_all(repo.join("funkot-core/tests/data")).unwrap();
        let data_wav = repo.join("funkot-core/tests/data/x.wav");
        fs::write(&data_wav, b"datawav").unwrap();
        fs::create_dir_all(repo.join("testdata")).unwrap();
        let labels = repo.join("testdata/labels.tsv.example");
        fs::write(&labels, b"labels").unwrap();

        let co = Checkout::at(&repo);
        write_owned(&co, &fixtures, "fx.wav", t0(), |p| write_bytes(p, b"fx")).unwrap();
        let _ = sweep(&co, t0() + RECLAIM_AFTER + Duration::from_secs(1));

        assert_eq!(fs::read(&golden).unwrap(), b"{\"v\":1}");
        assert_eq!(fs::read(&readme).unwrap(), b"readme");
        assert_eq!(fs::read(&data_wav).unwrap(), b"datawav");
        assert_eq!(fs::read(&labels).unwrap(), b"labels");
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn outside_roots_never_claimed() {
        let repo = temp_repo("outside");
        let co = Checkout::at(&repo);
        let elsewhere = repo.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let ((), claimed) =
            write_owned(&co, &elsewhere, "o.wav", t0(), |p| write_bytes(p, b"out")).unwrap();
        assert_eq!(claimed, Claimed::No(NotClaimed::OutsideRoots));
        assert!(!elsewhere.join(sidecar_name("o.wav")).exists());
        assert!(elsewhere.join("o.wav").is_file());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn crash_between_unlinks_drops_orphan_then_noop() {
        let repo = temp_repo("orphan");
        let co = Checkout::at(&repo);
        let dir = repo.join("testdata/synth");
        write_owned(&co, &dir, "or.wav", t0(), |p| write_bytes(p, b"x")).unwrap();
        fs::remove_file(dir.join("or.wav")).unwrap();
        let report = sweep(&co, t0() + RECLAIM_AFTER + Duration::from_secs(1));
        let RootReport::Swept(s) = &report.roots.iter().find(|(r, _)| *r == Root::Synth).unwrap().1
        else {
            panic!("expected swept");
        };
        assert_eq!(s.orphan_claims_dropped, 1);
        assert!(!dir.join(sidecar_name("or.wav")).exists());
        let report2 = sweep(&co, t0() + RECLAIM_AFTER + Duration::from_secs(1));
        assert!(report2.is_noop());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn second_sweep_is_noop() {
        let repo = temp_repo("noop");
        let co = Checkout::at(&repo);
        let dir = repo.join("testdata/synth");
        write_owned(&co, &dir, "n.wav", t0(), |p| write_bytes(p, b"x")).unwrap();
        let now = t0() + RECLAIM_AFTER + Duration::from_secs(1);
        let _ = sweep(&co, now);
        let report2 = sweep(&co, now);
        assert!(report2.is_noop());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn garbage_sidecar_is_not_a_claim() {
        let repo = temp_repo("garbage");
        let co = Checkout::at(&repo);
        let dir = repo.join("testdata/synth");
        fs::write(dir.join("g.wav"), b"garb").unwrap();
        fs::write(dir.join(sidecar_name("g.wav")), "hello").unwrap();
        let report = sweep(&co, t0() + RECLAIM_AFTER + Duration::from_secs(1));
        let RootReport::Swept(s) = &report.roots.iter().find(|(r, _)| *r == Root::Synth).unwrap().1
        else {
            panic!("expected swept");
        };
        assert_eq!(s.unknown_wavs, 1);
        assert!(dir.join("g.wav").is_file());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn decide_matrix_literal() {
        let id = FileId {
            dev: 1,
            ino: 2,
            len: 3,
            mtime_ns: 4,
        };
        let claim = Claim {
            root: Root::Synth,
            file: "a.wav".into(),
            claimed: t0(),
            id,
        };
        let obs = Observed {
            regular: true,
            nlink: 1,
            id,
        };
        assert_eq!(
            decide(&claim, Root::Synth, Some(obs), t0() + RECLAIM_AFTER - Duration::from_secs(1)),
            Verdict::Keep(KeepReason::InWindow)
        );
        assert_eq!(
            decide(&claim, Root::Synth, Some(obs), t0() + RECLAIM_AFTER + Duration::from_secs(1)),
            Verdict::Reclaim
        );
        assert_eq!(
            decide(&claim, Root::PhaseAb, Some(obs), t0() + RECLAIM_AFTER + Duration::from_secs(1)),
            Verdict::Keep(KeepReason::WrongRoot)
        );
        assert_eq!(
            decide(&claim, Root::Synth, None, t0()),
            Verdict::DropOrphanClaim
        );
        assert_eq!(
            decide(
                &claim,
                Root::Synth,
                Some(Observed {
                    regular: false,
                    nlink: 1,
                    id
                }),
                t0()
            ),
            Verdict::Keep(KeepReason::NotRegular)
        );
        let mut bad = id;
        bad.len = 99;
        assert_eq!(
            decide(
                &claim,
                Root::Synth,
                Some(Observed {
                    regular: true,
                    nlink: 1,
                    id: bad
                }),
                t0()
            ),
            Verdict::Keep(KeepReason::Mismatch)
        );
        assert_eq!(
            decide(
                &claim,
                Root::Synth,
                Some(Observed {
                    regular: true,
                    nlink: 2,
                    id
                }),
                t0()
            ),
            Verdict::Keep(KeepReason::Hardlinked)
        );
        assert_eq!(
            decide(&claim, Root::Synth, Some(obs), t0() - Duration::from_secs(1)),
            Verdict::Keep(KeepReason::ClockBehind)
        );
    }

    #[test]
    fn open_handle_sweeps_then_write_owned() {
        let repo = temp_repo("open");
        let co = Checkout::at(&repo);
        let dir = repo.join("testdata/synth");
        write_owned(&co, &dir, "old.wav", t0(), |p| write_bytes(p, b"old")).unwrap();
        let far = t0() + RECLAIM_AFTER + Duration::from_secs(1);
        let opened = co.open(far);
        assert!(!dir.join("old.wav").exists());
        let ((), claimed) = opened
            .write_owned(&dir, "new.wav", far, |p| write_bytes(p, b"new"))
            .unwrap();
        assert!(matches!(claimed, Claimed::Yes { root: Root::Synth }));
        assert!(dir.join(sidecar_name("new.wav")).is_file());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn reclaim_after_is_eight_days() {
        assert_eq!(RECLAIM_AFTER, Duration::from_secs(8 * 24 * 60 * 60));
    }
}
