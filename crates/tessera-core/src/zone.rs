//! Data-driven world grid: zones, a world map of them, and the loader that
//! builds a [`Zone`] from a data file through the [`crate::vfs::Vfs`].
//!
//! This replaces the old hardcoded room map with an owned, row-major terrain
//! grid loaded from `data/base/zones/*.toml`. Terrain ids are owned [`String`]s
//! (a key into the tile registry), so maps can come from data instead of only
//! from hardcoded `&'static str` constants.
//!
//! Determinism (see `docs/determinism.md`): storage is a flat [`Vec`], the
//! world map is a [`std::collections::BTreeMap`], and the loader iterates
//! files in sorted ([`Vfs::list`]) order — never a `HashMap`/`HashSet`, so no
//! randomized iteration order can leak into load behavior.
//!
//! # Generators
//!
//! A zone file declares a generator in its `[gen]` table:
//!
//! - `type = "ascii"` — a hand-authored grid: a glyph→terrain mapping plus one
//!   row-string per map row. This is the fixture / hand-authored path; it is
//!   the default and the only generator in this module (cave worldgen is a
//!   later, separate module).
//!
//! The loader validates every zone fail-closed: unknown generator type,
//! invalid dimensions, unknown glyphs, unknown terrain ids, and an invalid
//! [`Zone::start`] are all load errors naming the file. This mirrors the
//! blueprint loader's fail-closed diagnostics style so a bad zone can never
//! silently break the live world.

use std::collections::BTreeMap;

use serde::Deserialize;
use thiserror::Error;

use crate::events::Point;
use crate::tiles::TileRegistry;
use crate::vfs::Vfs;

/// A single grid world. Row-major owned terrain storage.
///
/// `TerrainId` is an owned [`String`] key into the tile registry; walkability
/// is a pure function of terrain (a wall tile is the only non-walkable tile in
/// the base tile set).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Zone {
    pub id: String,
    pub width: usize,
    pub height: usize,
    /// The spawn / start tile. Defaults to the first walkable cell in row-major
    /// scan order (deterministic, no RNG); a file may override it, in which
    /// case the loader validates it is walkable (fail-closed).
    pub start: Point,
    /// Terrain id per cell, row-major (`cells[y * width + x]`).
    cells: Vec<String>,
}

impl Zone {
    /// Build a zone from owned row-major cells (loader and tests both funnel
    /// through this; callers must guarantee `cells.len() == width * height`).
    pub fn from_cells(
        id: impl Into<String>,
        width: usize,
        height: usize,
        start: Point,
        cells: Vec<String>,
    ) -> Self {
        assert_eq!(
            cells.len(),
            width * height,
            "cell count must match width*height"
        );
        Self {
            id: id.into(),
            width,
            height,
            start,
            cells,
        }
    }

    /// True if `(x, y)` is on the grid.
    pub fn in_bounds(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height
    }

    /// The terrain id at a cell (caller must have checked [`Self::in_bounds`]).
    pub fn tile_id(&self, x: usize, y: usize) -> &str {
        &self.cells[y * self.width + x]
    }

    /// Terrain-id-based walkability. Out of bounds is always a wall.
    ///
    /// This is the map-state half of the occupancy seam: a cell is a legal
    /// move target iff [`Self::is_walkable`] *and* no actor occupies it.
    /// Occupancy (an actor standing on a cell) is a query over live entities'
    /// `Position` parts, not part of the terrain grid — the seam the spawn /
    /// respawn logic composes (`walkable && !occupied`).
    pub fn is_walkable(&self, x: i32, y: i32) -> bool {
        self.in_bounds(x, y) && self.tile_id(x as usize, y as usize) != "terrain.wall"
    }

    /// The tile ids used by this zone that have no definition in `reg`,
    /// sorted and deduplicated. Callers warn on these (they draw as `?`).
    pub fn missing_tile_ids(&self, reg: &TileRegistry) -> Vec<String> {
        let mut v: Vec<String> = self
            .cells
            .iter()
            .filter(|id| !reg.contains(id))
            .cloned()
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }
}

impl std::fmt::Display for Zone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "zone '{}' {}x{} start=({},{})",
            self.id, self.width, self.height, self.start.x, self.start.y
        )
    }
}

/// A set of zones keyed by id, loaded from the world data pack.
///
/// Determinism: a [`std::collections::BTreeMap`] so iteration order is stable.
/// The demo simulates exactly one zone today; this container exists so portals
/// / multi-zone worlds stay a data question, not a refactor.
#[derive(Clone, Debug, Default)]
pub struct WorldMap {
    zones: BTreeMap<String, Zone>,
}

