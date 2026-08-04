//! Ground-truth intro/outro section-length labels, for offline evaluation.
//!
//! This is Stage 0 of the section-length rework: before touching
//! `analysis.rs`, we need a way to measure whether a change helps. Labels
//! are the input to that measurement (`examples/eval_sections.rs` is the
//! consumer). Tracks are identified by [`crate::cache::content_hash`], not
//! by path, so labels survive file moves/renames and match whatever the
//! cache already keys on.
//!
//! On-disk format: `testdata/labels.tsv`, tab-separated, first non-comment/
//! non-blank line is the header:
//!
//! ```text
//! content_hash	file_name	intro_best	intro_ok	outro_best	outro_ok	note
//! ```
//!
//! - `intro_best` / `outro_best`: the single best bar count (integer), or
//!   empty. Empty means that side has not been labeled yet — e.g. a row
//!   written for its `note` alone (see `funkot-cli`'s `--label-sections`
//!   `s`/`q` handling), or a hand-edited file recording only one side so
//!   far. A row with both sides empty is legal and only carries a note.
//! - `intro_ok` / `outro_ok`: pipe-separated set of additionally acceptable
//!   bar counts (e.g. `32|48`). Empty means only `best` is acceptable (or,
//!   when `best` is itself empty, that there is no acceptable set at all).
//! - `note`: free text, may be empty. On load it may itself contain tabs
//!   (it is always the last field on the line, so [`load_labels`] doesn't
//!   need to split on them); [`save_labels`] replaces any tabs/newlines it
//!   finds with spaces before writing, so a note is not guaranteed to round
//!   -trip byte-for-byte through a save — only its tab/newline-free reading
//!   is preserved. `note` may additionally carry whitespace-separated
//!   `KEY:VALUE` tags mixed in with free text (see [`SectionLabel::note_tags`]
//!   for the exact grammar and [`SectionLabel::dup_group`] for the one tag
//!   this module itself interprets).
//! - Lines that are empty or start with `#` (after trimming) are ignored
//!   wherever they occur.
//!
//! `outro_best` / `outro_ok` are the *musical* main→outro structural
//! boundary in bars from the end, not `TrackAnalysis::outro_bars` (which
//! additionally includes a DJ mix lead-in that is not a fixed offset from
//! the boundary). Compare against `TrackAnalysis::outro_structure_bars`
//! instead — see `eval_sections.rs`.

use std::fs;
use std::path::Path;

use crate::{Error, Result};

/// Column count of a well-formed data row (see module docs for the layout).
const COLUMN_COUNT: usize = 7;

/// One track's hand-verified intro/outro section lengths.
#[derive(Debug, Clone, PartialEq)]
pub struct SectionLabel {
    /// [`crate::cache::content_hash`] of the labeled audio file.
    pub hash: String,
    /// Original file name, informational only (not used to identify the track).
    pub file_name: String,
    /// Best intro length in bars, or `None` if the intro side hasn't been
    /// labeled yet (see module docs).
    pub intro_best: Option<u32>,
    /// Additional acceptable intro lengths, as parsed from the file (does
    /// not necessarily include `intro_best`; callers that need the full
    /// tolerant set should union it in).
    pub intro_ok: Vec<u32>,
    /// Best outro *structural boundary* length in bars (see module docs),
    /// or `None` if the outro side hasn't been labeled yet.
    pub outro_best: Option<u32>,
    /// Additional acceptable outro lengths, as parsed from the file.
    pub outro_ok: Vec<u32>,
    /// Free-text note, may be empty.
    pub note: String,
}

impl SectionLabel {
    /// `intro_ok` plus `intro_best` (if labeled), deduplicated. This is the
    /// set an estimate should be checked against for "tolerant" correctness.
    /// If `intro_best` is `None`, this is just the deduplicated `intro_ok`
    /// list.
    pub fn intro_ok_set(&self) -> Vec<u32> {
        ok_set(self.intro_best, &self.intro_ok)
    }

    /// `outro_ok` plus `outro_best` (if labeled), deduplicated.
    pub fn outro_ok_set(&self) -> Vec<u32> {
        ok_set(self.outro_best, &self.outro_ok)
    }

