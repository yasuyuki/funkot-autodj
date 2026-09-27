//! Native Windows ownership. Every namespace operation after opening a drive
//! root is relative to a retained directory handle; reparse points fail closed.
//! ABI: Microsoft NtCreateFile, FILE_RENAME_INFORMATION, FILE_ID_BOTH_DIR_INFO,
//! GetFinalPathNameByHandleW and FILE_DISPOSITION_INFO documentation.
use super::*;
use std::ffi::{c_void, OsString};
use std::os::windows::{ffi::OsStringExt, fs::{MetadataExt, OpenOptionsExt}, io::{AsRawHandle, FromRawHandle}};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetFileInformationByHandleEx(handle: *mut c_void, class: i32, info: *mut c_void, size: u32) -> i32;
    fn SetFileInformationByHandle(handle: *mut c_void, class: i32, info: *const c_void, size: u32) -> i32;
    fn GetFileType(handle: *mut c_void) -> u32;
    fn GetFinalPathNameByHandleW(handle: *mut c_void, path: *mut u16, size: u32, flags: u32) -> u32;
}
#[repr(C)]
struct UnicodeString { len: u16, capacity: u16, buffer: *mut u16 }
#[repr(C)]
struct ObjectAttributes { len: u32, root: *mut c_void, name: *mut UnicodeString, attributes: u32, security: *mut c_void, qos: *mut c_void }
#[repr(C)]
#[derive(Default)]
struct IoStatus { status: usize, information: usize }
#[repr(C)]
struct RenameInfo { replace: u8, root: *mut c_void, len: u32, name: [u16; 1] }
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateFile(handle: *mut *mut c_void, access: u32, attributes: *mut ObjectAttributes, status: *mut IoStatus, allocation: *const i64, file_attributes: u32, sharing: u32, disposition: u32, options: u32, ea: *const c_void, ea_len: u32) -> i32;
    fn NtSetInformationFile(handle: *mut c_void, status: *mut IoStatus, info: *const c_void, len: u32, class: i32) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}
