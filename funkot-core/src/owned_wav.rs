//! Durable exact-generation WAV ownership. Rendering never implies acceptance.
//! New output is claimed before any bytes; only acceptance plus last-use release
//! can make it pending. Legacy, manual, held, modified and linked files survive.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Root { Synth, PhaseAb, Fixtures }
impl Root {
    pub const ALL: [Self; 3] = [Self::Synth, Self::PhaseAb, Self::Fixtures];
    fn rel(self) -> &'static str { match self { Self::Synth => "testdata/synth", Self::PhaseAb => "testdata/phase_ab", Self::Fixtures => "funkot-core/tests/fixtures" } }
}
pub struct Checkout { repo: PathBuf }
pub struct Opened<'a> { checkout: &'a Checkout }
#[derive(Debug, Clone)]
pub enum Claimed { Yes { generation: String, receipt: PathBuf }, No(String) }
impl fmt::Display for Claimed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self { Self::Yes { generation, receipt } => write!(f, "held generation={generation} receipt={}", receipt.display()), Self::No(why) => write!(f, "unclaimed ({why})") }
    }
}
impl Checkout {
    pub fn this() -> Self { Self::at(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")) }
    pub fn at(repo: &Path) -> Self { Self { repo: fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf()) } }
    pub fn open(&self, _now: SystemTime) -> Opened<'_> {
        for root in Root::ALL { retry_directory(&self.repo.join(root.rel()), &self.repo); }
        if let Ok(Some(context)) = context() { retry_directory(&context.owner_receipt_dir, &self.repo); }
        Opened { checkout: self }
    }
}
impl Opened<'_> {
    pub fn write_owned<T, E>(&self, dir: &Path, name: &str, now: SystemTime, write: impl FnOnce(&Path) -> Result<T, E>) -> Result<(T, Claimed), E>
    where E: From<io::Error> { write_owned(self.checkout, dir, name, now, write) }
}
pub fn write_owned<T, E>(checkout: &Checkout, dir: &Path, name: &str, _now: SystemTime, write: impl FnOnce(&Path) -> Result<T, E>) -> Result<(T, Claimed), E>
where E: From<io::Error> {
    valid_name(name).map_err(E::from)?;
    let mut owned = Generation::begin_at(&dir.join(name), &checkout.repo).map_err(E::from)?;
    if owned.owned.is_none() {
        return Err(invalid(&format!("owned generator refused protected output: {}", owned.reason)).into());
    }
    let value = write(owned.write_path())?;
    Ok((value, owned.finish().map_err(E::from)?))
}
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Identity { dev: u64, ino: u64, len: u64, mtime: i64, mtime_ns: i64 }
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct Claim {
    version: u32, owner: String, generation: String, output: PathBuf, receipt: PathBuf,
    directory_dev: u64, directory_ino: u64, state: String, hold: bool,
    source_revision: String, inputs: Vec<String>, identity: Option<Identity>,
    sha256: Option<String>, allocated_bytes: Option<u64>, accepted_proof: Option<String>,
    released_proof: Option<String>, result: Option<String>,
}
#[derive(Deserialize)]
struct Context {
    owner_receipt_dir: PathBuf,
    owner_receipt_argv: Vec<String>,
    owner_completion_argv: Option<Vec<String>>,
}
fn context() -> io::Result<Option<Context>> {
    let Some(value) = std::env::var_os("WORKSPACE_LIFECYCLE_CONTEXT") else { return Ok(None) };
    let value: serde_json::Value = serde_json::from_str(&value.to_string_lossy()).map_err(|e| invalid(&format!("invalid lifecycle context: {e}")))?;
    // Older context versions advertise no owner integration.
    if value.get("owner_receipt_dir").is_none() { return Ok(None) }
    serde_json::from_value(value).map(Some).map_err(|e| invalid(&format!("invalid owner context: {e}")))
}
/// Holds the cooperative owner lock and the exact output inode for its lifetime.
/// Abrupt termination leaves durable held/writing evidence, never an aged lease.
pub struct Generation { write_path: PathBuf, owned: Option<Owned>, reason: String, _unclaimed_lock: Vec<File> }
struct Owned { dir: Directory, receipts: Directory, _lock: File, file: File, side: String, claim: Claim }
impl Generation {
    pub fn begin(output: &Path) -> io::Result<Self> { Self::begin_at(output, &Checkout::this().repo) }
    fn unclaimed(path: &Path, reason: String) -> Self { Self { write_path: path.into(), owned: None, reason, _unclaimed_lock: Vec::new() } }
    fn begin_at(output: &Path, repo: &Path) -> io::Result<Self> {
        Self::begin_with_context(output, repo, context()?)
    }
    fn begin_with_context(output: &Path, repo: &Path, context: Option<Context>) -> io::Result<Self> {
        #[cfg(not(target_os = "linux"))]
        { let _ = (repo, context); return Ok(Self::unclaimed(output, "native safe ownership unavailable; retained".into())); }
        #[cfg(target_os = "linux")]
        {
            // Nested stream writer within write_owned already has an owned inode.
            if output.starts_with("/proc/self/fd") { return Ok(Self::unclaimed(output, "parent generation".into())); }
            let absolute = absolute(output)?;
            let name = file_name(&absolute)?.to_owned();
            let dir = match Directory::open(absolute.parent().unwrap(), repo) {
                Ok(dir) => dir,
                Err(e) if context.is_some() => return Err(e),
                Err(e) => return Ok(Self::unclaimed(output, format!("protected directory: {e}"))),
            };
            let receipt_dir = context.as_ref().map(|c| c.owner_receipt_dir.as_path()).unwrap_or(&dir.path);
            let receipts = Directory::open(receipt_dir, repo)?;
            retry_open_directory(&receipts, repo);
            let lock = receipts.lock(&absolute)?;
            // Never adopt pre-existing outputs or old/unknown receipts.
            if dir.exists(&name)? || dir.exists(&format!(".{name}.owned"))? {
                if context.is_some() { return Err(invalid("managed output or receipt already exists; retained")); }
                let mut result = Self::unclaimed(output, "pre-existing output or receipt".into());
                result._unclaimed_lock.push(lock);
                // Also coordinate across different task receipt directories.
                if let Ok(existing) = dir.read_regular(&name) {
                    existing.try_lock().map_err(|e| io::Error::other(format!("output busy: {e}")))?;
                    result._unclaimed_lock.push(existing);
                }
                return Ok(result);
            }
            let generation = generation_id();
            let side = if context.is_some() { format!(".{generation}.owned") } else { format!(".{name}.owned") };
            let mut claim = Claim {
                version: 3, owner: "funkot-wav".into(), generation, output: absolute,
                receipt: receipts.path.join(&side), directory_dev: dir.dev, directory_ino: dir.ino,
                state: "writing".into(), hold: false, source_revision: source_revision(repo)?,
                inputs: std::env::args().collect(), identity: None, sha256: None, allocated_bytes: None,
                accepted_proof: None, released_proof: None, result: None,
            };
            receipts.persist(&side, &claim, true)?;
            if let Some(context) = context { register_task_receipt(&context, &claim)?; }
            // Exclusive creation cannot adopt a file raced in after registration.
            let file = nofollow().write(true).read(true).create_new(true).open(dir.child(&name))?;
            file.try_lock().map_err(|e| io::Error::other(format!("output busy: {e}")))?;
            file.sync_all()?; dir.sync()?;
            claim.identity = Some(identity(&file.metadata()?));
            receipts.persist(&side, &claim, false)?;
            use std::os::fd::AsRawFd;
            let write_path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
            Ok(Self { write_path, owned: Some(Owned { dir, receipts, _lock: lock, file, side, claim }), reason: String::new(), _unclaimed_lock: Vec::new() })
        }
    }
    pub fn write_path(&self) -> &Path { &self.write_path }
    pub fn finish(&mut self) -> io::Result<Claimed> {
        let Some(o) = self.owned.as_mut() else {
            let result = Claimed::No(self.reason.clone());
            if self.reason != "parent generation" { eprintln!("owned_wav: {result}"); }
            return Ok(result);
        };
        if o.claim.state != "writing" { return Err(invalid("generation already finalized; evidence is immutable")); }
        o.file.sync_all()?; o.dir.revalidate()?;
        let meta = o.file.metadata()?;
        if !single_regular(&meta) || !same_inode(&meta, o.claim.identity.as_ref().unwrap()) { return Err(invalid("output identity changed during write")); }
        let file = o.dir.read_regular(file_name(&o.claim.output)?)?;
        if identity(&file.metadata()?) != identity(&meta) { return Err(invalid("output replaced during write")); }
        let hash = hash_file(&file)?;
        if identity(&file.metadata()?) != identity(&meta) { return Err(invalid("output changed while hashing")); }
        o.claim.identity = Some(identity(&meta)); o.claim.sha256 = Some(hash);
        o.claim.allocated_bytes = Some(allocated(&meta)); o.claim.state = "held".into();
        o.claim.result = Some("generated; awaiting acceptance and last-use release".into());
        o.receipts.persist(&o.side, &o.claim, false)?;
        let result = Claimed::Yes { generation: o.claim.generation.clone(), receipt: o.claim.receipt.clone() };
        eprintln!("owned_wav: {result}");
        Ok(result)
    }
}
/// Proofs become durable before deletion. Repeating completion is idempotent.
pub fn complete(output: &Path, generation: &str, receipt: Option<&Path>, accepted: &str, released: &str) -> io::Result<PathBuf> {
    if accepted.trim().is_empty() || released.trim().is_empty() { return Err(invalid("acceptance and last-use release proofs are required")); }
    let output = absolute(output)?;
    let local_receipt = output.with_file_name(format!(".{}.owned", file_name(&output)?));
    let receipt = absolute(receipt.unwrap_or(&local_receipt))?;
    let repo = Checkout::this().repo;
    let receipts = Directory::open(receipt.parent().unwrap(), &repo)?;
    let _lock = receipts.lock(&output)?;
    let side = file_name(&receipt)?;
    let mut claim = receipts.read_claim(side)?;
    if claim.version != 3 || claim.owner != "funkot-wav" || claim.output != output || claim.receipt != receipt || claim.generation != generation { return Err(invalid("generation does not match receipt")); }
    if (claim.state == "pending" || claim.state == "reclaimed")
        && (claim.accepted_proof.as_deref() != Some(accepted) || claim.released_proof.as_deref() != Some(released)) {
        return Err(invalid("completion proof differs from durable acceptance or last-use release"));
    }
    if claim.state == "reclaimed" {
        match fs::symlink_metadata(&output) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(receipt),
            _ => return Err(invalid("new output exists at a reclaimed path; retained")),
        }
    }
    if claim.hold { return Err(invalid("explicit hold remains; completion refused")); }
    let dir = Directory::open(output.parent().unwrap(), &repo)?;
    validate_claim(&dir, &receipts, side, &claim)?;
    if claim.state != "held" && claim.state != "pending" { return Err(invalid("incomplete generation cannot be accepted")); }
    if claim.sha256.is_none() || claim.identity.is_none() || !valid_provenance(&claim) { return Err(invalid("missing final output evidence")); }
    if claim.state == "held" {
        claim.accepted_proof = Some(accepted.into()); claim.released_proof = Some(released.into());
        claim.hold = false; claim.state = "pending".into();
        receipts.persist(side, &claim, false)?;
    }
    reclaim(&dir, &receipts, side, &mut claim)?;
    Ok(receipt)
}
fn validate_claim(dir: &Directory, receipts: &Directory, side: &str, c: &Claim) -> io::Result<()> {
    file_name(&c.output)?;
    if c.version != 3 || c.owner != "funkot-wav" || c.output.parent() != Some(dir.path.as_path())
        || c.receipt != receipts.path.join(side) || c.directory_dev != dir.dev || c.directory_ino != dir.ino
        || !c.generation.starts_with('g') || !c.generation.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(invalid("foreign, legacy or moved receipt"));
    }
    Ok(())
}
fn retry_directory(path: &Path, repo: &Path) { if let Ok(d) = Directory::open(path, repo) { retry_open_directory(&d, repo); } }
fn retry_open_directory(receipts: &Directory, repo: &Path) {
    let Ok(entries) = fs::read_dir(receipts.pinned()) else { return };
    for entry in entries.flatten() {
        let Some(side) = entry.file_name().to_str().map(str::to_owned) else { continue };
        if !side.starts_with('.') || !side.ends_with(".owned") { continue }
        let Ok(claim) = receipts.read_claim(&side) else { continue };
        if claim.state != "pending" { continue }
        let Ok(_lock) = receipts.lock(&claim.output) else { continue };
        let Ok(mut claim) = receipts.read_claim(&side) else { continue };
        let Some(parent) = claim.output.parent() else { continue };
        let Ok(dir) = Directory::open(parent, repo) else { continue };
        if let Err(e) = reclaim(&dir, receipts, &side, &mut claim) { eprintln!("owned_wav: pending {}: {e}", claim.generation); }
    }
}
fn reclaim(dir: &Directory, receipts: &Directory, side: &str, c: &mut Claim) -> io::Result<()> {
    validate_claim(dir, receipts, side, c)?;
    if !valid_provenance(c) || c.identity.is_none() || c.allocated_bytes.is_none()
        || c.sha256.as_deref().is_none_or(|hash| hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Err(invalid("missing final output evidence or reproducible generation provenance"));
    }
    if c.state != "pending" || c.hold || c.accepted_proof.as_deref().is_none_or(|s| s.trim().is_empty()) || c.released_proof.as_deref().is_none_or(|s| s.trim().is_empty()) { return Err(invalid("not accepted and released")); }
    dir.revalidate()?;
    let name = file_name(&c.output)?;
    let tomb = format!(".{name}.{}.reclaiming", c.generation);
    let output_lock;
    if !dir.exists(&tomb)? {
        if !dir.exists(name)? {
            c.state = "reclaimed".into(); c.result = Some("absent on pending retry; no additional bytes removed".into());
            return receipts.persist(side, c, false);
        }
        output_lock = verify_output(dir, name, c)?;
        output_lock.try_lock().map_err(|e| io::Error::other(format!("output busy: {e}")))?;
        rename_exclusive(&dir.child(name), &dir.child(&tomb))?; dir.sync()?;
    } else {
        output_lock = verify_output(dir, &tomb, c)?;
        output_lock.try_lock().map_err(|e| io::Error::other(format!("output busy: {e}")))?;
    }
    // Check again after rename. Never overwrite a raced-in new output to restore
    // a mismatch; preserve it and the quarantined inode for explicit resolution.
    verify_output(dir, &tomb, c)?; dir.revalidate()?;
    if dir.exists(name)? { return Err(invalid("new output at reclaimed name; retained")); }
    fs::remove_file(dir.child(&tomb))?; dir.sync()?;
    if dir.exists(name)? { return Err(invalid("new output appeared during reclamation; retained")); }
    c.state = "reclaimed".into(); c.result = Some(format!("removed; allocated_bytes={}", c.allocated_bytes.unwrap_or(0)));
    receipts.persist(side, c, false)
}
fn verify_output(dir: &Directory, name: &str, c: &Claim) -> io::Result<File> {
    let file = dir.read_regular(name)?; let before = file.metadata()?;
    if c.identity.as_ref() != Some(&identity(&before)) || c.sha256.as_deref() != Some(hash_file(&file)?.as_str()) || identity(&file.metadata()?) != identity(&before) { return Err(invalid("identity or hash changed; retained")); }
    Ok(file)
}
struct Directory { path: PathBuf, file: File, dev: u64, ino: u64, mount_id: u64, repo: PathBuf }
impl Directory {
    fn open(path: &Path, repo: &Path) -> io::Result<Self> {
        #[cfg(not(target_os = "linux"))]
        { let _ = (path, repo); return Err(invalid("native safe ownership unavailable; retained")); }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            let path = absolute(path)?;
            let repo = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
            if let Some(masters) = std::env::var_os("FUNKOT_TESTDATA_DIR").filter(|v| !v.is_empty()) {
                let masters = fs::canonicalize(masters)?;
                if overlaps(&path, &masters) { return Err(invalid("overlaps masters")); }
            }
            let mut walk = PathBuf::from("/"); let mut prior_dev = None;
            let mut fd = OpenOptions::new().read(true).custom_flags(0x20000 | 0x10000).open("/")?;
            for part in path.components().skip(1) {
                let Component::Normal(part) = part else { return Err(invalid("non-normal path")); };
                walk.push(part);
                use std::os::fd::AsRawFd;
                let child = PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd())).join(part);
                let f = OpenOptions::new().read(true).custom_flags(0x20000 | 0x10000).open(child)?; // O_NOFOLLOW | O_DIRECTORY
                let meta = f.metadata()?;
                if walk.starts_with(&repo) && walk != repo {
                    if prior_dev.is_some_and(|d| d != meta.dev()) || is_mountpoint(&walk)? { return Err(invalid("other mount")); }
                    if fs::symlink_metadata(walk.join(".git")).is_ok() { return Err(invalid("nested repository")); }
                }
                prior_dev = Some(meta.dev()); fd = f;
            }
            if path == Path::new("/") { return Err(invalid("root directory cannot own output")); }
            let file = fd;
            let meta = file.metadata()?;
            let mount_id = mount_id(&file)?;
            Ok(Self { path, file, dev: meta.dev(), ino: meta.ino(), mount_id, repo })
        }
    }
    fn pinned(&self) -> PathBuf {
        #[cfg(target_os = "linux")]
        { use std::os::fd::AsRawFd; PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd())) }
        #[cfg(not(target_os = "linux"))] { self.path.clone() }
    }
    fn child(&self, name: &str) -> PathBuf { self.pinned().join(name) }
    fn sync(&self) -> io::Result<()> { self.file.sync_all() }
    fn revalidate(&self) -> io::Result<()> {
        let current = Self::open(&self.path, &self.repo)?;
        if current.dev != self.dev || current.ino != self.ino || current.mount_id != self.mount_id { return Err(invalid("owner directory replaced")); } Ok(())
    }
    fn exists(&self, name: &str) -> io::Result<bool> {
        match fs::symlink_metadata(self.child(name)) { Ok(_) => Ok(true), Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false), Err(e) => Err(e) }
    }
    fn read_regular(&self, name: &str) -> io::Result<File> {
        let file = nofollow().read(true).open(self.child(name))?;
        if !single_regular(&file.metadata()?) { return Err(invalid("not a single-link regular file")); } Ok(file)
    }
    fn lock(&self, output: &Path) -> io::Result<File> {
        let name = format!(".funkot-wav-{:x}.lock", Sha256::digest(output.as_os_str().as_encoded_bytes()));
        let file = nofollow().read(true).write(true).create(true).truncate(false).open(self.child(&name))?;
        if !single_regular(&file.metadata()?) { return Err(invalid("invalid owner lock")); }
        file.try_lock().map_err(|e| io::Error::other(format!("owner busy: {e}")))?; self.revalidate()?; Ok(file)
    }
    fn read_claim(&self, side: &str) -> io::Result<Claim> { serde_json::from_reader(self.read_regular(side)?).map_err(|e| invalid(&format!("invalid receipt: {e}"))) }
    fn persist(&self, side: &str, claim: &Claim, create: bool) -> io::Result<()> {
        self.revalidate()?;
        let tmp = format!("{side}.{}.tmp", generation_id());
        let mut f = nofollow().write(true).create_new(true).open(self.child(&tmp))?;
        serde_json::to_writer_pretty(&mut f, claim)?; f.write_all(b"\n")?; f.sync_all()?;
        if create { rename_exclusive(&self.child(&tmp), &self.child(side))?; }
        else { self.read_regular(side)?; fs::rename(self.child(&tmp), self.child(side))?; }
        self.sync()
    }
}
fn nofollow() -> OpenOptions {
    let mut o = OpenOptions::new();
    #[cfg(target_os = "linux")] { use std::os::unix::fs::OpenOptionsExt; o.custom_flags(0x20000); } o
}
#[cfg(target_os = "linux")]
fn identity(m: &fs::Metadata) -> Identity { use std::os::unix::fs::MetadataExt; Identity { dev: m.dev(), ino: m.ino(), len: m.len(), mtime: m.mtime(), mtime_ns: m.mtime_nsec() } }
#[cfg(not(target_os = "linux"))]
fn identity(_m: &fs::Metadata) -> Identity { Identity { dev: 0, ino: 0, len: 0, mtime: 0, mtime_ns: 0 } }
fn same_inode(m: &fs::Metadata, id: &Identity) -> bool { let now = identity(m); now.dev == id.dev && now.ino == id.ino }
fn single_regular(m: &fs::Metadata) -> bool {
    #[cfg(target_os = "linux")] { use std::os::unix::fs::MetadataExt; m.is_file() && m.nlink() == 1 }
    #[cfg(not(target_os = "linux"))] { let _ = m; false }
}
fn allocated(m: &fs::Metadata) -> u64 {
    #[cfg(target_os = "linux")] { use std::os::unix::fs::MetadataExt; m.blocks() * 512 }
    #[cfg(not(target_os = "linux"))] { let _ = m; 0 }
}
fn hash_file(file: &File) -> io::Result<String> {
    use std::io::{Seek, SeekFrom};
    let mut file = file.try_clone()?; file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new(); let mut buf = [0u8; 65536];
    loop { let n = file.read(&mut buf)?; if n == 0 { break } hash.update(&buf[..n]); } Ok(format!("{:x}", hash.finalize()))
}
fn absolute(path: &Path) -> io::Result<PathBuf> {
    let path = if path.is_absolute() { path.into() } else { std::env::current_dir()?.join(path) };
    let mut normal = PathBuf::new();
    for c in path.components() { match c { Component::CurDir => {}, Component::ParentDir => return Err(invalid("parent traversal is not an ownership path")), _ => normal.push(c) } } Ok(normal)
}
fn file_name(path: &Path) -> io::Result<&str> { let name = path.file_name().and_then(|s| s.to_str()).ok_or_else(|| invalid("invalid filename"))?; valid_name(name)?; Ok(name) }
fn valid_name(name: &str) -> io::Result<()> { if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') { Err(invalid("bad output name")) } else { Ok(()) } }
fn overlaps(a: &Path, b: &Path) -> bool { a.starts_with(b) || b.starts_with(a) }
fn invalid(s: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, s) }
fn generation_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!("g{}-{}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}
fn valid_provenance(claim: &Claim) -> bool {
    let digest = claim.source_revision.rsplit("executable-sha256:").next().unwrap_or("");
    !claim.inputs.is_empty() && digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
        && claim.source_revision.contains("executable-sha256:")
}
fn source_revision(repo: &Path) -> io::Result<String> {
    // /proc/self/exe identifies the executable actually running, even if its
    // pathname was replaced after launch. Cache only within this process.
    static EXECUTABLE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let digest = match EXECUTABLE.get() {
        Some(digest) => digest.clone(),
        None => {
            let digest = hash_file(&File::open("/proc/self/exe")?)?;
            let _ = EXECUTABLE.set(digest.clone()); digest
        }
    };
    let explicit = std::env::var("FUNKOT_SOURCE_REVISION").ok();
    let git = || std::process::Command::new("git").args(["rev-parse", "HEAD"]).current_dir(repo).output().ok()
        .filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    let revision = explicit.or_else(git).filter(|r| matches!(r.len(), 40 | 64) && r.bytes().all(|b| b.is_ascii_hexdigit()));
    Ok(match revision {
        Some(revision) => format!("git:{revision}; executable-sha256:{digest}"),
        None => format!("executable-sha256:{digest}"),
    })
}
#[cfg(target_os = "linux")]
fn mount_id(file: &File) -> io::Result<u64> {
    use std::os::fd::AsRawFd;
    fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?.lines()
        .find_map(|line| line.strip_prefix("mnt_id:")).and_then(|id| id.trim().parse().ok())
        .ok_or_else(|| invalid("owner mount identity unavailable"))
}
#[cfg(target_os = "linux")]
fn is_mountpoint(path: &Path) -> io::Result<bool> {
    Ok(fs::read_to_string("/proc/self/mountinfo")?.lines().filter_map(|l| l.split_whitespace().nth(4)).any(|s| {
        let s = s.replace("\\040", " ").replace("\\011", "\t").replace("\\012", "\n").replace("\\134", "\\"); Path::new(&s) == path
    }))
}
fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt; use std::ffi::CString;
        unsafe extern "C" { fn renameat2(olddirfd: i32, oldpath: *const std::ffi::c_char, newdirfd: i32, newpath: *const std::ffi::c_char, flags: u32) -> i32; }
        let from = CString::new(from.as_os_str().as_bytes()).map_err(|_| invalid("NUL path"))?;
        let to = CString::new(to.as_os_str().as_bytes()).map_err(|_| invalid("NUL path"))?;
        // AT_FDCWD and RENAME_NOREPLACE; unsupported kernels fail closed.
        if unsafe { renameat2(-100, from.as_ptr(), -100, to.as_ptr(), 1) } != 0 { return Err(io::Error::last_os_error()); } Ok(())
    }
    #[cfg(not(target_os = "linux"))] { let _ = (from, to); Err(invalid("native safe rename unavailable")) }
}
fn register_task_receipt(context: &Context, claim: &Claim) -> io::Result<()> {
    let completion = match &context.owner_completion_argv {
        Some(argv) if !argv.is_empty() => argv.clone(),
        _ => {
            let exe = std::env::current_exe()?;
            if exe.file_stem().and_then(|s| s.to_str()) != Some("funkot-autodj") { return Err(invalid("managed example requires owner_completion_argv for the main CLI")); }
            vec![exe.to_string_lossy().into_owned()]
        }
    };
    let mut completion = completion;
    completion.extend(["--artifact-complete".into(), claim.output.to_string_lossy().into_owned(), "--generation".into(), claim.generation.clone(), "--artifact-receipt".into(), claim.receipt.to_string_lossy().into_owned(), "--accepted-proof".into(), "{result_ref}".into(), "--released-proof".into(), "{result_ref}".into()]);
    let (program, prefix) = context.owner_receipt_argv.split_first().ok_or_else(|| invalid("empty owner_receipt_argv"))?;
    let status = std::process::Command::new(program).args(prefix).args(["--owner", "funkot-wav", "--generation", &claim.generation, "--output"]).arg(&claim.output).arg("--receipt").arg(&claim.receipt).arg("--completion-json").arg(serde_json::to_string(&completion)?).status()?;
    if !status.success() { return Err(invalid("owner receipt registration failed; no output written")); } Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use tempfile::TempDir;
    fn fixture() -> (TempDir, Checkout, PathBuf) {
        let repo = tempfile::tempdir().unwrap();
        let dir = repo.path().join("testdata/synth"); fs::create_dir_all(&dir).unwrap();
        let checkout = Checkout::at(repo.path()); (repo, checkout, dir)
    }
    fn generate(checkout: &Checkout, dir: &Path) -> (PathBuf, String) {
        let (_, claimed) = write_owned(checkout, dir, "x.wav", SystemTime::now(), |p| fs::write(p, b"RIFF test samples")).unwrap();
        match claimed { Claimed::Yes { receipt, generation } => (receipt, generation), _ => panic!("not claimed") }
    }
    fn claim(path: &Path) -> Claim { serde_json::from_slice(&fs::read(path).unwrap()).unwrap() }
    fn pending(path: &Path) {
        let mut c = claim(path); c.state = "pending".into(); c.hold = false;
        c.accepted_proof = Some("accepted:test".into()); c.released_proof = Some("released:test".into());
        fs::write(path, serde_json::to_vec(&c).unwrap()).unwrap();
    }
    #[test]
    fn held_never_ages_acceptance_reclaims_and_keeps_evidence() {
        let (_r, co, dir) = fixture(); let (receipt, generation) = generate(&co, &dir);
        co.open(UNIX_EPOCH + std::time::Duration::from_secs(u32::MAX as u64));
        assert!(dir.join("x.wav").exists()); let before = claim(&receipt);
        complete(&dir.join("x.wav"), &generation, None, "accepted:test", "released:test").unwrap();
        assert!(!dir.join("x.wav").exists()); let after = claim(&receipt);
        assert_eq!(after.state, "reclaimed"); assert_eq!(after.sha256, before.sha256);
        assert_eq!(after.identity, before.identity); assert!(after.allocated_bytes.is_some());
        complete(&dir.join("x.wav"), &generation, None, "accepted:test", "released:test").unwrap();
    }
    #[test]
    fn missing_proof_wrong_generation_and_incomplete_output_are_protected() {
        let (_r, co, dir) = fixture(); let (_receipt, generation) = generate(&co, &dir);
        assert!(complete(&dir.join("x.wav"), &generation, None, "", "released").is_err());
        assert!(complete(&dir.join("x.wav"), "wrong", None, "accepted", "released").is_err());
        let output = dir.join("partial.wav");
        let writing = Generation::begin_at(&output, &co.repo).unwrap();
        fs::write(writing.write_path(), b"partial").unwrap();
        let c = claim(&dir.join(".partial.wav.owned")); drop(writing);
        assert!(complete(&output, &c.generation, None, "accepted", "released").is_err());
        co.open(SystemTime::now()); assert_eq!(fs::read(output).unwrap(), b"partial");
    }
    #[test]
    fn receipt_is_durable_before_producer_runs_and_failure_is_held() {
        let (_r, co, dir) = fixture();
        let result = write_owned(&co, &dir, "x.wav", SystemTime::now(), |p| -> io::Result<()> {
            assert_eq!(claim(&dir.join(".x.wav.owned")).state, "writing");
            fs::write(p, b"partial")?; Err(io::Error::other("producer failed"))
        });
        assert!(result.is_err()); co.open(SystemTime::now());
        assert_eq!(fs::read(dir.join("x.wav")).unwrap(), b"partial");
        assert_eq!(claim(&dir.join(".x.wav.owned")).state, "writing");
    }
    #[test]
    fn pending_startup_retry_and_crash_after_rename_are_idempotent() {
        let (_r, co, dir) = fixture(); let (receipt, generation) = generate(&co, &dir); pending(&receipt);
        let tomb = dir.join(format!(".x.wav.{generation}.reclaiming"));
        fs::rename(dir.join("x.wav"), &tomb).unwrap();
        co.open(SystemTime::now()); assert!(!tomb.exists()); assert_eq!(claim(&receipt).state, "reclaimed");
        co.open(SystemTime::now()); assert_eq!(claim(&receipt).state, "reclaimed");
    }
    #[test]
    fn crash_after_unlink_preserves_receipt() {
        let (_r, co, dir) = fixture(); let (receipt, _) = generate(&co, &dir); pending(&receipt);
        fs::remove_file(dir.join("x.wav")).unwrap(); co.open(SystemTime::now());
        assert_eq!(claim(&receipt).state, "reclaimed"); assert!(claim(&receipt).sha256.is_some());
    }
    #[test]
    fn manual_legacy_and_preexisting_receipts_are_never_adopted() {
        let (_r, co, dir) = fixture(); fs::write(dir.join("manual.wav"), b"manual").unwrap();
        let result = write_owned(&co, &dir, "manual.wav", SystemTime::now(), |_p| -> io::Result<()> { panic!("protected producer must not run") });
        assert!(result.is_err()); assert_eq!(fs::read(dir.join("manual.wav")).unwrap(), b"manual");
        assert!(!dir.join(".manual.wav.owned").exists());
        fs::write(dir.join(".legacy.wav.owned"), b"funkot-owned-wav v1").unwrap();
        let result = write_owned(&co, &dir, "legacy.wav", SystemTime::now(), |_p| -> io::Result<()> { panic!("legacy producer must not run") });
        assert!(result.is_err()); co.open(SystemTime::now());
        assert_eq!(fs::read(dir.join(".legacy.wav.owned")).unwrap(), b"funkot-owned-wav v1");
        assert!(!dir.join("legacy.wav").exists());
    }
    #[test]
    fn modified_replaced_symlink_and_hardlink_outputs_are_retained() {
        for mode in ["modified", "replaced", "symlink", "hardlink"] {
            let (_r, co, dir) = fixture(); let (receipt, generation) = generate(&co, &dir);
            let output = dir.join("x.wav");
            match mode {
                "modified" => fs::write(&output, b"changed").unwrap(),
                "replaced" => { fs::rename(&output, dir.join("original")).unwrap(); fs::write(&output, b"RIFF test samples").unwrap(); },
                "symlink" => { fs::rename(&output, dir.join("original")).unwrap(); std::os::unix::fs::symlink(dir.join("original"), &output).unwrap(); },
                "hardlink" => fs::hard_link(&output, dir.join("other")).unwrap(), _ => unreachable!(),
            }
            assert!(complete(&output, &generation, None, "accepted", "released").is_err(), "{mode}");
            assert!(output.exists(), "{mode}"); assert_eq!(claim(&receipt).state, "pending");
        }
    }
    #[test]
    fn symlink_root_nested_repo_and_mount_refused() {
        let (r, co, dir) = fixture();
        let nested = dir.join("nested"); fs::create_dir_all(nested.join(".git")).unwrap();
        assert!(Directory::open(&nested, &co.repo).is_err());
        let link = r.path().join("link"); std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(Directory::open(&link, &co.repo).is_err());
        assert!(Directory::open(Path::new("/proc"), Path::new("/")).is_err());
    }
    #[test]
    fn active_writer_and_owner_lock_prevent_completion() {
        let (_r, co, dir) = fixture(); let output = dir.join("x.wav");
        let mut writing = Generation::begin_at(&output, &co.repo).unwrap(); fs::write(writing.write_path(), b"RIFF").unwrap();
        let Claimed::Yes { generation, .. } = writing.finish().unwrap() else { panic!() };
        assert!(complete(&output, &generation, None, "accepted", "released").is_err());
        assert!(Generation::begin_at(&output, &co.repo).is_err()); drop(writing);
        complete(&output, &generation, None, "accepted", "released").unwrap();
    }
    #[test]
    fn moved_receipt_and_replaced_directory_are_refused() {
        let (r, co, dir) = fixture(); let (receipt, generation) = generate(&co, &dir); pending(&receipt);
        let new_dir = r.path().join("elsewhere"); fs::create_dir(&new_dir).unwrap();
        fs::copy(&receipt, new_dir.join(".x.wav.owned")).unwrap(); fs::copy(dir.join("x.wav"), new_dir.join("x.wav")).unwrap();
        assert!(complete(&new_dir.join("x.wav"), &generation, None, "accepted", "released").is_err());
        let pinned = Directory::open(&dir, &co.repo).unwrap(); fs::rename(&dir, r.path().join("old")).unwrap(); fs::create_dir(&dir).unwrap();
        assert!(pinned.revalidate().is_err());
    }
    #[test]
    fn same_generation_tomb_collision_is_retained() {
        let (_r, co, dir) = fixture(); let (_receipt, generation) = generate(&co, &dir);
        let tomb = dir.join(format!(".x.wav.{generation}.reclaiming")); fs::write(&tomb, b"manual").unwrap();
        assert!(complete(&dir.join("x.wav"), &generation, None, "accepted", "released").is_err());
        assert_eq!(fs::read(tomb).unwrap(), b"manual"); assert!(dir.join("x.wav").exists());
    }
    #[test]
    fn explicit_hold_cannot_be_cleared_by_completion() {
        let (_r, co, dir) = fixture(); let (receipt, generation) = generate(&co, &dir);
        let mut c = claim(&receipt); c.hold = true;
        fs::write(&receipt, serde_json::to_vec(&c).unwrap()).unwrap();
        assert!(complete(&dir.join("x.wav"), &generation, None, "accepted", "released").is_err());
        assert!(dir.join("x.wav").exists()); assert!(claim(&receipt).hold);
    }
    #[test]
    fn new_output_during_tomb_retry_is_protected() {
        let (_r, co, dir) = fixture(); let (receipt, generation) = generate(&co, &dir); pending(&receipt);
        let tomb = dir.join(format!(".x.wav.{generation}.reclaiming"));
        fs::rename(dir.join("x.wav"), &tomb).unwrap(); fs::write(dir.join("x.wav"), b"manual").unwrap();
        co.open(SystemTime::now()); assert!(tomb.exists());
        assert_eq!(fs::read(dir.join("x.wav")).unwrap(), b"manual"); assert_eq!(claim(&receipt).state, "pending");
    }

    #[test]
    fn managed_registration_precedes_bytes_and_external_receipt_survives() {
        let (repo, co, dir) = fixture(); let external = tempfile::tempdir().unwrap();
        let script = repo.path().join("register.sh");
        fs::write(&script, r#"set -eu
output= receipt=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output) output=$2; shift 2 ;;
    --receipt) receipt=$2; shift 2 ;;
    *) shift ;;
  esac
