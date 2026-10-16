//! CPU-side glyph atlas: packs 8x8 bitmap glyphs into one R8 texture.
//!
//! Sprites will later be extra rectangles in the same texture; the renderer only
//! sees "slot -> uv rect", so it needs no new code path for them.

use std::collections::BTreeMap;

use font8x8::{BASIC_FONTS, BLOCK_FONTS, BOX_FONTS, LATIN_FONTS, UnicodeFonts};

pub const GLYPH_PX: u32 = 8;
pub const ATLAS_COLS: u32 = 16;
pub const ATLAS_ROWS: u32 = 16;
pub const ATLAS_W: u32 = ATLAS_COLS * GLYPH_PX;
pub const ATLAS_H: u32 = ATLAS_ROWS * GLYPH_PX;

pub struct Atlas {
    slots: BTreeMap<char, u32>,
    /// R8 pixels, `ATLAS_W * ATLAS_H`, 255 = ink.
    pub pixels: Vec<u8>,
}

fn bitmap(ch: char) -> Option<[u8; 8]> {
    BASIC_FONTS
        .get(ch)
        .or_else(|| LATIN_FONTS.get(ch))
        .or_else(|| BOX_FONTS.get(ch))
        .or_else(|| BLOCK_FONTS.get(ch))
}

impl Atlas {
    /// Build an atlas for `chars`. Slot 0 is always a solid "unknown glyph" box.
    /// Characters with no bitmap map to slot 0. At most 255 distinct characters.
    pub fn build(chars: &[char]) -> Self {
        let mut pixels = vec![0u8; (ATLAS_W * ATLAS_H) as usize];
        let mut slots = BTreeMap::new();
        let mut put = |slot: u32, rows: [u8; 8]| {
            let (cx, cy) = (
                (slot % ATLAS_COLS) * GLYPH_PX,
                (slot / ATLAS_COLS) * GLYPH_PX,
            );
            for (y, row) in rows.iter().enumerate() {
                for x in 0..8u32 {
                    // font8x8: bit 0 is the leftmost pixel.
                    if row >> x & 1 == 1 {
                        pixels[((cy + y as u32) * ATLAS_W + cx + x) as usize] = 255;
                    }
                }
            }
        };
        put(0, [0xFF, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0xFF]);
        let mut next = 1;
        for &ch in chars {
            if slots.contains_key(&ch) || next >= ATLAS_COLS * ATLAS_ROWS {
                continue;
            }
            match bitmap(ch) {
                Some(rows) => {
                    put(next, rows);
                    slots.insert(ch, next);
                    next += 1;
                }
                None => {
                    slots.insert(ch, 0);
                }
            }
        }
        Self { slots, pixels }
    }

    pub fn slot(&self, ch: char) -> u32 {
        self.slots.get(&ch).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_sign_has_ink_and_space_does_not() {
        let a = Atlas::build(&['@', ' ', '#']);
        let ink = |slot: u32| {
            let (cx, cy) = ((slot % ATLAS_COLS) * 8, (slot / ATLAS_COLS) * 8);
            (0..8)
                .flat_map(|y| (0..8).map(move |x| (x, y)))
                .filter(|&(x, y)| a.pixels[((cy + y) * ATLAS_W + cx + x) as usize] != 0)
                .count()
        };
        assert!(ink(a.slot('@')) > 8);
        assert_eq!(ink(a.slot(' ')), 0);
        assert_ne!(a.slot('@'), a.slot('#'));
    }

    #[test]
    fn unknown_chars_use_slot_zero() {
        let a = Atlas::build(&['\u{1F600}']);
        assert_eq!(a.slot('\u{1F600}'), 0);
        assert_eq!(a.slot('Z'), 0);
    }

    #[test]
    fn duplicates_share_a_slot() {
        let a = Atlas::build(&['a', 'a', 'b']);
        assert_eq!(a.slot('a'), 1);
        assert_eq!(a.slot('b'), 2);
    }
}