    /// Extracts `KEY:VALUE` tags from [`Self::note`], in the order they
    /// appear, duplicate keys allowed.
    ///
    /// `note` is split on whitespace; a token is a tag only if its part
    /// before the first ASCII `:` is a valid key (see below) and the part
    /// after is non-empty. Tokens that don't match (including a `KEY:` with
    /// nothing after the colon) are silently treated as free text — a note
    /// may freely mix tags and prose, e.g. `"PH:half ズレている"` yields one
    /// `PH` tag plus the trailing words being ignored here (they remain part
    /// of `note` itself; this method only extracts tags).
    ///
    /// A valid key starts with an ASCII uppercase letter and has at least
    /// one more character from `[A-Z0-9_]` (so at least two characters
    /// total). This is deliberately strict so that ordinary Japanese or
    /// English free text — and incidental colon-bearing strings like a
    /// `http://` URL, whose scheme is lowercase — never accidentally parses
    /// as a tag. Do not loosen it.
    ///
    /// The tag vocabulary itself (`PH:`, `GRID:`, `DUP:`, ...) is an
    /// operational convention among people writing labels, not something
    /// this parser knows about or validates — it only recognizes the
    /// `KEY:VALUE` shape. Of that vocabulary, only `DUP:` is currently read
    /// by code, via [`Self::dup_group`].
    pub fn note_tags(&self) -> Vec<(String, String)> {
        let mut tags = Vec::new();
        for token in self.note.split_whitespace() {
            let Some(colon) = token.find(':') else {
                continue;
            };
            let (key, rest) = token.split_at(colon);
            let value = &rest[1..];
            if is_tag_key(key) && !value.is_empty() {
                tags.push((key.to_string(), value.to_string()));
            }
        }
        tags
    }

    /// The `DUP:` tag value from [`Self::note`], if present (first match
    /// wins if there happen to be several). Used by `eval_sections.rs` to
    /// group re-releases/edits of the same underlying track so a fold split
    /// never puts one version in training and another in test.
    pub fn dup_group(&self) -> Option<String> {
        self.note_tags()
            .into_iter()
            .find(|(k, _)| k == "DUP")
            .map(|(_, v)| v)
    }
}

/// Whether `key` is a valid [`SectionLabel::note_tags`] key: an ASCII
/// uppercase letter followed by one or more of `[A-Z0-9_]` (two characters
/// minimum total).
fn is_tag_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() => {}
        _ => return false,
    }
    let rest: Vec<char> = chars.collect();
    !rest.is_empty() && rest.iter().all(|&c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn ok_set(best: Option<u32>, extra: &[u32]) -> Vec<u32> {
    let mut set = Vec::with_capacity(extra.len() + 1);
    if let Some(best) = best {
        set.push(best);
    }
    for &v in extra {
        if !set.contains(&v) {
            set.push(v);
        }
    }
    set
}

/// Load labels from a TSV file. See module docs for the format.
pub fn load_labels(path: &Path) -> Result<Vec<SectionLabel>> {
    let contents = fs::read_to_string(path).map_err(|e| {
        Error::Labels(format!("cannot read labels file '{}': {e}", path.display()))
    })?;
    parse_labels(&contents)
}

/// Parse label TSV text (split out from [`load_labels`] for testing without a file).
fn parse_labels(contents: &str) -> Result<Vec<SectionLabel>> {
    let mut labels = Vec::new();
    let mut seen_header = false;
    for (idx, raw_line) in contents.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !seen_header {
            // First non-comment/non-blank line is the header; not validated
            // beyond being present, so hand-edited files with a slightly
            // different header wording still load.
            seen_header = true;
            continue;
        }
        labels.push(parse_row(raw_line, line_no)?);
    }
    Ok(labels)
}

/// Parse one data row. `note` is allowed to contain tabs, so only the first
/// six tabs are split on.
fn parse_row(raw_line: &str, line_no: usize) -> Result<SectionLabel> {
    let fields: Vec<&str> = raw_line.splitn(COLUMN_COUNT, '\t').collect();
    if fields.len() != COLUMN_COUNT {
        return Err(Error::Labels(format!(
            "labels line {line_no}: expected {COLUMN_COUNT} tab-separated columns, got {}",
            fields.len()
        )));
    }
    let hash = fields[0].trim().to_string();
    if hash.is_empty() {
        return Err(Error::Labels(format!(
            "labels line {line_no}: content_hash column is empty"
        )));
    }
    let file_name = fields[1].trim().to_string();
    let intro_best = parse_u32_opt(fields[2], line_no, "intro_best")?;
    let intro_ok = parse_ok_list(fields[3], line_no, "intro_ok")?;
    let outro_best = parse_u32_opt(fields[4], line_no, "outro_best")?;
    let outro_ok = parse_ok_list(fields[5], line_no, "outro_ok")?;
    let note = fields[6].to_string();
    Ok(SectionLabel {
        hash,
        file_name,
        intro_best,
        intro_ok,
        outro_best,
        outro_ok,
        note,
    })
}

