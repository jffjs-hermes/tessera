//! Seeded cellular-automata cave generation (native, integer-only, deterministic).
//!
//! The pipeline is the standard 4/5-rule CA cave generator, kept integer-only so
//! the same seed produces the same cells on every platform and thread. All
//! randomness comes from [`crate::rng::SimRng`], forked into a worldgen-only
//! stream so an unrelated roll in gameplay never shifts cave shapes.
//!
//! Steps (the standard CA cave-generation method):
//!
//! 1. **Seed fill.** Every interior cell starts as wall with probability
//!    `fill / 100`, drawn from the forked stream. The border ring is always
//!    wall.
//! 2. **Refinement.** `passes` rounds of the 4/5 rule over the 8-neighborhood:
//!    a cell with at least 5 wall neighbours stays (or becomes) wall; otherwise
//!    it becomes floor.
//! 3. **Region cleanup.** A flood fill labels every connected floor region
//!    (row-major scan order picks each region's root deterministically). Every
//!    region except the largest is filled back to wall, so the playable area is
//!    a single connected cave.
//!
//! The start tile is resolved by the caller via the same row-major scan the
//! zone loader already uses for the first walkable cell — the first floor cell
//! of the largest region, deterministic and RNG-free.

use crate::rng::SimRng;

/// The terrain id emitted for a floor cell.
pub const FLOOR_TERRAIN: &str = "terrain.floor";
/// The terrain id emitted for a wall cell.
pub const WALL_TERRAIN: &str = "terrain.wall";

/// The forked-stream label for world generation. Always the same constant so
/// caves are a pure function of the zone seed (no game state leaks in).
const WORLDGEN_FORK: u64 = 0x574f_524c_4447_454e; // "WORLDGEN" as bytes

/// Tunable cave parameters parsed from a zone's `[gen]` table (`type = "caves"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaveParams {
    pub width: usize,
    pub height: usize,
    /// Zone-local worldgen seed; the generator forks its own stream from it.
    pub seed: u64,
    /// Initial wall fill percentage (0..=100).
    pub fill: u32,
    /// Number of 4/5-rule refinement passes (1+).
    pub passes: u32,
}

/// The finished cave: row-major terrain-id cells plus the fraction that is
/// floor after cleanup, as a 0..100 integer percentage (the "minimum floor
/// fraction" acceptance measure, no floats).
#[derive(Clone, Debug)]
pub struct CaveResult {
    pub cells: Vec<String>,
    pub width: usize,
    pub height: usize,
    pub floor_percent: u32,
}

/// Validate load-time parameters value by value, so a malformed zone file
/// fails closed before any generation work. Returns a message on the first
/// offending value.
pub fn validate(params: &CaveParams) -> Result<(), String> {
    if params.width < 3 || params.height < 3 {
        return Err(format!(
            "cave zone must be at least 3x3 (got {}x{})",
            params.width, params.height
        ));
    }
    if params.fill > 100 {
        return Err(format!("fill must be 0..=100, got {}", params.fill));
    }
    if params.passes == 0 {
        return Err("passes must be at least 1".to_string());
    }
    Ok(())
}

/// Generate the cave for `params`. Assumes [`validate`] already passed.
pub fn generate(params: &CaveParams) -> CaveResult {
    let (w, h) = (params.width, params.height);

    // 1. Seed fill with a forked, worldgen-only stream. wall = true.
    let mut base = SimRng::from_seed(params.seed);
    let mut rng = base.fork(WORLDGEN_FORK);
    let mut grid: Vec<bool> = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let border = x == 0 || y == 0 || x + 1 == w || y + 1 == h;
            let wall = border || rng.below(100) < params.fill;
            grid.push(wall);
        }
    }

    // 2. Refinement passes (4/5 rule), interior cells only — the border ring
    //    stays wall by construction, so it can never be flipped by a pass.
    for _ in 0..params.passes {
        let mut next = grid.clone();
        for y in 0..h {
            for x in 0..w {
                let border = x == 0 || y == 0 || x + 1 == w || y + 1 == h;
                if border {
                    continue; // border is always wall.
                }
                // Interior cells have all 8 in-grid neighbours.
                next[y * w + x] = count_wall_neighbours(&grid, w, h, x, y) >= 5;
            }
        }
        grid = next;
    }

    // 3. Region cleanup: keep only the largest connected floor region.
    keep_largest_floor_region(&mut grid, w, h);

    let floor = grid.iter().filter(|cell| !**cell).count();
    let total = w * h;
    let floor_percent = floor
        .checked_mul(100)
        .and_then(|f| f.checked_div(total))
        .unwrap_or(0) as u32;

    let cells = grid
        .iter()
        .map(|wall| {
            if *wall {
                WALL_TERRAIN.to_string()
            } else {
                FLOOR_TERRAIN.to_string()
            }
        })
        .collect();

    CaveResult {
        cells,
        width: w,
        height: h,
        floor_percent,
    }
}

