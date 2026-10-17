//! Minimal grid map for the initial demo. A later zone/chunk model replaces
//! it.

use crate::tiles::TileRegistry;

pub struct Map {
    pub width: usize,
    pub height: usize,
    /// Terrain tile id per cell, row-major.
    cells: Vec<&'static str>,
}

impl Map {
    /// Hardcoded demo room with a few pillars.
    pub fn demo(width: usize, height: usize) -> Self {
        let mut cells = vec!["terrain.floor"; width * height];
        for y in 0..height {
            for x in 0..width {
                let edge = x == 0 || y == 0 || x == width - 1 || y == height - 1;
                let pillar = x % 8 == 4 && y % 6 == 3;
                if edge || pillar {
                    cells[y * width + x] = "terrain.wall";
                }
            }
        }
        Self {
            width,
            height,
            cells,
        }
    }

    pub fn in_bounds(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height
    }

    pub fn tile_id(&self, x: usize, y: usize) -> &'static str {
        self.cells[y * self.width + x]
    }

    pub fn is_walkable(&self, x: i32, y: i32) -> bool {
        self.in_bounds(x, y) && self.tile_id(x as usize, y as usize) != "terrain.wall"
    }

    pub fn missing_tile_ids(&self, reg: &TileRegistry) -> Vec<&'static str> {
        let mut v: Vec<_> = self
            .cells
            .iter()
            .copied()
            .filter(|id| !reg.contains(id))
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walls_block_floor_does_not() {
        let m = Map::demo(20, 10);
        assert!(!m.is_walkable(0, 0));
        assert!(m.is_walkable(1, 1));
        assert!(!m.is_walkable(-1, 3));
        assert!(!m.is_walkable(4, 3)); // pillar
    }
}
