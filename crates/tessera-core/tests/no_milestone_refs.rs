//! Integration test: no milestone references may appear anywhere in the
//! `crates/` tree (Rust comments, doc comments, or user-visible strings).
//!
//! Rule of the project owner: milestone names (M3, M4, ...) and the word
//! "milestone" belong only in `docs/` (specs, architecture, roadmap, README).
//! Code under `crates/` must describe behavior and constraints instead.
//!
//! This test walks every `*.rs` file under the `crates/` directory (the parent
//! of this crate) and fails if any line references a milestone.
//!
//! IMPORTANT: this guard intentionally skips its *own* source file (compared by
//! canonical path). That is not an allowlist of a milestone — rather, the guard
//! necessarily contains in its source the very words it detects (its failure
//! message says "milestone", and its unit test feeds it `M3` / `milestone`
//! samples). Everything else under `crates/` is scanned with no exemptions.

use std::fs;
use std::path::Path;

/// True if `line` contains a standalone `M<digits>` token (e.g. `M3`) where the
/// characters immediately before and after the token are not ASCII letters or
/// digits (or are line boundaries). So `M3`, `(M3)`, and `M3:` match, while
/// `ITEM3` and `Mode2` do not.
fn has_m_token(line: &str) -> bool {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        if bytes[i] == b'M' {
            let mut j = i + 1;
            while j < len && bytes[j].is_ascii_digit() {
                j += 1;
            }
            // Requires at least one digit after the M.
            if j > i + 1 {
                let left_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
                let right_ok = j >= len || !bytes[j].is_ascii_alphanumeric();
                if left_ok && right_ok {
                    return true;
                }
            }
            // Skip the whole M+digits run either way.
            i = j;
        } else {
            i += 1;
        }
    }
    false
}

/// True if `line` contains the case-insensitive word `milestone` (matching
/// plurals like "milestones" too, since the preceding character boundary is the
/// only requirement).
fn has_milestone_word(line: &str) -> bool {
    let lowered = line.to_ascii_lowercase();
    let bytes = lowered.as_bytes();
    const NEEDLE: &[u8] = b"milestone";
    let needle_len = NEEDLE.len();
    let len = bytes.len();
    if len < needle_len {
        return false;
    }
    for i in 0..=len - needle_len {
        if &bytes[i..i + needle_len] == NEEDLE {
            let left_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            if left_ok {
                return true;
            }
        }
    }
    false
}

/// True if `line` contains any milestone reference.
fn line_has_milestone_ref(line: &str) -> bool {
    has_m_token(line) || has_milestone_word(line)
}

/// Recurse through `dir`, collecting every milestone hit (`path:line: text`)
/// from `*.rs` files into `hits`. `self_path` is the canonical path of this
/// test file, which is skipped (see module docs).
fn scan_dir(dir: &Path, self_path: &Path, hits: &mut Vec<String>) {
    if !dir.is_dir() {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir(&path, self_path, hits);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            let canonical = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if canonical == *self_path {
                continue; // the guard's own file (see module docs).
            }
            let content = match fs::read_to_string(&path) {
                Ok(content) => content,
                Err(_) => continue,
            };
            for (index, line) in content.lines().enumerate() {
                if line_has_milestone_ref(line) {
                    hits.push(format!("{}:{}: {}", path.display(), index + 1, line));
                }
            }
        }
    }
}

#[test]
fn no_milestone_references_in_crates() {
    // `crates/` is the parent of this crate's manifest directory.
    let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    // This file lives at <manifest>/tests/no_milestone_refs.rs; build that from
    // the absolute manifest dir so the path always resolves regardless of CWD.
    let self_path = fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("no_milestone_refs.rs"),
    )
    .expect("canonicalize own source path");

    let mut hits = Vec::new();
    scan_dir(&crates_dir, &self_path, &mut hits);

    assert!(
        hits.is_empty(),
        "milestone references are not allowed in crates/ code or comments; \
         describe the behavior instead (see docs/ for milestone tracking)\n\n{}",
        hits.join("\n")
    );
}

#[test]
fn matcher_catches_real_milestone_references() {
    assert!(line_has_milestone_ref("// M3 scope"));
    assert!(line_has_milestone_ref("a milestone later"));
    assert!(line_has_milestone_ref("(M4) script"));
    assert!(line_has_milestone_ref("per M3: deterministic"));
    assert!(line_has_milestone_ref("later milestones"));
    assert!(line_has_milestone_ref("Milestone tracking"));
}

#[test]
fn matcher_ignores_lookalikes() {
    assert!(!line_has_milestone_ref("ITEM3"));
    assert!(!line_has_milestone_ref("Mode2"));
    assert!(!line_has_milestone_ref("run 2 tunes"));
    assert!(!line_has_milestone_ref("file m3_lowercase.rs"));
    assert!(!line_has_milestone_ref("a3"));
}