/// Parses a `best` column: empty (after trim) means "not labeled yet"
/// (`None`); anything else must be a valid non-negative integer.
fn parse_u32_opt(field: &str, line_no: usize, name: &str) -> Result<Option<u32>> {
    let trimmed = field.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse::<u32>()
        .map(Some)
        .map_err(|e| {
            Error::Labels(format!(
                "labels line {line_no}: {name} '{field}' is not a non-negative integer: {e}"
            ))
        })
}

fn parse_ok_list(field: &str, line_no: usize, name: &str) -> Result<Vec<u32>> {
    let field = field.trim();
    if field.is_empty() {
        return Ok(Vec::new());
    }
    field
        .split('|')
        .map(|part| {
            part.trim().parse::<u32>().map_err(|e| {
                Error::Labels(format!(
                    "labels line {line_no}: {name} entry '{part}' is not a non-negative integer: {e}"
                ))
            })
        })
        .collect()
}

/// Serialize labels as TSV text (header + one row per label, in order given).
fn render_labels(labels: &[SectionLabel]) -> String {
    let mut out = String::from(
        "content_hash\tfile_name\tintro_best\tintro_ok\toutro_best\toutro_ok\tnote\n",
    );
    for label in labels {
        out.push_str(&render_row(label));
        out.push('\n');
    }
    out
}

fn render_row(label: &SectionLabel) -> String {
    // Defensively strip tabs/newlines from fields that aren't supposed to
    // carry them, so a hand-typed file_name/note can't corrupt the column
    // layout on the next save. `note` is the last column so it could in
    // principle carry tabs safely, but keeping it single-line matches what
    // `load_labels` writes back out and is easiest to hand-edit.
    let file_name = sanitize_field(&label.file_name);
    let note = sanitize_field(&label.note);
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
        sanitize_field(&label.hash),
        file_name,
        render_opt(label.intro_best),
        render_ok_list(&label.intro_ok),
        render_opt(label.outro_best),
        render_ok_list(&label.outro_ok),
        note,
    )
}

fn sanitize_field(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

/// Renders a `best` column: `None` as an empty field, `Some(v)` as `v`.
fn render_opt(v: Option<u32>) -> String {
    v.map(|v| v.to_string()).unwrap_or_default()
}

fn render_ok_list(values: &[u32]) -> String {
    values
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("|")
}

/// Write labels as TSV, overwriting `path`. Creates the parent directory if needed.
pub fn save_labels(path: &Path, labels: &[SectionLabel]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| {
                Error::Labels(format!(
                    "cannot create labels directory '{}': {e}",
                    parent.display()
                ))
            })?;
        }
    }
    fs::write(path, render_labels(labels))
        .map_err(|e| Error::Labels(format!("cannot write labels file '{}': {e}", path.display())))
}

