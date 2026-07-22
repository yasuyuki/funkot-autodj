//! Playlist file loading (m3u-compatible: one path per line).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Load a playlist file: one path per line; blank lines and `#` comments ignored.
/// Relative paths are resolved against the playlist file's directory.
pub fn load_playlist_file(path: &Path) -> Result<Vec<PathBuf>> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read playlist file {}", path.display()))?;
    let base = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    parse_playlist_lines(&contents, base)
}

/// Parse playlist text with relative paths resolved against `base_dir`.
pub fn parse_playlist_lines(contents: &str, base_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut entries = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let path = Path::new(line);
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            base_dir.join(path)
        };
        entries.push(resolved);
    }
    validate_paths_exist(&entries)?;
    Ok(entries)
}

/// Fail fast if any path is missing; message lists all missing files.
pub fn validate_paths_exist(paths: &[PathBuf]) -> Result<()> {
    let missing: Vec<&PathBuf> = paths.iter().filter(|p| !p.exists()).collect();
    if missing.is_empty() {
        return Ok(());
    }
    let list = missing
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    bail!("missing playlist entries: {list}");
}
