# Determinism rules

The simulation must be reproducible from a seed plus inputs (tests, replays, save
validation, bug reports).

1. All simulation randomness uses `tessera_core::rng::SimRng`. Never `rand::rng()`,
   system time, or addresses.
2. Give each system its own stream via `SimRng::fork(label)`, so adding a roll in one
   system doesn't shift another.
3. Never iterate a `HashMap`/`HashSet` in simulation code. Use `BTreeMap`/`BTreeSet`/`Vec`.
4. The simulation is single-threaded. Threads are for loading and worldgen-only work
   with their own forked seeds.
5. Don't use floating point for gameplay outcomes where platforms could differ; prefer
   integers/fixed point.
6. The simulation crate must not depend on windowing or GPU crates. Rendering only reads state.

`SimRng` is ChaCha8; the `known_values_are_stable` test is the place to pin outputs once
saves exist.
