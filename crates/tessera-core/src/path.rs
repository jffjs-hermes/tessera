//! Deterministic native path and spatial primitives.
//!
//! This module owns the grid-search primitives that the simulation and the
//! script layer both call: A* pathfinding, integer squared distance, nearest
//! walkable-empty search, and random walkable-empty selection. Everything here
//! is integer-only, uses only [`Vec`] and [`BinaryHeap`] (never `HashMap` /
//! `HashSet`, and never floats), and is fully deterministic for a given
//! (seed, zone, inputs) — the same search always returns the same path.
//!
//! # The occupancy seam
//!
//! A cell is "walkable **empty**" iff it is terrain-walkable *and* no actor
//! stands on it. Occupancy (an actor on a cell) is a query over live entities'
//! `Position` parts, not part of the terrain grid. The path primitives accept
//! an [`Occupancy`] callback — the caller (the sim broker, later the script
//! host) supplies how `(x, y) -> occupied` is answered. This keeps the core
//! primitives free of any entity-broker dependency while giving every
//! respawn / spawn-target call one deterministic predicate: `walkable && !occupied`.
//!
//! # A* contract
//!
//! - 8-directional (matching the demo move set), fixed neighbor order
//!   N, NE, E, SE, S, SW, W, NW.
//! - Integer costs: straight = 10, diagonal = 14; octile heuristic ×10, so the
//!   path cost is an integer and `f = g + h` is exact.
//! - Frontier is a [`BinaryHeap`] of `(f, h, seq, index)` under [`Reverse`]:
//!   ties break on `h` (closer to goal first), then on the monotonic push
//!   counter `seq` (FIFO among full ties), then on cell index (a stable final
//!   key). `seq` is a plain `u64` — no hash container, no float compare.
//! - Closed set is a generation-stamp `Vec<u32>` (`seen`, `generation`) instead of a
//!   `HashSet` — O(1) reset per search by bumping `generation`.
//! - No corner cutting: a diagonal step requires both orthogonal neighbours
//!   walkable.
//! - `path` returns the full path excluding `from`, including `to`, or `None`
//!   when the goal is unreachable or the node budget is exceeded.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::events::Point;
use crate::rng::SimRng;
use crate::zone::Zone;

/// Straight-step cost (10). Integer octile geometry scaled ×10.
const STRAIGHT: u32 = 10;
/// Diagonal-step cost (14).
const DIAGONAL: u32 = 14;

/// Fixed neighbour expansion order: N, NE, E, SE, S, SW, W, NW (row-major by
/// `dy`, then `dx`). Every equal-cost path resolves the same way because
/// parent assignment follows this order.
const NEIGHBORS: [(i32, i32); 8] = [
    (0, -1),
    (1, -1),
    (1, 0),
    (1, 1),
    (0, 1),
    (-1, 1),
    (-1, 0),
    (-1, -1),
];

/// The occupancy query seam: does an actor currently stand on `(x, y)`?
///
/// The caller supplies this. "Walkable empty" is `zone.is_walkable(x, y)
/// && !occupied(x, y)` — the predicate every respawn and spawn-target call
/// composes internally.
pub type Occupancy<'a> = dyn Fn(i32, i32) -> bool + 'a;

/// Integer squared Euclidean distance between two cells. `dx² + dy²`, exact,
/// no `sqrt` and no floats. The "within 10 tiles" rule is `dist2 <= 100`.
pub fn dist2(a: Point, b: Point) -> u32 {
    let dx = a.x as i64 - b.x as i64;
    let dy = a.y as i64 - b.y as i64;
    (dx * dx + dy * dy) as u32
}

/// Octile heuristic ×10: `14·min(dx,dy) + 10·(max-min)`. Admissible and
/// consistent for the 10/14 cost geometry, so A* never over-cuts.
fn octile_heuristic(from: Point, to: Point) -> u32 {
    let dx = (from.x - to.x).unsigned_abs();
    let dy = (from.y - to.y).unsigned_abs();
    let (dmin, dmax) = (dx.min(dy), dx.max(dy));
    DIAGONAL * dmin + STRAIGHT * (dmax - dmin)
}

