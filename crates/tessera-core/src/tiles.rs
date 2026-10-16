//! Tile registry: the indirection between entities and how they look.
//!
//! Entities reference a tile *id* (`"creature.snapjaw"`), never a glyph or sprite.
//! A tileset file maps ids to appearance. Later tilesets (sprites) can override ids
//! one at a time; anything unresolved falls back to a visible "missing tile".

use std::collections::BTreeMap;

use serde::Deserialize;
use thiserror::Error;

use crate::vfs::Vfs;

#[derive(Debug, Error)]
pub enum TileError {
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
    #[error("{path}: tile {id:?}: {msg}")]
    Invalid {
        path: String,
        id: String,
        msg: String,
    },
}

/// 8-bit RGB color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const BLACK: Rgb = Rgb(0, 0, 0);
    pub const WHITE: Rgb = Rgb(255, 255, 255);

    /// Parse `#rrggbb`.
    pub fn parse(s: &str) -> Result<Rgb, String> {
        let h = s.strip_prefix('#').ok_or("color must start with '#'")?;
        if h.len() != 6 || !h.is_ascii() {
            return Err(format!("expected #rrggbb, got {s:?}"));
        }
        let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).map_err(|e| e.to_string());
        Ok(Rgb(p(0)?, p(2)?, p(4)?))
    }

    pub fn to_f32(self) -> [f32; 3] {
        [
            self.0 as f32 / 255.0,
            self.1 as f32 / 255.0,
            self.2 as f32 / 255.0,
        ]
    }
}

/// How a tile is drawn. Only glyphs exist today; a `Sprite` variant is added when
/// sprite support lands, without changing anything that references tile ids.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TileVisual {
    Glyph { ch: char },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tile {
    pub visual: TileVisual,
    pub fg: Rgb,
    /// `None` = transparent (draw the cell background through).
    pub bg: Option<Rgb>,
}

#[derive(Deserialize)]
struct RawTile {
    glyph: String,
    fg: Option<String>,
    bg: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TileRegistry {
    tiles: BTreeMap<String, Tile>,
    missing: Tile,
}

impl Default for TileRegistry {
    fn default() -> Self {
        Self {
            tiles: BTreeMap::new(),
            // Magenta '?' on black: impossible to miss.
            missing: Tile {
                visual: TileVisual::Glyph { ch: '?' },
                fg: Rgb(255, 0, 255),
                bg: Some(Rgb::BLACK),
            },
        }
    }
}

impl TileRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load every `tiles/*.toml` in the vfs, in sorted path order. Later files
    /// override earlier ones id-by-id (this is how sprite packs/mods layer on).
    pub fn load(vfs: &dyn Vfs) -> Result<Self, TileError> {
        let mut reg = Self::new();
        let files = vfs.list("tiles").map_err(|source| TileError::Io {
            path: "tiles".into(),
            source,
        })?;
        for path in files.iter().filter(|p| p.ends_with(".toml")) {
            let text = vfs.read_to_string(path).map_err(|source| TileError::Io {
                path: path.clone(),
                source,
            })?;
            reg.merge_toml(path, &text)?;
        }
        Ok(reg)
    }

    pub fn merge_toml(&mut self, path: &str, text: &str) -> Result<(), TileError> {
        let raw: BTreeMap<String, RawTile> =
            toml::from_str(text).map_err(|source| TileError::Parse {
                path: path.into(),
                source,
            })?;
        for (id, r) in raw {
            let bad = |msg: String| TileError::Invalid {
                path: path.into(),
                id: id.clone(),
                msg,
            };
            let mut chars = r.glyph.chars();
            let ch = match (chars.next(), chars.next()) {
                (Some(c), None) => c,
                _ => {
                    return Err(bad(format!(
                        "glyph must be one character, got {:?}",
                        r.glyph
                    )));
                }
            };
            let fg = match r.fg.as_deref() {
                Some(s) => Rgb::parse(s).map_err(bad)?,
                None => Rgb::WHITE,
            };
            let bg = match r.bg.as_deref() {
                None | Some("none") => None,
                Some(s) => Some(Rgb::parse(s).map_err(bad)?),
            };
            self.tiles.insert(
                id,
                Tile {
                    visual: TileVisual::Glyph { ch },
                    fg,
                    bg,
                },
            );
        }
        Ok(())
    }

    /// Resolve an id, falling back to the missing-tile marker.
    pub fn get(&self, id: &str) -> &Tile {
        self.tiles.get(id).unwrap_or(&self.missing)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.tiles.contains_key(id)
    }

    /// All glyph characters needed to draw this registry (for atlas building).
    pub fn glyph_chars(&self) -> Vec<char> {
        let mut v: Vec<char> = self
            .tiles
            .values()
            .chain(std::iter::once(&self.missing))
            .map(|t| match t.visual {
                TileVisual::Glyph { ch } => ch,
            })
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::MemVfs;

    #[test]
    fn parses_and_falls_back() {
        let v = MemVfs::new().with(
            "tiles/ascii.toml",
            r##"
"creature.player" = { glyph = "@", fg = "#ffffff" }
"terrain.wall" = { glyph = "#", fg = "#808080", bg = "#101010" }
"##,
        );
        let r = TileRegistry::load(&v).unwrap();
        assert_eq!(
            r.get("creature.player").visual,
            TileVisual::Glyph { ch: '@' }
        );
        assert_eq!(r.get("terrain.wall").bg, Some(Rgb(16, 16, 16)));
        assert_eq!(r.get("typo.id").visual, TileVisual::Glyph { ch: '?' });
        assert!(!r.contains("typo.id"));
    }

    #[test]
    fn later_files_override_per_id() {
        let v = MemVfs::new()
            .with(
                "tiles/a.toml",
                r##"x = { glyph = "a", fg = "#000000" }
y = { glyph = "b" }"##,
            )
            .with("tiles/b.toml", r##"x = { glyph = "z", fg = "#ff0000" }"##);
        let r = TileRegistry::load(&v).unwrap();
        assert_eq!(r.get("x").visual, TileVisual::Glyph { ch: 'z' });
        assert_eq!(r.get("y").visual, TileVisual::Glyph { ch: 'b' });
    }

    #[test]
    fn rejects_bad_input() {
        let mut r = TileRegistry::new();
        assert!(r.merge_toml("t", r#"x = { glyph = "ab" }"#).is_err());
        assert!(
            r.merge_toml("t", r##"x = { glyph = "a", fg = "red" }"##)
                .is_err()
        );
        assert!(r.merge_toml("t", "not toml [").is_err());
    }

    #[test]
    fn color_parse() {
        assert_eq!(Rgb::parse("#c8a060"), Ok(Rgb(0xc8, 0xa0, 0x60)));
        assert!(Rgb::parse("c8a060").is_err());
        assert!(Rgb::parse("#c8a0").is_err());
    }
}