done
test ! -e "$output"
test -s "$receipt"
grep -q '"state": "writing"' "$receipt"
"#).unwrap();
        let context = Context { owner_receipt_dir: external.path().into(), owner_receipt_argv: vec!["sh".into(), script.to_string_lossy().into_owned()], owner_completion_argv: Some(vec!["main-cli".into()]) };
        let output = dir.join("managed.wav");
        let mut writer = Generation::begin_with_context(&output, &co.repo, Some(context)).unwrap();
        fs::write(writer.write_path(), b"RIFF managed").unwrap();
        let Claimed::Yes { generation, receipt } = writer.finish().unwrap() else { panic!() }; drop(writer);
        assert!(receipt.starts_with(external.path())); assert!(!dir.join(".managed.wav.owned").exists());
        complete(&output, &generation, Some(&receipt), "accepted", "released").unwrap();
        assert!(!output.exists()); assert_eq!(claim(&receipt).state, "reclaimed");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }
    #[test]
    fn failed_registration_writes_no_output_bytes() {
        let (_repo, co, dir) = fixture(); let external = tempfile::tempdir().unwrap();
        let context = Context { owner_receipt_dir: external.path().into(), owner_receipt_argv: vec!["sh".into(), "-c".into(), "exit 1".into()], owner_completion_argv: Some(vec!["main-cli".into()]) };
        let output = dir.join("failed.wav");
        assert!(Generation::begin_with_context(&output, &co.repo, Some(context)).is_err());
        assert!(!output.exists());
        let receipts: Vec<_> = fs::read_dir(external.path()).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|e| e == "owned")).collect();
        assert_eq!(receipts.len(), 1); assert_eq!(claim(&receipts[0].path()).state, "writing");
    }

    #[test]
    fn proofs_and_provenance_cannot_be_replaced_at_completion() {
        let (_r, co, dir) = fixture(); let (receipt, generation) = generate(&co, &dir); pending(&receipt);
        assert!(complete(&dir.join("x.wav"), &generation, None, "different", "released:test").is_err());
        assert!(dir.join("x.wav").exists());
        let mut c = claim(&receipt); let original = c.source_revision.clone(); c.source_revision = "unknown".into();
        fs::write(&receipt, serde_json::to_vec(&c).unwrap()).unwrap();
        co.open(SystemTime::now()); assert!(dir.join("x.wav").exists());
        c.source_revision = original; fs::write(&receipt, serde_json::to_vec(&c).unwrap()).unwrap();
        complete(&dir.join("x.wav"), &generation, None, "accepted:test", "released:test").unwrap();
        assert!(complete(&dir.join("x.wav"), &generation, None, "changed", "released:test").is_err());
    }
    #[test]
    fn managed_preexisting_and_symlinked_outputs_fail_before_write() {
        let (repo, co, dir) = fixture(); let external = tempfile::tempdir().unwrap();
        let context = || Context { owner_receipt_dir: external.path().into(), owner_receipt_argv: vec!["false".into()], owner_completion_argv: Some(vec!["main-cli".into()]) };
        let output = dir.join("manual.wav"); fs::write(&output, b"manual").unwrap();
        assert!(Generation::begin_with_context(&output, &co.repo, Some(context())).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"manual");
        let link = repo.path().join("link"); std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(Generation::begin_with_context(&link.join("new.wav"), &co.repo, Some(context())).is_err());
        assert!(!dir.join("new.wav").exists());
    }
    #[test]
    fn masters_overlap_is_refused_in_isolated_process() {
        if let Some(path) = std::env::var_os("FUNKOT_OWNER_MASTER_TEST") {
            let path = PathBuf::from(path);
            assert!(Directory::open(&path, path.parent().unwrap()).is_err());
            return;
        }
        let (_r, _co, dir) = fixture();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "owned_wav::tests::masters_overlap_is_refused_in_isolated_process"])
            .env("FUNKOT_OWNER_MASTER_TEST", &dir).env("FUNKOT_TESTDATA_DIR", &dir).status().unwrap();
        assert!(status.success());
    }

}