/// Insert or replace a label by `hash`, then persist the full set to `path`.
///
/// If `path` doesn't exist yet, it is created with just this one label.
/// Row order is otherwise preserved; a replaced label keeps its original
/// position.
pub fn upsert_label(path: &Path, label: SectionLabel) -> Result<()> {
    let mut labels = if path.exists() {
        load_labels(path)?
    } else {
        Vec::new()
    };
    match labels.iter_mut().find(|l| l.hash == label.hash) {
        Some(existing) => *existing = label,
        None => labels.push(label),
    }
    save_labels(path, &labels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Process-unique scratch file path under the system temp dir.
    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "funkot-labels-test-{tag}-{}-{n}.tsv",
                std::process::id()
            ));
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn sample_labels() -> Vec<SectionLabel> {
        vec![
            SectionLabel {
                hash: "aaaa1111".to_string(),
                file_name: "track-a.flac".to_string(),
                intro_best: Some(32),
                intro_ok: vec![48],
                outro_best: Some(32),
                outro_ok: vec![],
                note: "clean drop".to_string(),
            },
            SectionLabel {
                hash: "bbbb2222".to_string(),
                file_name: "track-b.flac".to_string(),
                intro_best: Some(16),
                intro_ok: vec![],
                outro_best: Some(48),
                outro_ok: vec![32, 64],
                note: String::new(),
            },
        ]
    }

    #[test]
    fn round_trip_save_and_load() {
        let f = TempFile::new("roundtrip");
        let labels = sample_labels();
        save_labels(f.path(), &labels).unwrap();
        let loaded = load_labels(f.path()).unwrap();
        assert_eq!(loaded, labels);
    }

    #[test]
    fn parses_comments_blank_lines_and_ok_sets() {
        let text = "\
# this is a comment
content_hash\tfile_name\tintro_best\tintro_ok\toutro_best\toutro_ok\tnote

aaaa1111\ttrack-a.flac\t32\t48\t32\t\tclean drop
# another comment in the middle
bbbb2222\ttrack-b.flac\t16\t\t48\t32|64\t
";
        let labels = parse_labels(text).unwrap();
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0].hash, "aaaa1111");
        assert_eq!(labels[0].intro_ok, vec![48]);
        assert_eq!(labels[0].outro_ok, Vec::<u32>::new());
        assert_eq!(labels[1].outro_ok, vec![32, 64]);
        assert_eq!(labels[1].intro_ok_set(), vec![16]);
        assert_eq!(labels[1].outro_ok_set(), vec![48, 32, 64]);
    }

    #[test]
    fn upsert_replaces_matching_hash_and_keeps_others() {
        let f = TempFile::new("upsert-replace");
        save_labels(f.path(), &sample_labels()).unwrap();

        let mut replacement = sample_labels()[0].clone();
        replacement.intro_best = Some(64);
        replacement.note = "revised".to_string();
        upsert_label(f.path(), replacement.clone()).unwrap();

        let loaded = load_labels(f.path()).unwrap();
        assert_eq!(loaded.len(), 2, "row count must not change on replace");
        assert_eq!(loaded[0], replacement);
        assert_eq!(loaded[1], sample_labels()[1]);
    }

    #[test]
    fn upsert_appends_new_hash() {
        let f = TempFile::new("upsert-append");
        save_labels(f.path(), &sample_labels()).unwrap();

        let new_label = SectionLabel {
            hash: "cccc3333".to_string(),
            file_name: "track-c.flac".to_string(),
            intro_best: Some(8),
            intro_ok: vec![],
            outro_best: Some(8),
            outro_ok: vec![],
            note: String::new(),
        };
        upsert_label(f.path(), new_label.clone()).unwrap();

        let loaded = load_labels(f.path()).unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[2], new_label);
    }

    #[test]
    fn upsert_creates_a_missing_file() {
        let f = TempFile::new("upsert-new-file");
        assert!(!f.path().exists());
        let label = sample_labels().into_iter().next().unwrap();
        upsert_label(f.path(), label.clone()).unwrap();
        let loaded = load_labels(f.path()).unwrap();
        assert_eq!(loaded, vec![label]);
    }

    #[test]
    fn rejects_row_with_too_few_columns() {
        let text = "content_hash\tfile_name\tintro_best\tintro_ok\toutro_best\toutro_ok\tnote\naaaa\tname\t32\t\t32\n";
        let err = parse_labels(text).unwrap_err();
        match err {
            Error::Labels(msg) => {
                assert!(msg.contains("line 2"), "message was: {msg}");
                assert!(msg.contains('7'), "message was: {msg}");
            }
            other => panic!("expected Error::Labels, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_numeric_best() {
        let text = "content_hash\tfile_name\tintro_best\tintro_ok\toutro_best\toutro_ok\tnote\naaaa\tname\tabc\t\t32\t\t\n";
        let err = parse_labels(text).unwrap_err();
        match err {
            Error::Labels(msg) => {
                assert!(msg.contains("intro_best"), "message was: {msg}");
            }
            other => panic!("expected Error::Labels, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_numeric_ok_entry() {
        let text = "content_hash\tfile_name\tintro_best\tintro_ok\toutro_best\toutro_ok\tnote\naaaa\tname\t32\t48|bad\t32\t\t\n";
        let err = parse_labels(text).unwrap_err();
        match err {
            Error::Labels(msg) => {
                assert!(msg.contains("intro_ok"), "message was: {msg}");
            }
            other => panic!("expected Error::Labels, got {other:?}"),
        }
    }

    #[test]
    fn parses_one_side_empty_as_none() {
        let text = "\
content_hash\tfile_name\tintro_best\tintro_ok\toutro_best\toutro_ok\tnote
aaaa1111\ttrack-a.flac\t\t\t32\t\toutro only so far
";
        let labels = parse_labels(text).unwrap();
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].intro_best, None);
        assert_eq!(labels[0].outro_best, Some(32));
        assert_eq!(labels[0].note, "outro only so far");
    }

    #[test]
    fn parses_both_sides_empty_as_note_only_row() {
        let text = "\
content_hash\tfile_name\tintro_best\tintro_ok\toutro_best\toutro_ok\tnote
aaaa1111\ttrack-a.flac\t\t\t\t\tconstruction boundary, no candidate fits
";
        let labels = parse_labels(text).unwrap();
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].intro_best, None);
        assert_eq!(labels[0].outro_best, None);
        assert_eq!(labels[0].intro_ok, Vec::<u32>::new());
        assert_eq!(labels[0].outro_ok, Vec::<u32>::new());
        assert_eq!(labels[0].note, "construction boundary, no candidate fits");
    }

    #[test]
    fn round_trip_save_and_load_with_none_best() {
        let f = TempFile::new("roundtrip-none");
        let labels = vec![
            SectionLabel {
                hash: "aaaa1111".to_string(),
                file_name: "track-a.flac".to_string(),
                intro_best: None,
                intro_ok: vec![],
                outro_best: Some(32),
                outro_ok: vec![48],
                note: "intro not labeled yet".to_string(),
            },
            SectionLabel {
                hash: "bbbb2222".to_string(),
                file_name: "track-b.flac".to_string(),
                intro_best: None,
                intro_ok: vec![],
                outro_best: None,
                outro_ok: vec![],
                note: "note only, nothing labeled".to_string(),
            },
        ];
        save_labels(f.path(), &labels).unwrap();
        let loaded = load_labels(f.path()).unwrap();
        assert_eq!(loaded, labels);
    }

    #[test]
    fn ok_set_is_ok_list_alone_when_best_is_none() {
        let label = SectionLabel {
            hash: "aaaa1111".to_string(),
            file_name: "track-a.flac".to_string(),
            intro_best: None,
            intro_ok: vec![16, 32],
            outro_best: None,
            outro_ok: vec![],
            note: String::new(),
        };
        assert_eq!(label.intro_ok_set(), vec![16, 32]);
        assert_eq!(label.outro_ok_set(), Vec::<u32>::new());
    }

    #[test]
    fn existing_numeric_only_tsv_still_loads() {
        // Regression guard for backward compatibility: a file written before
        // best columns could be empty must still parse identically.
        let text = "\
content_hash\tfile_name\tintro_best\tintro_ok\toutro_best\toutro_ok\tnote
aaaa1111\ttrack-a.flac\t32\t48\t32\t\tclean drop
bbbb2222\ttrack-b.flac\t16\t\t48\t32|64\t
";
        let labels = parse_labels(text).unwrap();
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0].intro_best, Some(32));
        assert_eq!(labels[0].outro_best, Some(32));
        assert_eq!(labels[1].intro_best, Some(16));
        assert_eq!(labels[1].outro_best, Some(48));
    }

    fn label_with_note(note: &str) -> SectionLabel {
        SectionLabel {
            hash: "aaaa1111".to_string(),
            file_name: "track-a.flac".to_string(),
            intro_best: Some(32),
            intro_ok: vec![],
            outro_best: Some(32),
            outro_ok: vec![],
            note: note.to_string(),
        }
    }

    #[test]
    fn note_tags_japanese_free_text_only_yields_no_tags() {
        let label = label_with_note("4拍めにクリックがある");
        assert_eq!(label.note_tags(), Vec::new());
        assert_eq!(label.dup_group(), None);
    }

    #[test]
    fn note_tags_mixed_with_free_text() {
        let label = label_with_note("PH:half クリックが4拍めで鳴る DUP:ivy-remix");
        assert_eq!(
            label.note_tags(),
            vec![
                ("PH".to_string(), "half".to_string()),
                ("DUP".to_string(), "ivy-remix".to_string()),
            ]
        );
        assert_eq!(label.dup_group(), Some("ivy-remix".to_string()));
    }

    #[test]
    fn note_tags_rejects_lowercase_key_and_url_like_tokens() {
        let label = label_with_note("ph:half http://example.com/x GRID:ok");
        assert_eq!(
            label.note_tags(),
            vec![("GRID".to_string(), "ok".to_string())]
        );
    }

    #[test]
    fn note_tags_rejects_single_char_key_and_empty_value() {
        let label = label_with_note("X:5 PH: GRID:ok");
        assert_eq!(
            label.note_tags(),
            vec![("GRID".to_string(), "ok".to_string())]
        );
    }

    #[test]
    fn note_tags_extracts_dup_group() {
        let label = label_with_note("DUP:eternal-light-v2");
        assert_eq!(label.dup_group(), Some("eternal-light-v2".to_string()));
    }

    #[test]
    fn existing_tsv_with_tagged_note_round_trips() {
        let f = TempFile::new("roundtrip-tagged-note");
        let labels = vec![label_with_note("PH:half DUP:group-a note text")];
        save_labels(f.path(), &labels).unwrap();
        let loaded = load_labels(f.path()).unwrap();
        assert_eq!(loaded, labels);
        assert_eq!(loaded[0].dup_group(), Some("group-a".to_string()));
    }
}
