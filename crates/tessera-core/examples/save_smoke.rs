//! Headless file-I/O smoke for the atomic save layer: writes a save, rotates a
//! backup, reloads both, and prints the final save-directory state.
//!
//! Run with:
//! ```sh
//! cargo run -p tessera-core --example save_smoke
//! ```

use std::fs;

use tessera_core::rng::SimRng;
use tessera_core::save::{SAVE_FORMAT_VERSION, SaveData, load, sweep_stale_tmp, write_atomic};

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
        player_turns: 0,
        facing: (0, 0),
    }
}

fn main() {
    let dir = tempfile::tempdir().expect("create temp save dir");
    let save = dir.path().join("quicksave");

    // Write the first save, then a second — the second rotates the first to
    // `.bak` before the atomic rename lands.
    write_atomic(&save, &sample_data(1)).expect("write first save");
    write_atomic(&save, &sample_data(2)).expect("write second save");

    let live = load(&save).expect("reload live save");
    let backup = load(&save.with_extension("bak")).expect("reload .bak backup");
    println!("live save  clock = {}", live.clock);
    println!("backup     clock = {}", backup.clock);

    // Startup sweep: plant a stale temp (as a crashed write would leave) and
    // remove it, exactly what a game startup does before any load.
    fs::write(dir.path().join("quicksave.tmp"), b"stale").expect("plant stale tmp");
    let removed = sweep_stale_tmp(dir.path()).expect("sweep stale tmp");
    println!("stale tmp swept   = {removed}");

    println!("\nsave directory state:");
    let mut names: Vec<String> = fs::read_dir(dir.path())
        .expect("read save dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        println!("  {name}");
    }
}