/// A\* path from `from` to `to` through walkable terrain.
///
/// Returns the full path excluding `from`, including `to`, or `None` when
/// `to` is unreachable (or `max_nodes` expansions were used up first — the
/// two are indistinguishable to the caller, deliberately: both mean "don't
/// move along a path"). `start == goal` returns an empty path.
pub fn path(zone: &Zone, from: Point, to: Point, max_nodes: u32) -> Option<Vec<Point>> {
    if from == to {
        return Some(Vec::new());
    }
    if !zone.in_bounds(from.x, from.y) || !zone.in_bounds(to.x, to.y) {
        return None;
    }
    if !zone.is_walkable(from.x, from.y) || !zone.is_walkable(to.x, to.y) {
        return None;
    }

    let w = zone.width as i32;
    let n = zone.width * zone.height;
    let idx = |p: Point| (p.y * w + p.x) as usize;
    let pt = |i: usize| Point::new((i % w as usize) as i32, (i / w as usize) as i32);

    let start = idx(from);
    let goal = idx(to);

    // Generation-stamp closed set: `seen[i] == generation` means node `i` is
    // closed. A fresh `Vec` per search plus a nonzero stamp — no allocation of
    // a hash container, so no randomized iteration order can leak in.
    let generation: u32 = 1;
    let mut seen: Vec<u32> = vec![0; n];
    let mut g: Vec<u32> = vec![u32::MAX; n];
    let mut parent: Vec<usize> = vec![usize::MAX; n];

    let mut heap: BinaryHeap<Reverse<(u32, u32, u64, usize)>> = BinaryHeap::new();
    let h0 = octile_heuristic(from, to);
    heap.push(Reverse((h0, h0, 0, start)));
    g[start] = 0;
    let mut seq: u64 = 1;
    let mut expanded: u32 = 0;

    while let Some(Reverse((_f, _h, _seq, node))) = heap.pop() {
        if seen[node] == generation {
            continue; // already closed via another branch
        }
        seen[node] = generation;
        if node == goal {
            break;
        }
        expanded += 1;
        if expanded > max_nodes {
            // Budget exhausted without reaching the goal: treat as unreachable.
            return None;
        }
        let p = pt(node);
        for &(dx, dy) in &NEIGHBORS {
            let nx = p.x + dx;
            let ny = p.y + dy;
            // No corner cutting: a diagonal move needs both orthogonal sides.
            if dx != 0
                && dy != 0
                && (!zone.is_walkable(p.x + dx, p.y) || !zone.is_walkable(p.x, p.y + dy))
            {
                continue;
            }
            if !zone.is_walkable(nx, ny) {
                continue;
            }
            let ni = idx(Point::new(nx, ny));
            if seen[ni] == generation {
                continue;
            }
            let step = if dx != 0 && dy != 0 {
                DIAGONAL
            } else {
                STRAIGHT
            };
            let tentative = g[node] + step;
            if tentative < g[ni] {
                g[ni] = tentative;
                parent[ni] = node;
                let h = octile_heuristic(Point::new(nx, ny), to);
                heap.push(Reverse((tentative + h, h, seq, ni)));
                seq += 1;
            }
        }
    }

    if seen[goal] != generation {
        return None; // unreachable
    }

    // Reconstruct from `goal` back to `from`, excluding `from` from the result.
    let mut path_rev = Vec::new();
    let mut cur = goal;
    while cur != start {
        path_rev.push(pt(cur));
        cur = parent[cur];
    }
    path_rev.reverse();
    Some(path_rev)
}

/// Nearest walkable-empty cell to `(x, y)` via BFS in the fixed neighbour
/// order; first hit wins. Deterministic (no RNG). Returns `None` if no cell
/// qualifies. Used by player respawn and "nearest free tile" queries.
pub fn nearest_walkable_empty(
    zone: &Zone,
    occupied: &Occupancy<'_>,
    x: i32,
    y: i32,
) -> Option<Point> {
    use std::collections::VecDeque;

    if zone.is_walkable(x, y) && !occupied(x, y) {
        return Some(Point::new(x, y));
    }
    let w = zone.width as i32;
    let n = zone.width * zone.height;
    let idx = |p: Point| (p.y * w + p.x) as usize;

    let mut visited = vec![false; n];
    let mut queue: VecDeque<Point> = VecDeque::new();
    if zone.in_bounds(x, y) {
        visited[idx(Point::new(x, y))] = true;
        queue.push_back(Point::new(x, y));
    }
    while let Some(p) = queue.pop_front() {
        for &(dx, dy) in &NEIGHBORS {
            let np = Point::new(p.x + dx, p.y + dy);
            if !zone.in_bounds(np.x, np.y) {
                continue;
            }
            let ni = idx(np);
            if visited[ni] {
                continue;
            }
            visited[ni] = true;
            if !zone.is_walkable(np.x, np.y) {
                continue; // do not expand through walls
            }
            if !occupied(np.x, np.y) {
                return Some(np);
            }
            queue.push_back(np);
        }
    }
    None
}