impl WorldMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// The zone with `id`, if present.
    pub fn get(&self, id: &str) -> Option<&Zone> {
        self.zones.get(id)
    }

    pub fn len(&self) -> usize {
        self.zones.len()
    }

    pub fn is_empty(&self) -> bool {
        self.zones.is_empty()
    }

    /// Iterate zones in sorted id order (deterministic).
    pub fn zones(&self) -> impl Iterator<Item = &Zone> {
        self.zones.values()
    }

    /// Resolve the zone to simulate: the zone named by `requested` (a `--zone`
    /// value), or the single zone in the pack when none was requested. Fails
    /// closed when a requested id is unknown, or when the pack has more than
    /// one zone and none was requested (no silent pick).
    pub fn resolve<'a>(&'a self, requested: Option<&str>) -> Result<&'a Zone, ZoneError> {
        match requested {
            Some(id) => self
                .zones
                .get(id)
                .ok_or_else(|| ZoneError::UnknownZone(id.to_string())),
            None => {
                let mut it = self.zones.values();
                match (it.next(), it.next()) {
                    (Some(zone), None) => Ok(zone),
                    (Some(_), Some(_)) => Err(ZoneError::MultipleZones),
                    (None, _) => Err(ZoneError::NoZones),
                }
            }
        }
    }
}

/// Failure modes of the zone loader. Every variant names the offending file
/// (and, where meaningful, the zone id or message), matching the fail-closed
/// diagnostics style of the blueprint loader.
#[derive(Debug, Error)]
pub enum ZoneError {
    #[error("reading {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
    #[error("{0}")]
    Invalid(String),
    #[error("no zone named '{0}' in the world pack")]
    UnknownZone(String),
    #[error("the world pack has multiple zones; select one with --zone")]
    MultipleZones,
    #[error("the world pack contains no zones")]
    NoZones,
}

/// Load every `*.toml` zone file under `dir` (default `"zones"`) through the
/// vfs into a [`WorldMap`]. Files are read in sorted order (vfs contract), and
/// a duplicate zone id is a load error (fail-closed, no silent pick).
pub fn load_zones(vfs: &dyn Vfs, tiles: &TileRegistry) -> Result<WorldMap, ZoneError> {
    load_zones_from(vfs, tiles, "zones")
}

pub fn load_zones_from(
    vfs: &dyn Vfs,
    tiles: &TileRegistry,
    dir: &str,
) -> Result<WorldMap, ZoneError> {
    let files = vfs.list(dir).map_err(|source| ZoneError::Io {
        path: dir.to_string(),
        source,
    })?;
    let mut world = WorldMap::new();
    for path in files.iter().filter(|p| p.ends_with(".toml")) {
        let text = vfs.read_to_string(path).map_err(|source| ZoneError::Io {
            path: path.clone(),
            source,
        })?;
        let zone = load_zone(path, &text, tiles)?;
        if world.zones.contains_key(&zone.id) {
            return Err(ZoneError::Invalid(format!(
                "{path}: duplicate zone id '{}'",
                zone.id
            )));
        }
        world.zones.insert(zone.id.clone(), zone);
    }
    Ok(world)
}

/// Flat, serde-friendly shape of a `[gen.start]` override.
#[derive(Debug, Deserialize)]
struct StartCfg {
    x: i32,
    y: i32,
}

