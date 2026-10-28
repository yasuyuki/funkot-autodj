use std::path::{Path, PathBuf};
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().to_ascii_uppercase().starts_with("GIT_") { command.env_remove(name); }
    }
    let output = command.arg("-c").arg(format!("safe.directory={}", root.display()))
        .arg("-C").arg(root).args(args).output().ok()?;
    if !output.status.success() { return None; }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

fn main() {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).parent().unwrap().to_owned();
    for name in ["PATH", "GIT_DIR", "GIT_WORK_TREE"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    println!("cargo:rerun-if-changed={}", root.join(".git").display());
    println!("cargo:rerun-if-changed={}", root.join("funkot-core").display());
    println!("cargo:rerun-if-changed={}", root.join("Cargo.toml").display());
    println!("cargo:rerun-if-changed={}", root.join("Cargo.lock").display());
    for name in ["HEAD", "index"] {
        if let Some(path) = git(&root, &["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={}", root.join(path).display());
        }
    }
    if let Some(reference) = git(&root, &["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&root, &["rev-parse", "--git-path", &reference]) {
            println!("cargo:rerun-if-changed={}", root.join(path).display());
        }
    }
    let repository_root = git(&root, &["rev-parse", "--show-toplevel"])
        .and_then(|path| PathBuf::from(path).canonicalize().ok());
    let revision = git(&root, &["rev-parse", "--verify", "HEAD^{commit}"])
        .filter(|value| value.len() == 40 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        .filter(|_| root.canonicalize().ok() == repository_root && repository_root.is_some())
        .filter(|_| git(&root, &["status", "--porcelain", "--untracked-files=all"]).as_deref() == Some(""))
        .unwrap_or_default();
    println!("cargo:rustc-env=FUNKOT_REPLAY_BUILD_REVISION={revision}");
}