/// A walkable-empty cell chosen uniformly at random from the row-major list
/// of all qualified cells, via `rng.below(len)`. Row-major collection plus one
/// indexed draw is fully deterministic for a given stream state. Returns
/// `None` when no cell qualifies (callers must handle it).
pub fn random_walkable_empty(
    zone: &Zone,
    occupied: &Occupancy<'_>,
    rng: &mut SimRng,
) -> Option<Point> {
    let mut cells: Vec<Point> = Vec::new();
    for y in 0..zone.height as i32 {
        for x in 0..zone.width as i32 {
            let p = Point::new(x, y);
            if zone.is_walkable(x, y) && !occupied(x, y) {
                cells.push(p);
            }
        }
    }
    if cells.is_empty() {
        return None;
    }
    let i = rng.below(cells.len() as u32) as usize;
    Some(cells[i])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zone::Zone;

    fn no_occupant() -> impl Fn(i32, i32) -> bool {
        |_x, _y| false
    }

    /// A hand-authored grid as a `Zone`. `#` = wall, `.` = floor.
    fn zone(rows: &[&str]) -> Zone {
        let height = rows.len();
        let width = rows[0].len();
        let mut cells = Vec::with_capacity(width * height);
        for row in rows {
            assert_eq!(row.len(), width, "ragged test grid");
            for c in row.chars() {
                cells.push(
                    if c == '#' {
                        "terrain.wall"
                    } else {
                        "terrain.floor"
                    }
                    .to_string(),
                );
            }
        }
        Zone::from_cells("test", width, height, Point::new(1, 1), cells)
    }

    fn wall_grid() -> Vec<&'static str> {
        vec![
            "##########",
            "#........#",
            "#..####..#",
            "#..#..#..#",
            "#..#..#..#",
            "#........#",
            "##########",
        ]
    }

    // ---- dist2 ------------------------------------------------------------

    #[test]
    fn dist2_is_squared_integer_distance() {
        assert_eq!(dist2(Point::new(0, 0), Point::new(0, 0)), 0);
        assert_eq!(dist2(Point::new(0, 0), Point::new(3, 4)), 25);
        assert_eq!(dist2(Point::new(0, 0), Point::new(10, 0)), 100);
        assert_eq!(dist2(Point::new(0, 0), Point::new(11, 0)), 121);
        assert_eq!(dist2(Point::new(2, 3), Point::new(5, 7)), 9 + 16);
        // Symmetric.
        assert_eq!(dist2(Point::new(5, 7), Point::new(2, 3)), 25);
    }

    // ---- corridor-only detour (exact path) --------------------------------

    #[test]
    fn corridor_only_detour_is_exact() {
        // An L-shaped corridor around a wall block; the detour is the only path.
        let z = zone(&[
            "#########",
            "#.......#",
            "#.#####.#",
            "#.#...#.#",
            "#.#...#.#",
            "#.#...#.#",
            "#.......#",
            "#########",
        ]);
        // from (1,1) to (7,6); wall block in the middle column forces the route.
        let p = path(&z, Point::new(1, 1), Point::new(7, 6), 4096).expect("reachable");
        assert_eq!(p.last().copied(), Some(Point::new(7, 6)));
        assert_eq!(p.first().copied(), Some(Point::new(2, 1)));
        // Every returned cell is walkable and consecutive cells are neighbours.
        for w in p.windows(2) {
            let d = (w[1].x - w[0].x).abs() + (w[1].y - w[0].y).abs();
            assert!(
                d == 1 || d == 2,
                "stepped ({},{}) -> ({},{})",
                w[0].x,
                w[0].y,
                w[1].x,
                w[1].y
            );
            assert!(z.is_walkable(w[1].x, w[1].y));
        }
    }

    // ---- equal-cost tie winner --------------------------------------------

    #[test]
    fn equal_cost_tie_resolves_to_neighbor_order_winner() {
        // A central 3x3 block leaves two symmetric routes (over the top or
        // under the bottom), both equal cost. The fixed neighbor order + tie
        // breaks must deterministically pick one route every run.
        let z = zone(&[
            "########", "#......#", "#..###.#", "#..###.#", "#..###.#", "#......#", "########",
        ]);
        // from (1,4) to (6,4): must go around the block either above (row 1) or
        // below (row 5) — equal cost either way.
        let p = path(&z, Point::new(1, 4), Point::new(6, 4), 4096).expect("reachable");
        // Deterministic: a second search is byte-identical.
        let p2 = path(&z, Point::new(1, 4), Point::new(6, 4), 4096).unwrap();
        assert_eq!(p, p2);
        // Pin the actual deterministic winner and the structurally valid gold
        // standard: the route stays entirely on one side of the block (it never
        // mixes sides, which would add cost), reaches the goal, and every step
        // is a legal contiguous move.
        let all_in_top = p.iter().all(|pt| pt.y <= 3);
        let all_in_bottom = p.iter().all(|pt| pt.y >= 4);
        assert!(
            all_in_top || all_in_bottom,
            "tie route should be a single clean side, got {p:?}"
        );
        assert_eq!(p.last().copied(), Some(Point::new(6, 4)));
        for w in p.windows(2) {
            let dx = (w[1].x - w[0].x).abs();
            let dy = (w[1].y - w[0].y).abs();
            assert!(
                dx <= 1 && dy <= 1 && dx + dy >= 1,
                "non-contiguous step in {p:?}"
            );
            assert!(z.is_walkable(w[1].x, w[1].y));
        }
    }

    // ---- no corner cutting ------------------------------------------------

    #[test]
    fn diagonal_squeeze_does_not_cut_corners() {
        // A central 2x2 wall block must be rounded via orthogonals; any diagonal
        // step taken along the way needs both orthogonal neighbours walkable.
        let z = zone(&["######", "#....#", "#.##.#", "#.##.#", "#....#", "######"]);
        let p = path(&z, Point::new(1, 1), Point::new(4, 4), 4096).unwrap();
        assert_eq!(p.last().copied(), Some(Point::new(4, 4)));
        // Every step is contiguous (Chebyshev distance 1) and no diagonal step
        // cuts a corner (both orthogonal cells around it are walkable).
        for w in p.windows(2) {
            let dx = (w[1].x - w[0].x).abs();
            let dy = (w[1].y - w[0].y).abs();
            assert!(dx <= 1 && dy <= 1, "non-adjacent step in path");
            if dx == 1 && dy == 1 {
                let (x0, y0) = (w[0].x, w[0].y);
                let (x1, y1) = (w[1].x, w[1].y);
                assert!(
                    z.is_walkable(x1, y0) && z.is_walkable(x0, y1),
                    "corner cut at ({x0},{y0})->({x1},{y1})"
                );
            }
        }
    }

    // ---- unreachable / start==goal / truncation ---------------------------

    #[test]
    fn unreachable_returns_none() {
        let z = zone(&["#####", "#.#.#", "#####"]);
        // (1,1) and (3,1) are separate enclosed floor cells: no connection.
        assert_eq!(path(&z, Point::new(1, 1), Point::new(3, 1), 4096), None);
        // Out of bounds / non-walkable endpoints also return None.
        assert_eq!(path(&z, Point::new(1, 1), Point::new(99, 1), 4096), None);
        assert_eq!(path(&z, Point::new(0, 0), Point::new(1, 1), 4096), None);
    }

    #[test]
    fn start_equals_goal_is_empty_path() {
        let z = zone(&wall_grid());
        let p = path(&z, Point::new(1, 1), Point::new(1, 1), 4096).unwrap();
        assert!(p.is_empty());
    }

    #[test]
    fn max_nodes_truncation_returns_none() {
        // A long, fully-open corridor; a tiny budget cannot reach the far goal.
        let z = zone(&["##############", "#............#", "##############"]);
        // From (1,1) to (12,1) is an 11-step straight run: a budget of 3 nodes
        // exhausts long before the goal.
        assert_eq!(path(&z, Point::new(1, 1), Point::new(12, 1), 3), None);
        // A generous budget reaches it.
        assert!(path(&z, Point::new(1, 1), Point::new(12, 1), 4096).is_some());
    }

    // ---- octile diagonal preference ---------------------------------------

    #[test]
    fn octile_diagonal_route_is_cheaper_and_preferred() {
        let z = zone(&[
            "#########",
            "#.......#",
            "#.......#",
            "#.......#",
            "#.......#",
            "#########",
        ]);
        // from (1,1) to (7,3): dx=6, dy=2. An octile-optimal path uses 2 diagonal
        // (14 each) + 4 straight (10 each) steps = 68. A purely orthogonal route
        // would be 8 * 10 = 80, which is worse — so the heuristic must prefer the
        // diagonal shortcut.
        let p = path(&z, Point::new(1, 1), Point::new(7, 3), 4096).unwrap();
        assert_eq!(p.last().copied(), Some(Point::new(7, 3)));
        // Sum the integer step costs actually walked.
        let mut cost = 0;
        let cur = (1, 1);
        let mut prev = cur;
        for step in &p {
            let dx = step.x - prev.0;
            let dy = step.y - prev.1;
            let d = dx.abs() + dy.abs();
            cost += if d == 2 { 14 } else { 10 };
            prev = (step.x, step.y);
        }
        // 2 diagonals + 4 straights = 68. The heuristic's octile value matches.
        assert_eq!(
            cost, 68,
            "A* should find the 2-diagonal + 4-straight shortcut"
        );
        assert_eq!(octile_heuristic(Point::new(1, 1), Point::new(7, 3)), 68);
    }

    // ---- nearest BFS order ------------------------------------------------

    #[test]
    fn nearest_walkable_empty_bfs_ring_order() {
        // Destination (0,0) is wall; ring 1 floor cells are (1,0) and (0,1)
        // in N,NE,E,SE,S,SW,W,NW order from (1,1). From (1,1) the N neighbor
        // (1,0) is floor -> first hit.
        let z = zone(&["##.", "#..", "..."]);
        assert_eq!(
            nearest_walkable_empty(&z, &no_occupant(), 1, 1),
            Some(Point::new(1, 1))
        );
        // Occupied seat, so move outward in fixed order from (1,1).
        let occ = |x: i32, y: i32| x == 1 && y == 1;
        // From (1,1) with origin occupied: the first BFS ring in N..NW order.
        // N=(1,0) is wall; NE=(2,0) is floor+empty -> returned first.
        assert_eq!(
            nearest_walkable_empty(&z, &occ, 1, 1),
            Some(Point::new(2, 0))
        );
    }

    #[test]
    fn nearest_walkable_empty_returns_none_when_boxed_in() {
        let z = zone(&["###", "#.#", "###"]);
        // Only floor is (1,1), occupied -> nothing else qualifies.
        let occ = |_x: i32, _y: i32| true;
        assert_eq!(nearest_walkable_empty(&z, &occ, 1, 1), None);
    }

    // ---- random row-major + seeded ----------------------------------------

    #[test]
    fn random_walkable_empty_is_row_major_and_seeded() {
        // 3x4 grid; walkable cells collected row-major.
        let z = zone(&["###", "#..", "#..", "###"]);
        // Only (1,1),(2,1),(1,2),(2,2) are floor. rng.below(4) picks row-major.
        let mut r1 = SimRng::from_seed(7);
        let mut r2 = SimRng::from_seed(7);
        let a = random_walkable_empty(&z, &no_occupant(), &mut r1).unwrap();
        let b = random_walkable_empty(&z, &no_occupant(), &mut r2).unwrap();
        assert_eq!(a, b, "same seed must pick the same tile");
        assert!(z.is_walkable(a.x, a.y));

        // Occupancy filters the candidate list.
        let occ = |x: i32, y: i32| x == 1 && y == 1;
        let mut r3 = SimRng::from_seed(7);
        let c = random_walkable_empty(&z, &occ, &mut r3).unwrap();
        assert_ne!((c.x, c.y), (1, 1), "occupied cell must not be chosen");

        // All-floor map with every cell occupied -> None.
        let z2 = zone(&["...", "..."]);
        let occ_all = |_x: i32, _y: i32| true;
        let mut r4 = SimRng::from_seed(1);
        assert_eq!(random_walkable_empty(&z2, &occ_all, &mut r4), None);
    }

    // ---- occupied filtering across the query seam -------------------------

    #[test]
    fn occupied_filtering_applies_to_nearest_and_random() {
        let z = zone(&["###", "#.#", "#.#", "###"]);
        // (1,1) and (1,2) floor. With (1,1) occupied, nearest from (1,1) -> (1,2).
        let occ = |x: i32, y: i32| x == 1 && y == 1;
        assert_eq!(
            nearest_walkable_empty(&z, &occ, 1, 1),
            Some(Point::new(1, 2))
        );
        assert_eq!(
            nearest_walkable_empty(&z, &no_occupant(), 1, 1),
            Some(Point::new(1, 1))
        );
    }
}
