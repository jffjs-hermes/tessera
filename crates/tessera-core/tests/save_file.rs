//! File-layer tests for the atomic save writer, backup rotation, stale-temp
//! sweep, and path load.
//!
//! Everything here is non-panicking in the code under test: bad input comes
//! back as `Err(SaveError::…)`, never a panic, and the tests assert the error
//! variants rather than unwinding.

use std::fs;
use std::path::Path;

use tempfile::TempDir;

use tessera_core::rng::SimRng;
use tessera_core::save::{
    SAVE_FORMAT_VERSION, SaveData, SaveError, load, save_to_bytes, sweep_stale_tmp, write_atomic,
};

fn sample_data(clock: u64) -> SaveData {
    SaveData {
        version: SAVE_FORMAT_VERSION,
        zone_id: "start".to_string(),
        next_entity_bits: vec![1, 2, 3],
        entities: vec![],
        player: None,
        clock,
        actors: vec![],
        rng: SimRng::from_seed(1234),
        streams: Default::default(),
        player_turns: 0,
        facing: (0, 0),
    }
}

/// Number of stale-temp-pattern files (`*.tmp` / `*.tmp.*`) in `dir`.
fn tmp_files(dir: &Path) -> usize {
    fs::read_dir(dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.ends_with(".tmp") || name.contains(".tmp.")
        })
        .count()
}

#[test]
fn atomic_happy_path_leaves_save_and_no_tmp() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("quicksave");

    write_atomic(&path, &sample_data(1)).expect("write");

    // The save exists and decodes back to exactly what was written.
    assert_eq!(load(&path).expect("load"), sample_data(1));
    // A first write leaves no tmp leftovers and no backup.
    assert_eq!(tmp_files(dir.path()), 0);
    assert!(!path.with_extension("bak").exists());
}

#[test]
fn bak_rotation_produces_loadable_backup() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("quicksave");

    write_atomic(&path, &sample_data(1)).expect("first write");
    write_atomic(&path, &sample_data(2)).expect("second write");

    let bak = path.with_extension("bak");
    assert!(bak.exists(), ".bak must exist after a second write");
    // The backup decodes to the previous save; the live save holds the new one.
    assert_eq!(load(&bak).expect("backup loads"), sample_data(1));
    assert_eq!(load(&path).expect("live loads"), sample_data(2));
    assert_eq!(tmp_files(dir.path()), 0);
}

/// Failure-injected: with the directory read-only the tmp write still succeeds
/// (the tmp file pre-exists and is writable) but the final rename fails, so the
/// prior save must be preserved untouched. Unix-only — Windows has no
/// Unix-style directory write permission to inject with.
#[cfg(unix)]
#[test]
fn unwritable_rename_preserves_prior_save() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("quicksave");
    write_atomic(&path, &sample_data(1)).expect("first write");

    // Pre-create the tmp file so `File::create` truncates an existing writable
    // file instead of needing to create a new entry in a read-only directory —
    // that lets us reach the final-rename failure specifically.
    fs::write(path.with_extension("tmp"), b"stale").expect("pre-create tmp");

    // Make the directory read-only: creation/rename inside it now fails.
    let mut perms = fs::metadata(dir.path())
        .expect("dir metadata")
        .permissions();
    perms.set_mode(0o555);
    fs::set_permissions(dir.path(), perms).expect("make dir read-only");

    let result = write_atomic(&path, &sample_data(2));

    // Restore permissions before asserting so tempdir cleanup always works.
    let mut perms = fs::metadata(dir.path())
        .expect("dir metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(dir.path(), perms).expect("restore dir permissions");

    assert!(matches!(result, Err(SaveError::WriteFailed { .. })));
    // The prior save is intact and still decodes to the first data.
    assert_eq!(load(&path).expect("prior save intact"), sample_data(1));
    // No backup was produced, and the failed write left a tmp to be swept.
    assert!(!path.with_extension("bak").exists());
    assert!(path.with_extension("tmp").exists());
}

#[test]
fn stale_tmp_sweep_removes_tmp_and_preserves_bak() {
    let dir = TempDir::new().expect("tempdir");
    let d = dir.path();
    fs::write(d.join("quicksave.tmp"), b"a").expect("tmp");
    fs::write(d.join("quicksave.tmp.1"), b"b").expect("tmp.1");
    fs::write(d.join("quicksave.tmp.20191231"), b"c").expect("tmp.date");
    fs::write(d.join("quicksave.bak"), b"backup").expect("bak");
    fs::write(d.join("quicksave"), b"save").expect("save");
    fs::write(d.join("notes.txt"), b"keep").expect("control");

    let removed = sweep_stale_tmp(d).expect("sweep");
    assert_eq!(removed, 3);

    // All three tmp-pattern files are gone.
    assert!(!d.join("quicksave.tmp").exists());
    assert!(!d.join("quicksave.tmp.1").exists());
    assert!(!d.join("quicksave.tmp.20191231").exists());
    // The .bak and everything else survive.
    assert!(d.join("quicksave.bak").exists());
    assert!(d.join("quicksave").exists());
    assert!(d.join("notes.txt").exists());
}

#[test]
fn sweep_missing_dir_is_a_noop() {
    let dir = TempDir::new().expect("tempdir");
    let missing = dir.path().join("does-not-exist");
    assert_eq!(sweep_stale_tmp(&missing).expect("sweep missing dir"), 0);
}

#[test]
fn corrupt_and_bad_magic_path_load_is_an_error_without_fallback() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("quicksave");
    let good = save_to_bytes(&sample_data(1)).expect("encode");

    // Bad magic -> BadMagic.
    let mut bad_magic = good.clone();
    bad_magic[0..4].copy_from_slice(b"XXXX");
    fs::write(&path, &bad_magic).expect("write bad magic");
    assert!(matches!(load(&path), Err(SaveError::BadMagic)));

    // Truncated header -> Corrupt.
    fs::write(&path, &good[..4]).expect("write truncated");
    assert!(matches!(load(&path), Err(SaveError::Corrupt(_))));

    // Corrupted zstd frame -> Corrupt.
    let mut corrupt = good.clone();
    corrupt[12] ^= 0xFF;
    fs::write(&path, &corrupt).expect("write corrupt frame");
    assert!(matches!(load(&path), Err(SaveError::Corrupt(_))));

    // Missing file -> ReadFailed, distinct from corrupt data.
    assert!(matches!(
        load(&dir.path().join("nope")),
        Err(SaveError::ReadFailed { .. })
    ));

    // No silent fallback: a corrupt live save next to a valid .bak still errors,
    // because the caller must explicitly point at the .bak to recover.
    let mut bad = good.clone();
    bad[12] ^= 0x80;
    fs::write(&path, &bad).expect("write corrupt live save");
    fs::write(path.with_extension("bak"), &good).expect("write valid backup");
    assert!(matches!(load(&path), Err(SaveError::Corrupt(_))));
}

#[test]
fn write_atomic_rejects_bad_version_before_touching_disk() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("quicksave");
    let mut data = sample_data(1);
    data.version = 99;

    assert!(matches!(
        write_atomic(&path, &data),
        Err(SaveError::UnsupportedVersion { .. })
    ));
    // Encoding failure happens before any file operation.
    assert!(!path.exists());
    assert_eq!(tmp_files(dir.path()), 0);
}