/// Count wall neighbours of `(x, y)` in the 8-neighborhood.
fn count_wall_neighbours(grid: &[bool], w: usize, h: usize, x: usize, y: usize) -> u32 {
    let mut n = 0u32;
    for dy in -1i32..=1 {
        for dx in -1i32..=1 {
            if dx == 0 && dy == 0 {
                continue;
            }
            let nx = x as i32 + dx;
            let ny = y as i32 + dy;
            if nx >= 0
                && ny >= 0
                && (nx as usize) < w
                && (ny as usize) < h
                && grid[ny as usize * w + nx as usize]
            {
                n += 1;
            }
        }
    }
    n
}

/// Flood-fill floor regions (row-major scan, iterative BFS) and keep only the
/// largest connected one; every smaller region becomes wall.
fn keep_largest_floor_region(grid: &mut [bool], w: usize, h: usize) {
    let mut label = vec![0u32; grid.len()];
    let mut region_area: Vec<usize> = vec![0]; // index 0 unused.
    let mut next_label = 1u32;

    // Row-major scan: each unvisited floor cell starts a new region.
    for idx in 0..grid.len() {
        if !grid[idx] && label[idx] == 0 {
            let mut queue: Vec<usize> = vec![idx];
            label[idx] = next_label;
            let mut area = 0usize;
            while let Some(cur) = queue.pop() {
                area += 1;
                let cx = (cur % w) as i32;
                let cy = (cur / w) as i32;
                for dy in -1i32..=1 {
                    for dx in -1i32..=1 {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        let nx = cx + dx;
                        let ny = cy + dy;
                        if nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h {
                            let ni = ny as usize * w + nx as usize;
                            if !grid[ni] && label[ni] == 0 {
                                label[ni] = next_label;
                                queue.push(ni);
                            }
                        }
                    }
                }
            }
            region_area.push(area);
            next_label += 1;
        }
    }

    // Largest region: scan area[1..] for the max (row-major first-wins on ties,
    // deterministic).
    let largest_label = (1..region_area.len())
        .max_by_key(|&i| region_area[i])
        .unwrap_or(0);
    for (idx, wall) in grid.iter_mut().enumerate() {
        let l = label[idx];
        if l != 0 && l != largest_label as u32 {
            *wall = true;
        }
    }
}