#[repr(C)]
#[derive(Default)]
struct FileId { volume: u64, id: [u8; 16] }
#[repr(C)]
#[derive(Default)]
struct Standard { allocation: i64, size: i64, links: u32, delete_pending: u8, directory: u8 }
fn info<T: Default>(file: &File, class: i32) -> io::Result<T> {
    let mut value = T::default();
    // Each caller supplies the documented structure for this information class.
    if unsafe { GetFileInformationByHandleEx(file.as_raw_handle(), class, (&mut value as *mut T).cast(), std::mem::size_of::<T>() as u32) } == 0 { return Err(io::Error::last_os_error()); }
    Ok(value)
}
fn nt_result(status: i32) -> io::Result<()> {
    if status < 0 { Err(io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)) } else { Ok(()) }
}
fn id(file: &File) -> io::Result<FileId> {
    if unsafe { GetFileType(file.as_raw_handle()) } != 1 || file.metadata()?.file_attributes() & 0x400 != 0 {
        return Err(invalid("reparse point or non-disk object; retained"));
    }
    info(file, 18) // FileIdInfo: full 128-bit ID and volume serial.
}
fn same_id(a: &FileId, b: &FileId) -> bool { a.volume == b.volume && a.id == b.id }
pub(super) fn identity(file: &File) -> io::Result<Identity> {
    let id = id(file)?; let meta = file.metadata()?;
    Ok(Identity { dev: id.volume, ino: 0, file_id: Some(id.id), len: meta.file_size(), mtime: meta.last_write_time() as i64, mtime_ns: 0 })
}
pub(super) fn single_regular(file: &File) -> io::Result<bool> {
    id(file)?; let value: Standard = info(file, 1)?;
    Ok(value.directory == 0 && value.links == 1 && value.delete_pending == 0)
}
pub(super) fn allocated(file: &File) -> io::Result<u64> {
    let value: Standard = info(file, 1)?;
    value.allocation.try_into().map_err(|_| invalid("invalid allocated size"))
}
pub(super) fn normal_path(path: &Path) -> io::Result<PathBuf> {
    use std::path::Prefix;
    let path = if path.is_absolute() { path.to_owned() } else { std::env::current_dir()?.join(path) };
    let mut parts = path.components();
    let Some(Component::Prefix(prefix)) = parts.next() else { return Err(invalid("owner requires a local disk path")); };
    let disk = match prefix.kind() { Prefix::Disk(d) | Prefix::VerbatimDisk(d) => d, _ => return Err(invalid("UNC and device ownership paths are unsupported; retained")) };
    if parts.next() != Some(Component::RootDir) { return Err(invalid("owner requires an absolute disk path")); }
    let mut result = PathBuf::from(format!("{}:\\", disk.to_ascii_uppercase() as char));
    for part in parts {
        match part {
            Component::CurDir => {},
            Component::Normal(name) => { valid_name(name.to_str().ok_or_else(|| invalid("non-Unicode ownership path"))?)?; result.push(name); },
            _ => return Err(invalid("non-normal ownership path")),
        }
    }
    Ok(result)
}
fn canonical(file: &File) -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 256];
    loop {
        let n = unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), buffer.as_mut_ptr(), buffer.len() as u32, 0) };
        if n == 0 { return Err(io::Error::last_os_error()); }
        if (n as usize) < buffer.len() { buffer.truncate(n as usize); return normal_path(&PathBuf::from(OsString::from_wide(&buffer))); }
        buffer.resize(n as usize + 1, 0);
    }
}
// One component only. FILE_OPEN_REPARSE_POINT prevents leaf traversal;
// OBJ_DONT_REPARSE rejects any object-manager reparse. The RootDirectory handle
// binds resolution even if somebody changes a pinned directory's reparse data.
fn relative(parent: &File, name: &str, access: u32, sharing: u32, disposition: u32, directory: bool) -> io::Result<File> {
    relative_options(parent, name, access, sharing, disposition, directory, 0)
}
fn relative_options(parent: &File, name: &str, access: u32, sharing: u32, disposition: u32, directory: bool, extra: u32) -> io::Result<File> {
    valid_name(name)?; id(parent)?;
    let mut utf16: Vec<u16> = name.encode_utf16().collect();
    let len = u16::try_from(utf16.len() * 2).map_err(|_| invalid("native name too long"))?;
    if utf16.contains(&0) { return Err(invalid("NUL name")); }
    let mut string = UnicodeString { len, capacity: len, buffer: utf16.as_mut_ptr() };
    let mut object = ObjectAttributes { len: std::mem::size_of::<ObjectAttributes>() as u32, root: parent.as_raw_handle(), name: &mut string, attributes: 0x1040, security: std::ptr::null_mut(), qos: std::ptr::null_mut() };
    let mut status = IoStatus::default(); let mut raw = std::ptr::null_mut();
    // SYNCHRONIZE, synchronous nonalert I/O, no leaf reparsing. Type checked
    // through the resulting handle, including for metadata-only existence tests.
    let options = 0x20 | 0x00200000 | extra | if directory { 0 } else { 0x40 };
    nt_result(unsafe { NtCreateFile(&mut raw, access | 0x100000, &mut object, &mut status, std::ptr::null(), 0x80, sharing, disposition, options, std::ptr::null(), 0) })?;
    // Successful NtCreateFile transfers one valid owned handle to File.
    let file = unsafe { File::from_raw_handle(raw) };
    id(&file)?;
    Ok(file)
}
fn chain(path: &Path) -> io::Result<Vec<File>> {
    let path = normal_path(path)?; let mut parts = path.components();
    let mut root = PathBuf::from(parts.next().unwrap().as_os_str()); root.push(parts.next().unwrap().as_os_str());
    // The only absolute open is a validated local drive root, without user path
    // components. Every descendant is opened using the preceding handle.
    let mut files = vec![OpenOptions::new().read(true).share_mode(3).custom_flags(0x02200000).open(root)?];
    id(&files[0])?;
    for part in parts {
        let Component::Normal(name) = part else { return Err(invalid("non-normal directory")); };
        let file = relative(files.last().unwrap(), name.to_str().ok_or_else(|| invalid("non-Unicode name"))?, 0x81, 3, 1, true)?;
        let standard: Standard = info(&file, 1)?;
        if standard.directory == 0 || standard.delete_pending != 0 { return Err(invalid("not a live directory")); }
        if id(files.last().unwrap())?.volume != id(&file)?.volume { return Err(invalid("other volume")); }
        files.push(file);
    }
    Ok(files)
}
pub(super) fn directory(path: &Path, repo: &Path, receipt_role: bool) -> io::Result<Directory> {
    let mut parents = chain(path)?;
    if parents.len() < 2 { return Err(invalid("root directory cannot own output")); }
    let repository = chain(repo)?;
    let repository_ids: Vec<_> = repository.iter().map(id).collect::<io::Result<_>>()?;
    let masters = std::env::var_os("FUNKOT_TESTDATA_DIR").filter(|v| !v.is_empty()).map(|p| chain(Path::new(&p))).transpose()?;
    for file in &parents {
        let current = id(file)?;
        if let Some(masters) = &masters {
            if same_id(&current, &id(masters.last().unwrap())?) { return Err(invalid("overlaps masters")); }
        }
        if !receipt_role && !repository_ids.iter().any(|r| same_id(r, &current)) && exists_file(file, ".git")? { return Err(invalid("nested or foreign repository")); }
    }
    let file = parents.pop().unwrap(); let current = id(&file)?;
    if let Some(masters) = masters {
        for ancestor in masters { if same_id(&current, &id(&ancestor)?) { return Err(invalid("overlaps masters")); } }
    }
    let path = canonical(&file)?; let repo = canonical(repository.last().unwrap())?;
    Ok(Directory { path, file, dev: current.volume, ino: 0, file_id: Some(current.id), mount_id: 0, repo, _parents: parents, receipt_role })
}
fn exists_file(parent: &File, name: &str) -> io::Result<bool> {
    match relative(parent, name, 0x80, 7, 1, true) { Ok(_) => Ok(true), Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false), Err(e) => Err(e) }
}
pub(super) fn exists(dir: &Directory, name: &str) -> io::Result<bool> { exists_file(&dir.file, name) }
pub(super) fn canonical_child(dir: &Directory, name: &str) -> io::Result<PathBuf> {
    match relative(&dir.file, name, 0x80, 7, 1, true) {
        Ok(f) => canonical(&f), Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(dir.path.join(name)), Err(e) => Err(e),
    }
}
pub(super) fn read(dir: &Directory, name: &str) -> io::Result<File> { relative(&dir.file, name, 0x80000000, 3, 1, false) }
pub(super) fn create(dir: &Directory, name: &str) -> io::Result<File> { relative(&dir.file, name, 0xc0000000, 1, 2, false) }
pub(super) fn manual_writer(dir: &Directory, name: &str) -> io::Result<File> {
    let file = relative(&dir.file, name, 0xc0000000, 1, 1, false)?;
    if !single_regular(&file)? { return Err(invalid("linked or special manual output; retained")); } Ok(file)
}
pub(super) fn lock(dir: &Directory, name: &str) -> io::Result<File> {
    let file = relative(&dir.file, name, 0xc0000000, 3, 3, false)?;
    if !single_regular(&file)? { return Err(invalid("invalid owner lock")); }
    file.try_lock().map_err(|e| io::Error::other(format!("owner busy: {e}")))?; dir.revalidate()?; Ok(file)
}
pub(super) fn output_lock(dir: &Directory, output: &Path) -> io::Result<File> {
    // FILE_CREATE + FILE_DELETE_ON_CLOSE: never adopt an unknown lock, and
    // process death closes the handle and removes the transient lease as well.
    let file = relative_options(&dir.file, &format!(".{}.funkot-wav.lock", file_name(output)?), 0xc0010000, 0, 2, false, 0x1000)?;
    let standard: Standard = info(&file, 1)?;
    if standard.directory != 0 || standard.links != 1 { return Err(invalid("invalid output lease")); }
    dir.revalidate()?; Ok(file)
}
pub(super) fn names(dir: &Directory) -> io::Result<Vec<String>> {
    id(&dir.file)?;
    let mut names = Vec::new(); let mut restart = true;
    loop {
        let mut storage = vec![0u64; 8192];
        let size = (storage.len() * 8) as u32;
        if unsafe { GetFileInformationByHandleEx(dir.file.as_raw_handle(), if restart { 11 } else { 10 }, storage.as_mut_ptr().cast(), size) } == 0 {
            let e = io::Error::last_os_error(); if e.raw_os_error() == Some(18) { return Ok(names); } return Err(e);
        }
        restart = false;
        let bytes = unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<u8>(), size as usize) };
        let mut offset = 0;
        loop {
            // FILE_ID_BOTH_DIR_INFO: FileNameLength at 60; FileName at 104.
            if offset + 104 > bytes.len() { return Err(invalid("invalid directory enumeration")); }
            let u32_at = |at| u32::from_le_bytes(bytes[at..at+4].try_into().unwrap()) as usize;
            let next = u32_at(offset); let len = u32_at(offset + 60);
            if len % 2 != 0 || len > bytes.len() - offset - 104 { return Err(invalid("invalid directory name")); }
            let wide: Vec<_> = bytes[offset+104..offset+104+len].chunks_exact(2).map(|s| u16::from_le_bytes([s[0],s[1]])).collect();
            if let Ok(name) = String::from_utf16(&wide) { if name != "." && name != ".." { names.push(name); } }
            if next == 0 { break; }
            if next < 104 + len || next > bytes.len() - offset { return Err(invalid("invalid directory offset")); }
            offset += next;
        }
    }
}
pub(super) fn persist(dir: &Directory, side: &str, claim: &Claim, create: bool) -> io::Result<()> {
    dir.revalidate()?;
    if !create {
        let prior = dir.read_claim(side)?;
        if prior.version != claim.version || prior.owner != claim.owner || prior.generation != claim.generation || prior.output != claim.output || prior.receipt != claim.receipt {
            return Err(invalid("receipt replaced; retained"));
        }
    }
    let tmp = format!("{side}.{}.tmp", generation_id());
    let mut file = relative(&dir.file, &tmp, 0xc0010000, 1, 2, false)?;
    serde_json::to_writer_pretty(&mut file, claim)?; file.write_all(b"\n")?; file.sync_all()?;
    id(&dir.file)?;
    let wide: Vec<u16> = side.encode_utf16().collect(); valid_name(side)?;
    let offset = std::mem::offset_of!(RenameInfo, name);
    let bytes = (offset + wide.len()*2).max(std::mem::size_of::<RenameInfo>());
    let mut storage = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
    let ptr = storage.as_mut_ptr().cast::<RenameInfo>();
    // Aligned, zero-initialized variable-length FILE_RENAME_INFORMATION.
    unsafe {
        (*ptr).replace = u8::from(!create); (*ptr).root = dir.file.as_raw_handle(); (*ptr).len = (wide.len()*2) as u32;
        std::ptr::copy_nonoverlapping(wide.as_ptr(), storage.as_mut_ptr().cast::<u8>().add(offset).cast::<u16>(), wide.len());
    }
    let mut status = IoStatus::default();
    nt_result(unsafe { NtSetInformationFile(file.as_raw_handle(), &mut status, ptr.cast(), bytes as u32, 10) })?;
    file.sync_all()?; Ok(())
}
pub(super) fn marker_name(claim: &Claim) -> io::Result<String> { Ok(format!(".{}.owned", file_name(&claim.output)?)) }
fn marker_bytes(claim: &Claim) -> io::Result<Vec<u8>> {
    Ok(serde_json::to_vec(&serde_json::json!({"version":1,"owner":"funkot-wav-marker","generation":claim.generation,"output":claim.output,"receipt":claim.receipt}))?)
}
pub(super) fn create_marker(dir: &Directory, claim: &mut Claim) -> io::Result<()> {
    let name = marker_name(claim)?;
    let mut file = create(dir, &name)?; file.write_all(&marker_bytes(claim)?)?; file.sync_all()?; drop(file);
    let file = dir.read_regular(&name)?;
    claim.output_marker = Some(identity(&file)?); claim.output_marker_sha256 = Some(hash_file(&file)?); Ok(())
}
fn marker_for_delete(dir: &Directory, claim: &Claim) -> io::Result<Option<File>> {
    let Some(expected) = &claim.output_marker else {
        if claim.output_marker_sha256.is_some() { return Err(invalid("incomplete output marker proof")); }
        return Ok(None);
    };
    let file = match relative(&dir.file, &marker_name(claim)?, 0x80010000, 1, 1, false) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound && claim.result.as_deref() == Some(DELETE_INTENT) => return Ok(None),
        Err(e) => return Err(e),
    };
    let hash = hash_file(&file)?;
    if !single_regular(&file)? || &identity(&file)? != expected || claim.output_marker_sha256.as_deref() != Some(hash.as_str())
        || hash != format!("{:x}", Sha256::digest(marker_bytes(claim)?)) { return Err(invalid("output marker changed; retained")); }
    Ok(Some(file))
}
fn finish_delete(dir: &Directory, receipts: &Directory, side: &str, claim: &mut Claim, marker: Option<File>) -> io::Result<()> {
    if let Some(marker) = marker { delete(&marker)?; drop(marker); }
    if dir.exists(file_name(&claim.output)?)? || (claim.output_marker.is_some() && dir.exists(&marker_name(claim)?)?) { return Err(invalid("output or marker still present; retry retained")); }
    claim.state = "reclaimed".into(); claim.result = Some(format!("removed; allocated_bytes={}", claim.allocated_bytes.unwrap_or(0)));
    receipts.persist(side, claim, false)
}
fn delete(file: &File) -> io::Result<()> {
    let disposition: u8 = 1;
    if unsafe { SetFileInformationByHandle(file.as_raw_handle(), 4, (&disposition as *const u8).cast(), 1) } == 0 { return Err(io::Error::last_os_error()); }
    Ok(())
}
const DELETE_INTENT: &str = "verified Windows handle; deletion intent persisted";
pub(super) fn reclaim(dir: &Directory, receipts: &Directory, side: &str, claim: &mut Claim) -> io::Result<()> {
    let name = file_name(&claim.output)?;
    let marker = marker_for_delete(dir, claim)?;
    let file = match relative(&dir.file, name, 0x80010000, 1, 1, false) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound && claim.result.as_deref() == Some(DELETE_INTENT) => {
            return finish_delete(dir, receipts, side, claim, marker);
        },
        Err(e) => return Err(e),
    };
    if !single_regular(&file)? { return Err(invalid("linked or special output; retained")); }
    let before = identity(&file)?;
    if claim.identity.as_ref() != Some(&before) || claim.sha256.as_deref() != Some(hash_file(&file)?.as_str()) || identity(&file)? != before { return Err(invalid("identity or hash changed; retained")); }
    dir.revalidate()?;
    claim.result = Some(DELETE_INTENT.into()); receipts.persist(side, claim, false)?;
    delete(&file)?; drop(file);
    #[cfg(test)]
    if let Some(marker) = std::env::var_os("FUNKOT_OWNER_DELETE_KILL_MARKER") {
        fs::write(marker, b"deleted; receipt still pending")?;
        loop { std::thread::sleep(std::time::Duration::from_secs(1)); }
    }
    finish_delete(dir, receipts, side, claim, marker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::os::windows::ffi::OsStrExt;
    fn fixture() -> (TempDir, Checkout, PathBuf) {
        let repo = tempfile::tempdir().unwrap();
        let dir = repo.path().join("testdata/synth"); fs::create_dir_all(&dir).unwrap();
        let checkout = Checkout::at(repo.path()); (repo, checkout, dir)
    }
    fn claim(path: &Path) -> Claim { serde_json::from_slice(&fs::read(path).unwrap()).unwrap() }
    fn generate(checkout: &Checkout, dir: &Path) -> (PathBuf, String) {
        let (_, result) = write_owned(checkout, dir, "x.wav", SystemTime::now(), |mut file| file.write_all(b"RIFF samples")).unwrap();
        let Claimed::Yes { receipt, generation } = result else { panic!("native ownership missing") };
        (receipt, generation)
    }
    #[test]
    fn standalone_generation_is_owned_and_reclaimed() {
        let (_repo, co, dir) = fixture(); let (receipt, generation) = generate(&co, &dir);
        let before = claim(&receipt); assert!(before.identity.as_ref().unwrap().file_id.is_some());
        co.open(UNIX_EPOCH); assert!(dir.join("x.wav").exists());
        complete(&dir.join("x.wav"), &generation, None, "accepted", "released").unwrap();
        assert!(!dir.join("x.wav").exists());
        let after = claim(&receipt); assert_eq!(after.state, "reclaimed");
        assert_eq!(before.identity, after.identity); assert_eq!(before.sha256, after.sha256);
        complete(&dir.join("x.wav"), &generation, None, "accepted", "released").unwrap();
        assert!(complete(&dir.join("x.wav"), &generation, None, "changed", "released").is_err());
    }
    #[test]
    fn hound_writer_finishes_before_windows_metadata_is_sealed() {
        let (_repo, co, dir) = fixture();
        let (_, result) = write_owned(&co, &dir, "hound.wav", SystemTime::now(), |file| -> io::Result<()> {
            let spec = hound::WavSpec { channels: 1, sample_rate: 8000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
            let mut writer = hound::WavWriter::new(std::io::BufWriter::new(file), spec).map_err(io::Error::other)?;
            writer.write_sample(123i16).map_err(io::Error::other)?;
            writer.finalize().map_err(io::Error::other)?; Ok(())
        }).unwrap();
        let Claimed::Yes { generation, .. } = result else { panic!() };
        let mut reader = hound::WavReader::open(dir.join("hound.wav")).unwrap();
        assert_eq!(reader.samples::<i16>().next().unwrap().unwrap(), 123); drop(reader);
        complete(&dir.join("hound.wav"), &generation, None, "accepted", "released").unwrap();
        assert!(!dir.join("hound.wav").exists());
    }
    #[test]
    fn writing_claim_precedes_bytes_and_producer_failure_stays_held() {
        let (_repo, co, dir) = fixture();
        let result = write_owned(&co, &dir, "x.wav", SystemTime::now(), |mut file| -> io::Result<()> {
            assert_eq!(claim(&dir.join(".x.wav.owned")).state, "writing");
            file.write_all(b"partial")?; Err(io::Error::other("producer failed"))
        });
        assert!(result.is_err());
        assert_eq!(claim(&dir.join(".x.wav.owned")).state, "writing");
    }
    #[test]
    fn active_generation_pins_output_and_ancestors_and_refuses_completion() {
        let (repo, co, dir) = fixture(); let path = dir.join("x.wav");
        let mut writing = Generation::begin_at(&path, &co.repo).unwrap();
        writing.writer_file().unwrap().write_all(b"RIFF").unwrap();
        assert!(fs::rename(&path, dir.join("renamed.wav")).is_err());
        assert!(fs::remove_file(&path).is_err());
        assert!(fs::rename(&dir, repo.path().join("renamed")).is_err());
        let Claimed::Yes { generation, .. } = writing.finish().unwrap() else { panic!() };
        assert!(complete(&path, &generation, None, "accepted", "released").is_err());
        drop(writing); complete(&path, &generation, None, "accepted", "released").unwrap();
    }
    #[test]
    fn held_busy_and_changed_objects_are_retained() {
        for kind in ["hold", "writer", "reader", "modified", "replacement", "hardlink"] {
            let (_repo, co, dir) = fixture(); let (receipt, generation) = generate(&co, &dir); let path = dir.join("x.wav");
            let mut held = None;
            match kind {
                "hold" => { let mut c = claim(&receipt); c.hold = true; fs::write(&receipt, serde_json::to_vec(&c).unwrap()).unwrap(); },
                "writer" => held = Some(OpenOptions::new().write(true).open(&path).unwrap()),
                "reader" => held = Some(OpenOptions::new().read(true).share_mode(1).open(&path).unwrap()),
                "modified" => fs::write(&path, b"changed").unwrap(),
                "replacement" => { fs::rename(&path, dir.join("original")).unwrap(); fs::write(&path, b"RIFF samples").unwrap(); },
                "hardlink" => fs::hard_link(&path, dir.join("linked.wav")).unwrap(),
                _ => unreachable!(),
            }
            assert!(complete(&path, &generation, None, "accepted", "released").is_err(), "{kind}");
            assert!(path.exists(), "{kind}"); drop(held);
            if matches!(kind, "writer" | "reader") { co.open(SystemTime::now()); assert!(!path.exists()); }
        }
    }
    #[test]
    fn pending_delete_retry_requires_intent_and_keeps_exact_identity() {
        let (_repo, co, dir) = fixture(); let (receipt, generation) = generate(&co, &dir); let output = dir.join("x.wav");
        let mut c = claim(&receipt); c.state = "pending".into(); c.accepted_proof = Some("accepted".into()); c.released_proof = Some("released".into());
        c.result = Some(DELETE_INTENT.into()); fs::write(&receipt, serde_json::to_vec(&c).unwrap()).unwrap();
        let file = OpenOptions::new().access_mode(0x80010000).share_mode(1).open(&output).unwrap();
        delete(&file).unwrap(); drop(file); // Crash boundary: no reclaimed receipt saved.
        co.open(SystemTime::now()); assert_eq!(claim(&receipt).state, "reclaimed");
        assert_eq!(claim(&receipt).identity, c.identity);
        fs::write(&output, b"new generation").unwrap();
        assert!(complete(&output, &generation, None, "accepted", "released").is_err());
        assert_eq!(fs::read(output).unwrap(), b"new generation");
    }
    #[test]
    fn interrupted_delete_never_adopts_a_replacement_or_unproven_absence() {
        for replacement in [false, true] {
            let (_repo, co, dir) = fixture(); let (receipt, generation) = generate(&co, &dir); let output = dir.join("x.wav");
            let mut c = claim(&receipt); c.state = "pending".into();
            c.accepted_proof = Some("accepted".into()); c.released_proof = Some("released".into());
            if replacement { c.result = Some(DELETE_INTENT.into()); }
            fs::write(&receipt, serde_json::to_vec(&c).unwrap()).unwrap();
            fs::remove_file(&output).unwrap();
            if replacement { fs::write(&output, b"unknown replacement").unwrap(); }
            assert!(complete(&output, &generation, None, "accepted", "released").is_err());
            co.open(SystemTime::now()); assert_eq!(claim(&receipt).state, "pending");
            if replacement { assert_eq!(fs::read(&output).unwrap(), b"unknown replacement"); }
        }
    }
    #[test]
    fn explicit_standalone_render_can_overwrite_without_adopting_manual_output() {
        let (_repo, co, dir) = fixture(); let output = dir.join("manual.wav");
        fs::write(&output, b"manual").unwrap();
        let mut writer = Generation::begin_at(&output, &co.repo).unwrap();
        writer.writer_file().unwrap().write_all(b"explicit render").unwrap();
        assert!(matches!(writer.finish().unwrap(), Claimed::No(_))); drop(writer);
        assert_eq!(fs::read(output).unwrap(), b"explicit render");
        assert!(!dir.join(".manual.wav.owned").exists());
    }
    #[test]
    fn manual_output_and_nested_repository_are_not_adopted() {
        let (_repo, co, dir) = fixture(); fs::write(dir.join("x.wav"), b"manual").unwrap();
        let result = write_owned(&co, &dir, "x.wav", SystemTime::now(), |_| -> io::Result<()> { panic!("must not run") });
        assert!(result.is_err()); assert_eq!(fs::read(dir.join("x.wav")).unwrap(), b"manual");
        let nested = dir.join("nested"); fs::create_dir_all(nested.join(".git")).unwrap();
        assert!(Directory::open(&nested, &co.repo).is_err());
    }
    #[test]
    fn masters_overlap_uses_native_ids() {
        if let Some(path) = std::env::var_os("FUNKOT_OWNER_MASTER_TEST") {
            let path = PathBuf::from(path);
            assert!(Directory::open(&path, path.parent().unwrap()).is_err()); return;
        }
        let (_repo, _co, dir) = fixture();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "owned_wav::windows::tests::masters_overlap_uses_native_ids"])
            .env("FUNKOT_OWNER_MASTER_TEST", dir.to_string_lossy().to_uppercase())
            .env("FUNKOT_TESTDATA_DIR", &dir).status().unwrap();
        assert!(status.success());
    }
    #[test]
    fn directory_junction_is_refused_before_generation() {
        let (repo, co, dir) = fixture(); let link = repo.path().join("junction");
        // mklink is a cmd builtin: forward slashes in the fixture path are
        // parsed as switches rather than separators.
        let result = std::process::Command::new("cmd.exe").args(["/d", "/c", "mklink", "/J"])
            .arg(link.to_string_lossy().replace('/', "\\"))
            .arg(dir.to_string_lossy().replace('/', "\\")).output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        assert!(Directory::open(&link, &co.repo).is_err());
        // Remove only the junction itself; never recurse into its target.
        fs::remove_dir(link).unwrap();
    }
    #[test]
    fn managed_registration_precedes_output_and_receipt_survives() {
        let (repo, co, dir) = fixture(); let external = tempfile::tempdir().unwrap();
        let script = repo.path().join("register.py");
        fs::write(&script, r#"import json, pathlib, sys
pairs = list(zip(sys.argv[1::2], sys.argv[2::2]))
args = dict(pairs)
outputs = [v for k,v in pairs if k == '--output']
assert len(outputs) == 2 and all(not pathlib.Path(p).exists() for p in outputs)
args['--output'] = outputs[0]
assert str(pathlib.Path(outputs[0]).parent.resolve()) == str(pathlib.Path(outputs[0]).parent)
assert json.loads(pathlib.Path(args['--receipt']).read_text())['state'] == 'writing'
completion = json.loads(args['--completion-json'])
assert completion[0] == 'main cli with spaces'
assert completion[completion.index('--artifact-receipt') + 1] == args['--receipt']
assert completion[completion.index('--artifact-complete') + 1] == args['--output']
"#).unwrap();
        let python = std::env::var("FUNKOT_OWNER_TEST_PYTHON").unwrap_or_else(|_| "python".into());
        let context = Context { owner_receipt_dir: external.path().into(), owner_receipt_argv: vec![python, script.to_string_lossy().into_owned()], owner_completion_argv: Some(vec!["main cli with spaces".into()]) };
        let output = dir.join("managed space.wav"); let mut writer = Generation::begin_with_context(&output, &co.repo, Some(context)).unwrap();
        writer.writer_file().unwrap().write_all(b"RIFF managed").unwrap();
        let Claimed::Yes { generation, receipt } = writer.finish().unwrap() else { panic!() }; drop(writer);
        assert_eq!(receipt.parent().unwrap(), normal_path(&fs::canonicalize(external.path()).unwrap()).unwrap());
        complete(&output, &generation, Some(&receipt), "accepted", "released").unwrap();
        assert!(!output.exists()); assert_eq!(claim(&receipt).state, "reclaimed");
    }
    fn managed(co: &Checkout, dir: &Path, external: &Path) -> (Generation, PathBuf) {
        let script = co.repo.join("register-managed.py");
        fs::write(&script, r#"import json,pathlib,sys
pairs=list(zip(sys.argv[1::2],sys.argv[2::2])); args=dict(pairs)
outputs=[v for k,v in pairs if k=='--output']
assert len(outputs)==2
assert all(not pathlib.Path(p).exists() for p in outputs)
c=json.loads(pathlib.Path(args['--receipt']).read_text())
assert c['state']=='writing' and c['output']==outputs[0]
assert str(pathlib.Path(outputs[0]).parent.resolve())==str(pathlib.Path(outputs[0]).parent)
assert str(pathlib.Path(args['--receipt']).resolve())==args['--receipt']
"#).unwrap();
        let python = std::env::var("FUNKOT_OWNER_TEST_PYTHON").unwrap_or_else(|_| "python".into());
        let context = Context { owner_receipt_dir: external.into(), owner_receipt_argv: vec![python, script.to_string_lossy().into_owned()], owner_completion_argv: Some(vec!["main cli".into()]) };
        let path = dir.join("Long Owned Output.wav");
        (Generation::begin_with_context(&path, &co.repo, Some(context)).unwrap(), path)
    }
    fn short_path(path: &Path) -> PathBuf {
        #[link(name = "kernel32")]
        unsafe extern "system" { fn GetShortPathNameW(long: *const u16, short: *mut u16, size: u32) -> u32; }
        let mut long: Vec<_> = path.as_os_str().encode_wide().collect(); long.push(0);
        let mut out = vec![0u16; 32768];
        let n = unsafe { GetShortPathNameW(long.as_ptr(), out.as_mut_ptr(), out.len() as u32) };
        assert!(n > 0 && (n as usize) < out.len(), "{}", io::Error::last_os_error());
        PathBuf::from(OsString::from_wide(&out[..n as usize]))
    }
    #[test]
    fn cross_owner_case_and_short_aliases_cannot_truncate_active_or_held_output() {
        let (_repo, co, dir) = fixture(); let external = tempfile::tempdir().unwrap(); let other = tempfile::tempdir().unwrap();
        let (mut writer, output) = managed(&co, &dir, external.path());
        writer.writer_file().unwrap().write_all(b"protected managed bytes").unwrap();
        let short = short_path(&output);
        eprintln!("8.3 alias exercised: {} => {}", output.display(), short.display());
        for path in [output.clone(), PathBuf::from(output.to_string_lossy().to_uppercase()), short.clone()] {
            assert!(Generation::begin_with_context(&path, &co.repo, None).is_err());
            let ctx = Context { owner_receipt_dir: other.path().into(), owner_receipt_argv: vec!["must not execute".into()], owner_completion_argv: None };
            assert!(Generation::begin_with_context(&path, &co.repo, Some(ctx)).is_err());
            assert_eq!(fs::read(&output).unwrap(), b"protected managed bytes");
        }
        let Claimed::Yes { generation, receipt } = writer.finish().unwrap() else { panic!() }; drop(writer);
        assert!(!dir.join(".Long Owned Output.wav.funkot-wav.lock").exists());
        for path in [output.clone(), short] { assert!(Generation::begin_with_context(&path, &co.repo, None).is_err()); }
        complete(&PathBuf::from(output.to_string_lossy().to_uppercase()), &generation, Some(&short_path(&receipt)), "accepted", "released").unwrap();
        assert!(!output.exists()); assert!(!dir.join(".Long Owned Output.wav.owned").exists());
        assert!(!dir.join(".Long Owned Output.wav.funkot-wav.lock").exists());
    }
    #[test]
    fn canonical_parent_registration_and_runtime_checkout() {
        if let Some(expected) = std::env::var_os("FUNKOT_OWNER_CHECKOUT_TEST") {
            assert_eq!(Checkout::this().repo, fs::canonicalize(expected).unwrap()); return;
        }
        let (repo, co, dir) = fixture(); fs::create_dir(repo.path().join(".git")).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "owned_wav::windows::tests::canonical_parent_registration_and_runtime_checkout"])
            .current_dir(&dir).env_remove("WORKSPACE_LIFECYCLE_CONTEXT").env("FUNKOT_OWNER_CHECKOUT_TEST", repo.path()).status().unwrap();
        assert!(status.success());
        fs::remove_dir(repo.path().join(".git")).unwrap();
        let external = tempfile::tempdir().unwrap();
        let alias = PathBuf::from(short_path(&dir).to_string_lossy().to_uppercase());
        let receipt_alias = PathBuf::from(short_path(external.path()).to_string_lossy().to_uppercase());
        let (mut writer, _) = managed(&co, &alias, &receipt_alias);
        writer.writer_file().unwrap().write_all(b"canonical").unwrap();
        let Claimed::Yes { generation, receipt } = writer.finish().unwrap() else { panic!() }; drop(writer);
        let c = claim(&receipt);
        assert_eq!(c.output.parent().unwrap(), normal_path(&fs::canonicalize(&dir).unwrap()).unwrap());
        complete(&c.output, &generation, Some(&receipt), "accepted", "released").unwrap();
    }
    fn set_junction(directory: &Path, target: &Path) {
        #[link(name = "kernel32")]
        unsafe extern "system" { fn DeviceIoControl(handle: *mut c_void, code: u32, input: *const c_void, input_len: u32, output: *mut c_void, output_len: u32, returned: *mut u32, overlapped: *mut c_void) -> i32; }
        let file = OpenOptions::new().access_mode(0x100).share_mode(7).custom_flags(0x02200000).open(directory).unwrap();
        let print: Vec<u16> = normal_path(target).unwrap().as_os_str().encode_wide().collect();
        let substitute: Vec<u16> = "\\??\\".encode_utf16().chain(print.iter().copied()).collect();
        let mut data = Vec::new(); data.extend_from_slice(&0xa0000003u32.to_le_bytes());
        data.extend_from_slice(&((8+(substitute.len()+print.len()+2)*2) as u16).to_le_bytes()); data.extend_from_slice(&0u16.to_le_bytes());
        for value in [0, (substitute.len()*2) as u16, ((substitute.len()+1)*2) as u16, (print.len()*2) as u16] { data.extend_from_slice(&value.to_le_bytes()); }
        for value in substitute.into_iter().chain([0]).chain(print).chain([0]) { data.extend_from_slice(&value.to_le_bytes()); }
        let mut returned = 0;
        assert_ne!(unsafe { DeviceIoControl(file.as_raw_handle(), 0x900a4, data.as_ptr().cast(), data.len() as u32, std::ptr::null_mut(), 0, &mut returned, std::ptr::null_mut()) }, 0, "{}", io::Error::last_os_error());
    }
    #[test]
    fn pinned_directory_reparse_change_cannot_redirect_any_namespace_operation() {
        let (_repo, co, dir) = fixture(); let protected = tempfile::tempdir().unwrap();
        fs::write(protected.path().join("sentinel.wav"), b"master bytes").unwrap();
        let (_claim_repo, claim_co, claim_dir) = fixture();
        let (receipt, _) = generate(&claim_co, &claim_dir); let saved_claim = claim(&receipt);
        let pinned = Directory::open(&dir, &co.repo).unwrap();
        set_junction(&dir, protected.path()); // succeeds despite the no-delete-sharing directory pin
        assert!(pinned.create_regular("new.wav").is_err());
        assert!(pinned.read_regular("sentinel.wav").is_err());
        assert!(pinned.exists("sentinel.wav").is_err());
        assert!(pinned.names().is_err());
        assert!(pinned.persist(".new.owned", &saved_claim, true).is_err());
        assert!(pinned.lock(&dir.join("x.wav")).is_err());
        assert!(Generation::begin_with_context(&dir.join("new.wav"), &co.repo, None).is_err());
        assert_eq!(fs::read(protected.path().join("sentinel.wav")).unwrap(), b"master bytes");
        assert_eq!(fs::read_dir(protected.path()).unwrap().count(), 1);
        drop(pinned); fs::remove_dir(&dir).unwrap();
    }
    #[test]
    fn unknown_marker_lock_and_malformed_receipt_preserve_objects() {
        for kind in ["marker", "lease", "malformed", "changed-marker"] {
            let (_repo, co, dir) = fixture(); let external = tempfile::tempdir().unwrap();
            if kind == "marker" || kind == "lease" {
                let name = if kind == "marker" { ".x.wav.owned" } else { ".x.wav.funkot-wav.lock" };
                fs::write(dir.join(name), b"manual object").unwrap();
                assert!(Generation::begin_with_context(&dir.join("x.wav"), &co.repo, None).is_err());
                assert_eq!(fs::read(dir.join(name)).unwrap(), b"manual object"); assert!(!dir.join("x.wav").exists());
            } else {
                let (mut writer, output) = managed(&co, &dir, external.path()); writer.writer_file().unwrap().write_all(b"held").unwrap();
                let Claimed::Yes { generation, receipt } = writer.finish().unwrap() else { panic!() }; drop(writer);
                let changed = if kind == "malformed" { receipt.clone() } else { dir.join(".Long Owned Output.wav.owned") };
                fs::write(&changed, b"unknown content").unwrap();
                assert!(complete(&output, &generation, Some(&receipt), "accepted", "released").is_err());
                assert_eq!(fs::read(output).unwrap(), b"held"); assert_eq!(fs::read(changed).unwrap(), b"unknown content");
            }
        }
    }
    #[test]
    fn killed_after_data_delete_retries_marker_and_keeps_original_proof() {
        if let Some(value) = std::env::var_os("FUNKOT_OWNER_KILL_COMPLETE") {
            let values: Vec<String> = serde_json::from_str(&value.to_string_lossy()).unwrap();
            complete(Path::new(&values[0]), &values[1], Some(Path::new(&values[2])), "accepted", "released").unwrap(); panic!("parent must kill this process");
        }
        let (_repo, co, dir) = fixture(); let external = tempfile::tempdir().unwrap();
        let (mut writer, output) = managed(&co, &dir, external.path()); writer.writer_file().unwrap().write_all(b"kill boundary").unwrap();
        let Claimed::Yes { generation, receipt } = writer.finish().unwrap() else { panic!() }; drop(writer);
        let original = claim(&receipt); let marker = external.path().join("delete-checkpoint");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "owned_wav::windows::tests::killed_after_data_delete_retries_marker_and_keeps_original_proof"])
            .env("FUNKOT_OWNER_KILL_COMPLETE", serde_json::to_string(&[output.to_string_lossy().into_owned(), generation.clone(), receipt.to_string_lossy().into_owned()]).unwrap())
            .env("FUNKOT_OWNER_DELETE_KILL_MARKER", &marker).spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !marker.exists() {
            assert!(child.try_wait().unwrap().is_none(), "child exited before kill boundary");
            if std::time::Instant::now() > deadline { child.kill().unwrap(); child.wait().unwrap(); panic!("delete checkpoint timeout"); }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!output.exists()); assert!(dir.join(".Long Owned Output.wav.owned").exists());
        child.kill().unwrap(); assert!(!child.wait().unwrap().success());
        assert!(!dir.join(".Long Owned Output.wav.funkot-wav.lock").exists());
        assert_eq!(claim(&receipt).state, "pending");
        complete(&output, &generation, Some(&receipt), "accepted", "released").unwrap();
        let after = claim(&receipt); assert_eq!(after.state, "reclaimed");
        assert_eq!(after.identity, original.identity); assert_eq!(after.sha256, original.sha256); assert_eq!(after.output_marker, original.output_marker);
        assert!(!dir.join(".Long Owned Output.wav.owned").exists());
    }

    #[test]
    fn control_git_receipts_work_without_allowing_foreign_repository_outputs() {
        if let Some(root) = std::env::var_os("FUNKOT_OWNER_GIT_LAYOUT_TEST") {
            let root = PathBuf::from(root); let task = root.join("task"); let co = Checkout::this();
            assert_eq!(co.repo, fs::canonicalize(&task).unwrap());
            let dir = task.join("testdata/synth"); let receipts = root.join("control/.git/workspace-lifecycle/owner-receipts/taskhash");
            assert!(Directory::open(&receipts, &co.repo).is_err());
            let receipt_dir = Directory::receipts(&receipts, &co.repo).unwrap(); receipt_dir.revalidate().unwrap();
            assert!(Directory::open(&root.join("control/foreign-output"), &co.repo).is_err());
            let (mut writer, output) = managed(&co, &dir, &receipts);
            writer.writer_file().unwrap().write_all(b"managed linked checkout").unwrap();
            let Claimed::Yes { generation, receipt } = writer.finish().unwrap() else { panic!() }; drop(writer);
            let busy = OpenOptions::new().write(true).open(&output).unwrap();
            assert!(complete(&output, &generation, Some(&receipt), "accepted", "released").is_err());
            assert_eq!(claim(&receipt).state, "pending"); drop(busy);
            co.open(SystemTime::now()); // Context startup retry uses receipt role too.
            assert_eq!(claim(&receipt).state, "reclaimed"); assert!(!output.exists());
            assert!(!dir.join(".Long Owned Output.wav.owned").exists());
            complete(&output, &generation, Some(&receipt), "accepted", "released").unwrap(); return;
        }
        let fixture = tempfile::tempdir().unwrap(); let root = fixture.path(); let task = root.join("task");
        let receipts = root.join("control/.git/workspace-lifecycle/owner-receipts/taskhash");
        fs::create_dir_all(&receipts).unwrap(); fs::create_dir_all(task.join("testdata/synth")).unwrap();
        fs::create_dir_all(root.join("control/foreign-output")).unwrap();
        fs::create_dir_all(root.join("control/.git/worktrees/task")).unwrap();
        fs::write(task.join(".git"), format!("gitdir: {}\n", root.join("control/.git/worktrees/task").display())).unwrap();
        let context = serde_json::json!({"repo":task,"owner_receipt_dir":receipts,"owner_receipt_argv":["must not execute during retry"],"owner_completion_argv":["main-cli"]});
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "owned_wav::windows::tests::control_git_receipts_work_without_allowing_foreign_repository_outputs"])
            .current_dir(&task).env("FUNKOT_OWNER_GIT_LAYOUT_TEST", root)
            .env("WORKSPACE_LIFECYCLE_CONTEXT", context.to_string()).status().unwrap();
        assert!(result.success());
    }

}