/// Parse and validate a single zone file's text into a [`Zone`].
fn load_zone(path: &str, text: &str, tiles: &TileRegistry) -> Result<Zone, ZoneError> {
    let raw: ZoneFile = toml::from_str(text).map_err(|source| ZoneError::Parse {
        path: path.to_string(),
        source,
    })?;

    let generator = raw.settings.r#type.as_deref().unwrap_or("ascii");
    // Explicit zero dimensions are invalid regardless of generator (fail-closed
    // before any generation).
    if raw.settings.width == Some(0) || raw.settings.height == Some(0) {
        return Err(ZoneError::Invalid(format!(
            "{path}: zone '{}' has invalid dimensions (must be > 0)",
            raw.id
        )));
    }
    let (mut cells, width, height) = match generator {
        "ascii" => generate_ascii(path, &raw.id, &raw.settings, tiles)?,
        "caves" => generate_cave(path, &raw.id, &raw.settings, tiles)?,
        other => {
            return Err(ZoneError::Invalid(format!(
                "{path}: zone '{}' declares unknown generator {other:?}",
                raw.id
            )));
        }
    };
    // Apply the overlay last (in sorted key order), overriding generated cells
    // for both generators. Same validation as the ascii path.
    apply_overlay(
        path,
        &raw.id,
        &raw.settings,
        &mut cells,
        width,
        height,
        tiles,
    )?;
    if width == 0 || height == 0 {
        return Err(ZoneError::Invalid(format!(
            "{path}: zone '{}' has invalid dimensions {width}x{height} (must be > 0)",
            raw.id
        )));
    }

    // Start tile: an explicit override must be walkable (fail-closed); the
    // default is the first walkable cell in row-major scan order — the same
    // deterministic, RNG-free rule the worldgen start rule uses.
    let start = match &raw.settings.start {
        Some(s) => {
            let p = Point::new(s.x, s.y);
            if !is_walkable_idx(&cells, width, p.x, p.y) {
                return Err(ZoneError::Invalid(format!(
                    "{path}: zone '{}' start tile ({},{}) is not walkable",
                    raw.id, p.x, p.y
                )));
            }
            p
        }
        None => first_walkable(&cells, width).ok_or_else(|| {
            ZoneError::Invalid(format!(
                "{path}: zone '{}' has no walkable start tile (all walls)",
                raw.id
            ))
        })?,
    };

    Ok(Zone {
        id: raw.id,
        width,
        height,
        start,
        cells,
    })
}