/// FNV-1a 64 hash over a byte slice. Exposed as the cave-determinism pin used
/// by the acceptance test; ~6 lines, no dependency.
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Render the generated cells to a text grid (floor=`.` and wall=`#`, one row
/// per line with a trailing newline — the same shape as the headless `text_map`
/// without the player overlay). Deterministic; the pin hash is over this.
pub fn text_render(result: &CaveResult) -> String {
    let mut s = String::with_capacity(result.cells.len() + result.height);
    for y in 0..result.height {
        for x in 0..result.width {
            let wall = result.cells[y * result.width + x] == WALL_TERRAIN;
            s.push(if wall { '#' } else { '.' });
        }
        s.push('\n');
    }
    s
}

/// FNV-1a 64 over the text rendering of the cave generated from `params`. Same
/// seed & parameters => same checksum, byte for byte. This is the acceptance
/// pin kept in the worldgen tests.
pub fn cave_checksum(params: &CaveParams) -> u64 {
    fnv1a_64(text_render(&generate(params)).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shipped start-zone parameters (data/base/zones/start.toml) and the seed
    /// the acceptance criteria require.
    fn shipped_params() -> CaveParams {
        CaveParams {
            width: 80,
            height: 50,
            seed: 20_260_318,
            fill: 45,
            passes: 2,
        }
    }

    #[test]
    fn pinned_checksum_for_shipped_seed_and_params() {
        // Pinned FNV-1a cave checksum for seed 20260318 + the shipped start
        // parameters. Any single-cell shift in the generator changes this
        // constant and fails the test; only a deliberate, reviewed re-pin
        // passes it again.
        assert_eq!(
            cave_checksum(&shipped_params()),
            9_504_760_926_063_972_046,
            "pinned cave checksum for seed 20260318 + shipped start.toml"
        );
    }

    #[test]
    fn same_seed_identical_cells() {
        let a = generate(&shipped_params());
        let b = generate(&shipped_params());
        assert_eq!(a.cells, b.cells);
    }

    #[test]
    fn differing_seeds_differ() {
        let params = shipped_params();
        let hashes: Vec<u64> = (1..=8)
            .map(|i| cave_checksum(&CaveParams { seed: i, ..params }))
            .collect();
        let mut unique = hashes.clone();
        unique.sort_unstable();
        unique.dedup();
        assert!(
            unique.len() >= 7,
            "expected most seeds to differ, got {hashes:?}"
        );
    }

    #[test]
    fn all_borders_are_walls() {
        let r = generate(&shipped_params());
        for y in 0..r.height {
            for x in 0..r.width {
                let border = x == 0 || y == 0 || x + 1 == r.width || y + 1 == r.height;
                if border {
                    assert_eq!(
                        r.cells[y * r.width + x],
                        WALL_TERRAIN.to_string(),
                        "border cell ({x},{y}) must be wall"
                    );
                }
            }
        }
    }

    #[test]
    fn exactly_one_floor_region() {
        let r = generate(&shipped_params());
        let mut label = vec![0u32; r.cells.len()];
        let (w, h) = (r.width, r.height);
        let mut next = 1u32;
        for idx in 0..r.cells.len() {
            if r.cells[idx] == FLOOR_TERRAIN && label[idx] == 0 {
                let mut queue = vec![idx];
                label[idx] = next;
                while let Some(cur) = queue.pop() {
                    let cx = (cur % w) as i32;
                    let cy = (cur / w) as i32;
                    for dy in -1i32..=1 {
                        for dx in -1i32..=1 {
                            if dx == 0 && dy == 0 {
                                continue;
                            }
                            let nx = cx + dx;
                            let ny = cy + dy;
                            if nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h {
                                let ni = ny as usize * w + nx as usize;
                                if r.cells[ni] == FLOOR_TERRAIN && label[ni] == 0 {
                                    label[ni] = next;
                                    queue.push(ni);
                                }
                            }
                        }
                    }
                }
                next += 1;
            }
        }
        // Region ids are 1..next; there must be exactly one region.
        assert_eq!(
            next - 1,
            1,
            "expected exactly one floor region, found {}",
            next - 1
        );
    }

    #[test]
    fn first_walkable_cell_is_interior_floor() {
        let r = generate(&shipped_params());
        let first = r
            .cells
            .iter()
            .position(|c| c == FLOOR_TERRAIN)
            .expect("cave has floor");
        let y = first / r.width;
        let x = first % r.width;
        assert_eq!(r.cells[first], FLOOR_TERRAIN);
        // Interior, not a border.
        assert!(x > 0 && y > 0 && x + 1 < r.width && y + 1 < r.height);
    }

    #[test]
    fn minimum_floor_fraction() {
        let r = generate(&shipped_params());
        assert!(
            r.floor_percent >= 25,
            "floor fraction {}% below the 25% floor",
            r.floor_percent
        );
    }

    #[test]
    fn invalid_fill_and_passes_fail_closed() {
        let params = shipped_params();
        assert!(
            validate(&CaveParams {
                fill: 101,
                ..params
            })
            .is_err()
        );
        assert!(
            validate(&CaveParams {
                passes: 0,
                ..params
            })
            .is_err()
        );
        assert!(validate(&CaveParams { width: 2, ..params }).is_err());
        assert!(validate(&params).is_ok());
    }

    #[test]
    fn fnv1a_known_vector() {
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a_64(b"foobar"), 0x8594_4171_f739_67e8);
    }
}
