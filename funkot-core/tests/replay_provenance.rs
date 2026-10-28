use std::path::Path;
use std::process::Command;

fn clean_env(command: &mut Command) -> &mut Command {
    command.env_remove("GIT_DIR").env_remove("GIT_WORK_TREE")
        .env_remove("GIT_CONFIG_COUNT").env_remove("GIT_CONFIG_KEY_0")
        .env_remove("GIT_CONFIG_VALUE_0")
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = clean_env(Command::new("git").current_dir(root).args(args)).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn build_revision_requires_a_clean_identified_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("source");
    std::fs::create_dir_all(repo.join("funkot-core")).unwrap();
    std::fs::write(repo.join("tracked"), b"original").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "tracked"]);
    git(&repo, &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture"]);
    let revision = git(&repo, &["rev-parse", "HEAD"]);
    let build = temp.path().join(if cfg!(windows) { "build.exe" } else { "build" });
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs");
    let output = Command::new("rustc").arg(source).arg("-o").arg(&build).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let run = |missing_git: bool| {
        let mut command = Command::new(&build);
        clean_env(&mut command).env("CARGO_MANIFEST_DIR", repo.join("funkot-core"));
        if missing_git { command.env("PATH", ""); }
        let output = command.output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    };
    assert!(run(false).contains(&format!("cargo:rustc-env=FUNKOT_REPLAY_BUILD_REVISION={revision}\n")));
    let other = temp.path().join("other");
    std::fs::create_dir(&other).unwrap();
    git(&other, &["init", "-q"]);
    git(&other, &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-qm", "other"]);
    let spoofed = Command::new(&build).env("CARGO_MANIFEST_DIR", repo.join("funkot-core"))
        .env("GIT_DIR", other.join(".git")).env("GIT_WORK_TREE", &repo)
        .output().unwrap();
    assert!(spoofed.status.success());
    assert!(String::from_utf8(spoofed.stdout).unwrap().contains(&format!("cargo:rustc-env=FUNKOT_REPLAY_BUILD_REVISION={revision}\n")));
    std::fs::write(repo.join("tracked"), b"changed").unwrap();
    assert!(run(false).contains("cargo:rustc-env=FUNKOT_REPLAY_BUILD_REVISION=\n"));
    std::fs::write(repo.join("tracked"), b"original").unwrap();
    assert!(run(true).contains("cargo:rustc-env=FUNKOT_REPLAY_BUILD_REVISION=\n"));
}