/// The flat shape of a zone file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ZoneFile {
    id: String,
    #[serde(rename = "gen")]
    settings: GenTable,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenTable {
    /// Generator type; default `"ascii"`.
    #[serde(rename = "type")]
    r#type: Option<String>,
    width: Option<usize>,
    height: Option<usize>,
    /// Optional start tile override; defaults to the first walkable cell.
    start: Option<StartCfg>,
    /// Ascii-generator table (`type = "ascii"`): the glyph grid.
    #[serde(default)]
    ascii: Option<AsciiSection>,
    /// Ascii-generator glyph→terrain mapping (type = "ascii").
    #[serde(default)]
    glyphs: BTreeMap<String, String>,
    /// Optional overlay: force specific cells after generation. Keys are
    /// `"x,y"`, values are terrain ids, applied (and validated) in sorted key
    /// order so it is deterministic. Applies to every generator.
    #[serde(default)]
    overlay: BTreeMap<String, String>,
    // ---- cave generator (`type = "caves"`) ----
    /// Zone-local worldgen seed (caves).
    seed: Option<u64>,
    /// Initial wall-fill percentage 0..=100 (caves).
    fill: Option<u32>,
    /// Number of 4/5-rule refinement passes, 1+ (caves).
    passes: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AsciiSection {
    rows: Vec<String>,
}

/// The ascii generator: turn a glyph grid + glyph→terrain mapping into row-major
/// cells. Fail-closed on unknown glyphs and unknown terrain ids.
fn generate_ascii(
    path: &str,
    id: &str,
    raw_type: &GenTable,
    tiles: &TileRegistry,
) -> Result<(Vec<String>, usize, usize), ZoneError> {
    let invalid = |msg: String| ZoneError::Invalid(format!("{path}: zone '{id}': {msg}"));

    let rows = match &raw_type.ascii {
        Some(section) => &section.rows,
        None => {
            return Err(invalid(
                "ascii generator requires a `[gen.ascii]` section".into(),
            ));
        }
    };
    if rows.is_empty() {
        return Err(invalid("ascii generator requires at least one row".into()));
    }
    // Dimensions derive from the rendered grid unless a file overrides them
    // (then the grid must match; validates ascii "parameters" fail-closed).
    let width = raw_type.width.unwrap_or(rows[0].len());
    let height = raw_type.height.unwrap_or(rows.len());
    if rows.len() != height {
        return Err(invalid(format!(
            "ascii grid has {} rows but height is {height}",
            rows.len()
        )));
    }
    for (i, row) in rows.iter().enumerate() {
        if row.len() != width {
            return Err(invalid(format!(
                "ascii row {i} has width {} but expected {width}",
                row.len()
            )));
        }
    }

    // Validate the glyph map: exactly one character per glyph, and the id must
    // exist in the tile registry.
    let glyph_map: BTreeMap<char, String> = raw_type
        .glyphs
        .iter()
        .map(|(glyph, terrain_id)| {
            let mut cs = glyph.chars();
            match (cs.next(), cs.next()) {
                (Some(c), None) => {
                    if !tiles.contains(terrain_id) {
                        return Err(invalid(format!(
                            "glyph {glyph:?} maps to unknown terrain id {terrain_id:?}"
                        )));
                    }
                    Ok((c, terrain_id.clone()))
                }
                _ => Err(invalid(format!(
                    "glyph {glyph:?} must be exactly one character"
                ))),
            }
        })
        .collect::<Result<_, _>>()?;

    if glyph_map.is_empty() {
        return Err(invalid(
            "ascii generator requires at least one glyph mapping".into(),
        ));
    }

    // Resolve every cell and collect unknown glyphs together (sorted) for a
    // single complete diagnostic rather than failing on the first.
    let mut cells = Vec::with_capacity(width * height);
    let mut unknown: Vec<String> = Vec::new();
    for row in rows {
        for c in row.chars() {
            match glyph_map.get(&c) {
                Some(terrain) => cells.push(terrain.clone()),
                None => {
                    let g = c.to_string();
                    if !unknown.contains(&g) {
                        unknown.push(g);
                    }
                    // Keep dimensions aligned with an empty placeholder; the
                    // load fails below regardless.
                    cells.push(String::new());
                }
            }
        }
    }
    if !unknown.is_empty() {
        return Err(invalid(format!(
            "ascii grid uses glyphs with no mapping: {}",
            unknown.join(", ")
        )));
    }

    Ok((cells, width, height))
}

/// Apply the `[gen.overlay]` table to `cells` (in sorted key order). Keys are
/// `"x,y"`; every overlay must be in bounds and name a known terrain, and any
/// failure is a load error (fail-closed, same as the ascii path). Called from
/// [`load_zone`] for every generator so overlay semantics match across them.
fn apply_overlay(
    path: &str,
    id: &str,
    raw_type: &GenTable,
    cells: &mut [String],
    width: usize,
    height: usize,
    tiles: &TileRegistry,
) -> Result<(), ZoneError> {
    let invalid = |msg: String| ZoneError::Invalid(format!("{path}: zone '{id}': {msg}"));
    for (key, terrain_id) in &raw_type.overlay {
        if !tiles.contains(terrain_id) {
            return Err(invalid(format!(
                "overlay {key:?} names unknown terrain id {terrain_id:?}"
            )));
        }
        let (x, y) = parse_cell_key(key)
            .ok_or_else(|| invalid(format!("overlay key {key:?} is not \"x,y\"")))?;
        if x >= width || y >= height {
            return Err(invalid(format!(
                "overlay key {key:?} is out of bounds ({width}x{height})"
            )));
        }
        cells[y * width + x] = terrain_id.clone();
    }
    Ok(())
}

/// The cave generator: read `[gen]` cave parameters (`seed`, `fill`, `passes`,
/// `width`, `height`), validate them fail-closed, and run `worldgen::generate`.
/// Returns the row-major cells plus the resolved dimensions. Overlay (if any)
/// is applied afterwards by [`load_zone`] via [`apply_overlay`], exactly as for
/// the ascii generator.
fn generate_cave(
    path: &str,
    id: &str,
    raw_type: &GenTable,
    _tiles: &TileRegistry,
) -> Result<(Vec<String>, usize, usize), ZoneError> {
    let invalid = |msg: String| ZoneError::Invalid(format!("{path}: zone '{id}': {msg}"));

    let width = raw_type
        .width
        .ok_or_else(|| invalid("caves generator requires `width`".into()))?;
    let height = raw_type
        .height
        .ok_or_else(|| invalid("caves generator requires `height`".into()))?;
    let seed = raw_type
        .seed
        .ok_or_else(|| invalid("caves generator requires `seed`".into()))?;
    // Defaults match the shipped start zone's authored values.
    let fill = raw_type.fill.unwrap_or(45);
    let passes = raw_type.passes.unwrap_or(2);

    let params = crate::worldgen::CaveParams {
        width,
        height,
        seed,
        fill,
        passes,
    };
    crate::worldgen::validate(&params).map_err(invalid)?;
    let cave = crate::worldgen::generate(&params);
    Ok((cave.cells, cave.width, cave.height))
}

/// Is `(x, y)` walkable in flat row-major cells?
fn is_walkable_idx(cells: &[String], width: usize, x: i32, y: i32) -> bool {
    if x < 0 || y < 0 || (x as usize) >= width {
        return false;
    }
    let idx = y as usize * width + x as usize;
    idx < cells.len() && cells[idx] != "terrain.wall"
}

/// First walkable cell in row-major scan order, or `None` if every cell is a
/// wall (or the grid is empty — guaranteed by dimension validation).
fn first_walkable(cells: &[String], width: usize) -> Option<Point> {
    for (i, cell) in cells.iter().enumerate() {
        if cell != "terrain.wall" {
            let x = (i % width) as i32;
            let y = (i / width) as i32;
            return Some(Point::new(x, y));
        }
    }
    None
}

/// Parse an `"x,y"` overlay key into `(x, y)`.
fn parse_cell_key(key: &str) -> Option<(usize, usize)> {
    let (x, y) = key.split_once(',')?;
    Some((x.parse().ok()?, y.parse().ok()?))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::vfs::MemVfs;

    /// The classic demo room the old hardcoded map provided: a bordered
    /// all-floor grid with a pillar at `x % 8 == 4 && y % 6 == 3`. Tests that
    /// exercise wall collision / scheduling rely on this exact layout.
    pub(crate) fn demo_zone(width: usize, height: usize) -> Zone {
        let mut cells = vec!["terrain.floor".to_string(); width * height];
        for y in 0..height {
            for x in 0..width {
                let edge = x == 0 || y == 0 || x == width - 1 || y == height - 1;
                let pillar = x % 8 == 4 && y % 6 == 3;
                if edge || pillar {
                    cells[y * width + x] = "terrain.wall".to_string();
                }
            }
        }
        Zone::from_cells("test", width, height, Point::new(1, 1), cells)
    }

    fn tiles() -> TileRegistry {
        let v = MemVfs::new().with(
            "tiles/t.toml",
            "\"terrain.floor\" = { glyph = \".\", fg = \"#333333\" }\n\
             \"terrain.wall\" = { glyph = \"#\", fg = \"#888888\" }\n",
        );
        TileRegistry::load(&v).unwrap()
    }

    fn load_from(vfs: &MemVfs) -> Result<WorldMap, ZoneError> {
        let t = tiles();
        load_zones_from(vfs, &t, "zones")
    }

    /// A zone whose `[gen]` carries `type = "ascii"`, the floor/wall glyph
    /// mapping, plus `body` lines and an `[gen.ascii]` section holding
    /// the given rows (each element is one quoted-row string, no brackets).
    fn zone_toml(body: &str, rows: &[&str]) -> String {
        let rows_str = rows
            .iter()
            .map(|r| format!("    \"{r}\""))
            .collect::<Vec<_>>()
            .join(",\n");
        format!(
            "id = \"start\"\n[gen]\ntype = \"ascii\"\n{body}\n\
             [gen.glyphs]\n\".\" = \"terrain.floor\"\n\"#\" = \"terrain.wall\"\n\
             [gen.ascii]\nrows = [\n{rows_str}\n]\n"
        )
    }

    #[test]
    fn loads_basic_ascii_zone() {
        let s = zone_toml("", &["###", "#.#", "###"]);
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let world = load_from(&vfs).unwrap();
        assert_eq!(world.len(), 1);
        let z = world.get("start").unwrap();
        assert_eq!((z.width, z.height), (3, 3));
        assert!(z.is_walkable(1, 1));
        assert!(!z.is_walkable(0, 0));
        assert!(!z.is_walkable(2, 1));
        assert_eq!(z.tile_id(1, 1), "terrain.floor");
        // Default start = first walkable cell in row-major scan.
        assert_eq!(z.start, Point::new(1, 1));
    }

    // ---- validation: unknown generator -------------------------------------

    #[test]
    fn unknown_generator_is_a_load_error() {
        let vfs = MemVfs::new().with(
            "zones/start.toml",
            "id = \"start\"\n[gen]\ntype = \"nope\"\n[gen.glyphs]\n\".\" = \"terrain.floor\"\n",
        );
        let err = load_from(&vfs).unwrap_err();
        assert!(err.to_string().contains("zones/start.toml"), "{err}");
        assert!(err.to_string().contains("unknown generator"), "{err}");
    }

    // ---- validation: invalid dimensions / parameters -----------------------

    #[test]
    fn zero_dimensions_are_a_load_error() {
        let s = zone_toml("width = 0\n", &["."]);
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let err = load_from(&vfs).unwrap_err();
        assert!(err.to_string().contains("invalid dimensions"), "{err}");
    }

    #[test]
    fn height_mismatching_grid_rows_is_a_load_error() {
        let s = zone_toml("height = 5\n", &["...", "...", "..."]);
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let err = load_from(&vfs).unwrap_err();
        assert!(err.to_string().contains("rows"), "{err}");
    }

    #[test]
    fn ragged_ascii_rows_are_a_load_error() {
        let s = zone_toml("", &["..", "....."]);
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let err = load_from(&vfs).unwrap_err();
        assert!(err.to_string().contains("width"), "{err}");
    }

    #[test]
    fn empty_ascii_grid_is_a_load_error() {
        let s = zone_toml("", &[]);
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let err = load_from(&vfs).unwrap_err();
        assert!(err.to_string().contains("at least one row"), "{err}");
    }

    // ---- validation: unknown glyph and unknown terrain ---------------------

    #[test]
    fn unknown_glyph_is_a_load_error() {
        let s = zone_toml("", &[".~."]);
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let err = load_from(&vfs).unwrap_err();
        assert!(err.to_string().contains("~"), "{err}");
        assert!(err.to_string().contains("no mapping"), "{err}");
    }

    #[test]
    fn unknown_terrain_id_is_a_load_error() {
        let vfs = MemVfs::new().with(
            "zones/start.toml",
            "id = \"start\"\n[gen]\ntype = \"ascii\"\n\
             [gen.glyphs]\n\"l\" = \"terrain.lava\"\n\
             [gen.ascii]\nrows = [\"l\"]\n",
        );
        let t = tiles(); // has floor + wall, no lava
        let err = load_zones_from(&vfs, &t, "zones").unwrap_err();
        assert!(err.to_string().contains("terrain.lava"), "{err}");
        assert!(err.to_string().contains("unknown terrain"), "{err}");
    }

    // ---- selection: unknown --zone / multiple zones ------------------------

    #[test]
    fn unknown_requested_zone_is_an_error() {
        let s = zone_toml("", &["."]);
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let world = load_from(&vfs).unwrap();
        match world.resolve(Some("missing")) {
            Err(ZoneError::UnknownZone(id)) => assert_eq!(id, "missing"),
            other => panic!("expected UnknownZone, got {other:?}"),
        }
    }

    #[test]
    fn multiple_zones_without_selection_is_an_error() {
        let base = "id = \"{id}\"\n[gen]\ntype = \"ascii\"\n\
                    [gen.glyphs]\n\".\" = \"terrain.floor\"\n\
                    [gen.ascii]\nrows = [\".\"]\n";
        let vfs = MemVfs::new()
            .with("zones/a.toml", base.replace("{id}", "a"))
            .with("zones/b.toml", base.replace("{id}", "b"));
        let world = load_from(&vfs).unwrap();
        assert_eq!(world.len(), 2);
        match world.resolve(None) {
            Err(ZoneError::MultipleZones) => {}
            other => panic!("expected MultipleZones, got {other:?}"),
        }
        // Explicit selection disambiguates.
        assert_eq!(world.resolve(Some("b")).unwrap().id, "b");
    }

    #[test]
    fn single_zone_resolves_without_selection() {
        let s = zone_toml("", &["."]);
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let world = load_from(&vfs).unwrap();
        assert_eq!(world.resolve(None).unwrap().id, "start");
        assert_eq!(world.resolve(Some("start")).unwrap().id, "start");
    }

    #[test]
    fn empty_pack_is_an_error() {
        let world = WorldMap::new();
        match world.resolve(None) {
            Err(ZoneError::NoZones) => {}
            other => panic!("expected NoZones, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_zone_id_is_a_load_error() {
        let base = "id = \"dup\"\n[gen]\ntype = \"ascii\"\n\
                    [gen.glyphs]\n\".\" = \"terrain.floor\"\n\
                    [gen.ascii]\nrows = [\".\"]\n";
        let vfs = MemVfs::new()
            .with("zones/x.toml", base)
            .with("zones/y.toml", base);
        let err = load_from(&vfs).unwrap_err();
        assert!(err.to_string().contains("duplicate zone id"), "{err}");
    }

    // ---- overlay order -----------------------------------------------------

    #[test]
    fn overlay_forces_cells_after_generation() {
        let file = "id = \"start\"\n[gen]\ntype = \"ascii\"\n\
             [gen.glyphs]\n\".\" = \"terrain.floor\"\n\"#\" = \"terrain.wall\"\n\
             [gen.ascii]\nrows = [\n    \"...\",\n    \"...\",\n    \"...\"\n]\n\
             [gen.overlay]\n\"0,0\" = \"terrain.wall\"\n\"2,2\" = \"terrain.wall\"\n\"1,1\" = \"terrain.wall\"\n";
        let vfs = MemVfs::new().with("zones/start.toml", file);
        let world = load_from(&vfs).unwrap();
        let z = world.get("start").unwrap();
        assert!(!z.is_walkable(0, 0), "overlay walled 0,0");
        assert!(!z.is_walkable(2, 2), "overlay walled 2,2");
        assert!(!z.is_walkable(1, 1), "overlay walled 1,1");
        assert!(z.is_walkable(0, 1), "non-overlaid cell untouched");
        // Default start = first walkable cell, row-major: (1,0) is the first
        // floor after overlay walls at (0,0),(1,1),(2,2).
        assert_eq!(z.start, Point::new(1, 0));
    }

    #[test]
    fn overlay_overrides_generator_terrain() {
        // Every generated cell is floor; the overlay turns (0,0) into wall,
        // proving overlay wins over generated terrain.
        let file = "id = \"start\"\n[gen]\ntype = \"ascii\"\n\
             [gen.glyphs]\n\".\" = \"terrain.floor\"\n\"#\" = \"terrain.wall\"\n\
             [gen.ascii]\nrows = [\"..\", \"..\"]\n\
             [gen.overlay]\n\"0,0\" = \"terrain.wall\"\n";
        let vfs = MemVfs::new().with("zones/start.toml", file);
        let world = load_from(&vfs).unwrap();
        let z = world.get("start").unwrap();
        assert!(!z.is_walkable(0, 0), "overlay wins over generated floor");
        assert_eq!(z.tile_id(0, 0), "terrain.wall");
        assert!(z.is_walkable(1, 0), "neighbour still floor");
    }

    #[test]
    fn overlay_unknown_terrain_or_out_of_bounds_is_a_load_error() {
        let file = "id = \"start\"\n[gen]\ntype = \"ascii\"\n\
             [gen.glyphs]\n\".\" = \"terrain.floor\"\n\"#\" = \"terrain.wall\"\n\
             [gen.ascii]\nrows = [\"...\"]\n\
             [gen.overlay]\n\"0,0\" = \"terrain.lava\"\n";
        let vfs = MemVfs::new().with("zones/start.toml", file);
        let t = tiles();
        let err = load_zones_from(&vfs, &t, "zones").unwrap_err();
        assert!(err.to_string().contains("terrain.lava"), "{err}");

        let file = "id = \"start\"\n[gen]\ntype = \"ascii\"\n\
             [gen.glyphs]\n\".\" = \"terrain.floor\"\n\"#\" = \"terrain.wall\"\n\
             [gen.ascii]\nrows = [\"...\"]\n\
             [gen.overlay]\n\"9,9\" = \"terrain.wall\"\n";
        let vfs = MemVfs::new().with("zones/start.toml", file);
        let err = load_from(&vfs).unwrap_err();
        assert!(err.to_string().contains("out of bounds"), "{err}");
    }

    #[test]
    fn overlay_malformed_key_is_a_load_error() {
        let file = "id = \"start\"\n[gen]\ntype = \"ascii\"\n\
             [gen.glyphs]\n\".\" = \"terrain.floor\"\n\"#\" = \"terrain.wall\"\n\
             [gen.ascii]\nrows = [\"...\"]\n\
             [gen.overlay]\n\"not-a-key\" = \"terrain.wall\"\n";
        let vfs = MemVfs::new().with("zones/start.toml", file);
        let err = load_from(&vfs).unwrap_err();
        assert!(err.to_string().contains("not \"x,y\""), "{err}");
    }

    // ---- cave generator ---------------------------------------------------

    fn cave_toml(body: &str) -> String {
        format!(
            "id = \"start\"\n[gen]\ntype = \"caves\"\nwidth = 30\nheight = 20\nseed = 20260318\nfill = 45\npasses = 2\n{body}"
        )
    }

    #[test]
    fn cave_zone_loads_and_has_single_region() {
        let vfs = MemVfs::new().with("zones/start.toml", cave_toml(""));
        let world = load_from(&vfs).unwrap();
        let z = world.get("start").unwrap();
        assert_eq!((z.width, z.height), (30, 20));
        // Border is all wall.
        for y in 0..z.height {
            assert!(!z.is_walkable(0, y as i32));
            assert!(!z.is_walkable(z.width as i32 - 1, y as i32));
        }
        for x in 0..z.width {
            assert!(!z.is_walkable(x as i32, 0));
            assert!(!z.is_walkable(x as i32, z.height as i32 - 1));
        }
        // Default start = first walkable cell, row-major, and it is walkable.
        assert!(z.is_walkable(z.start.x, z.start.y));
    }

    #[test]
    fn cave_missing_seed_or_dimensions_is_a_load_error() {
        // Missing seed.
        let s = "id = \"start\"\n[gen]\ntype = \"caves\"\nwidth = 30\nheight = 20\nfill = 45\n";
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let err = load_from(&vfs).unwrap_err();
        assert!(err.to_string().contains("requires `seed`"), "{err}");

        // Missing width.
        let s = "id = \"start\"\n[gen]\ntype = \"caves\"\nheight = 20\nseed = 1\n";
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let err = load_from(&vfs).unwrap_err();
        assert!(err.to_string().contains("requires `width`"), "{err}");
    }

    #[test]
    fn cave_invalid_fill_or_passes_is_a_load_error() {
        let s = "id = \"start\"\n[gen]\ntype = \"caves\"\nwidth = 30\nheight = 20\n\
                 seed = 1\nfill = 101\npasses = 2\n";
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let err = load_from(&vfs).unwrap_err();
        assert!(err.to_string().contains("fill"), "{err}");

        let s = "id = \"start\"\n[gen]\ntype = \"caves\"\nwidth = 30\nheight = 20\n\
                 seed = 1\nfill = 45\npasses = 0\n";
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let err = load_from(&vfs).unwrap_err();
        assert!(err.to_string().contains("passes"), "{err}");
    }

    #[test]
    fn cave_overlay_applies_after_generation() {
        let s = cave_toml("[gen.overlay]\n\"1,1\" = \"terrain.wall\"\n");
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let world = load_from(&vfs).unwrap();
        let z = world.get("start").unwrap();
        assert_eq!(
            z.tile_id(1, 1),
            "terrain.wall",
            "overlay wins over cave cells"
        );
    }

    // ---- ascii round-trip --------------------------------------------------

    #[test]
    fn ascii_round_trip_preserves_rendered_grid() {
        // Render the loaded zone's cells back through the tile registry glyphs
        // ('#' for wall, '.' for floor) and compare with the authored rows.
        let reg = tiles();
        let s = zone_toml("", &["###", "#.#", "###"]);
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let world = load_zones_from(&vfs, &reg, "zones").unwrap();
        let z = world.get("start").unwrap();

        let mut rendered = String::new();
        for y in 0..z.height {
            for x in 0..z.width {
                let id = z.tile_id(x, y);
                let tile = reg.get(id);
                match tile.visual {
                    crate::tiles::TileVisual::Glyph { ch } => rendered.push(ch),
                }
            }
            if y + 1 < z.height {
                rendered.push('\n');
            }
        }
        assert_eq!(rendered, "###\n#.#\n###");
    }

    // ---- start tile walkability --------------------------------------------

    #[test]
    fn start_defaults_to_first_walkable_cell_row_major() {
        let s = zone_toml("", &["#", "."]);
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let world = load_from(&vfs).unwrap();
        let z = world.get("start").unwrap();
        assert!(!z.is_walkable(0, 0));
        assert!(z.is_walkable(0, 1));
        assert_eq!(z.start, Point::new(0, 1));
    }

    #[test]
    fn explicit_start_must_be_walkable_else_load_error() {
        // Explicit start on a wall cell must fail closed.
        let s = zone_toml("start = { x = 0, y = 0 }\n", &["#."]);
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let err = load_from(&vfs).unwrap_err();
        assert!(
            err.to_string().contains("not walkable"),
            "expected not-walkable error, got {err}"
        );

        // Explicit start on a floor cell is accepted.
        let s = zone_toml("start = { x = 1, y = 0 }\n", &["#."]);
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let world = load_from(&vfs).unwrap();
        assert_eq!(world.get("start").unwrap().start, Point::new(1, 0));
    }

    #[test]
    fn all_wall_zone_has_no_walkable_start() {
        let s = zone_toml("", &["###"]);
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let err = load_from(&vfs).unwrap_err();
        assert!(err.to_string().contains("no walkable start tile"), "{err}");
    }

    #[test]
    fn missing_tile_ids_reports_undefined_terrains() {
        let reg = TileRegistry::new(); // empty registry
        let z = Zone {
            id: "z".into(),
            width: 2,
            height: 1,
            start: Point::new(0, 0),
            cells: vec!["terrain.floor".into(), "terrain.wall".into()],
        };
        assert_eq!(
            z.missing_tile_ids(&reg),
            vec!["terrain.floor".to_string(), "terrain.wall".to_string()]
        );
    }

    #[test]
    fn in_bounds_and_tile_id_behave() {
        let s = zone_toml("", &["##", "#."]);
        let vfs = MemVfs::new().with("zones/start.toml", s);
        let world = load_from(&vfs).unwrap();
        let z = world.get("start").unwrap();
        assert!(z.in_bounds(0, 0));
        assert!(z.in_bounds(1, 1));
        assert!(!z.in_bounds(2, 0));
        assert!(!z.in_bounds(-1, 0));
        assert!(z.is_walkable(1, 1));
        assert!(!z.is_walkable(1, 0));
        assert!(!z.is_walkable(99, 99), "out of bounds is not walkable");
    }
}
