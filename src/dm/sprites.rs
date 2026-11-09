//! Tiles and sprites for `!dm` maps, from Kenney's Tiny Town and Tiny Dungeon and
//! Clint Bellanger's Tiny Creatures (see `assets/dm/README.md`).
//!
//! Claude picks how things look by name, from `LOOKS`, so it never needs to know where
//! a sprite sits on a sheet.

use std::sync::OnceLock;

use image::RgbaImage;

/// Sprites are 16 pixels square on every sheet.
pub const SIZE: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sheet {
    Town,
    Dungeon,
    Creatures,
}

impl Sheet {
    fn columns(self) -> u32 {
        match self {
            Self::Town | Self::Dungeon => 12,
            Self::Creatures => 10,
        }
    }
}

/// What things can look like, by name, and where each is drawn from.
pub const LOOKS: &[(&str, Sheet, u32)] = &[
    // Monsters and beasts.
    ("bat", Sheet::Dungeon, 120),
    ("bear", Sheet::Creatures, 163),
    ("beetle", Sheet::Creatures, 143),
    ("boar", Sheet::Creatures, 160),
    ("bog-monster", Sheet::Creatures, 42),
    ("cultist", Sheet::Creatures, 19),
    ("cyclops", Sheet::Dungeon, 109),
    ("demon", Sheet::Creatures, 38),
    ("dragon", Sheet::Creatures, 33),
    ("floating-eye", Sheet::Creatures, 120),
    ("ghost", Sheet::Dungeon, 121),
    ("giant-bee", Sheet::Creatures, 140),
    ("goblin", Sheet::Creatures, 10),
    ("golem", Sheet::Creatures, 47),
    ("imp", Sheet::Creatures, 39),
    ("minotaur", Sheet::Creatures, 20),
    ("mushroom", Sheet::Creatures, 13),
    ("ogre", Sheet::Creatures, 14),
    ("orc", Sheet::Creatures, 11),
    ("rat", Sheet::Dungeon, 123),
    ("scorpion", Sheet::Creatures, 145),
    ("skeleton", Sheet::Creatures, 1),
    ("slime", Sheet::Dungeon, 108),
    ("snake", Sheet::Creatures, 40),
    ("spider", Sheet::Dungeon, 122),
    ("treant", Sheet::Creatures, 113),
    ("troll", Sheet::Creatures, 125),
    ("vampire", Sheet::Creatures, 2),
    ("werewolf", Sheet::Creatures, 24),
    ("wolf", Sheet::Creatures, 23),
    ("wraith", Sheet::Creatures, 4),
    ("yeti", Sheet::Creatures, 43),
    ("zombie", Sheet::Creatures, 0),
    // People, for players and other characters.
    ("dwarf", Sheet::Dungeon, 87),
    ("knight", Sheet::Dungeon, 96),
    ("monk", Sheet::Dungeon, 111),
    ("old-woman", Sheet::Dungeon, 100),
    ("peasant", Sheet::Dungeon, 88),
    ("ranger", Sheet::Dungeon, 112),
    ("villager", Sheet::Dungeon, 85),
    ("wizard", Sheet::Dungeon, 84),
    ("woman", Sheet::Dungeon, 99),
    // Items.
    ("axe", Sheet::Dungeon, 118),
    ("bomb", Sheet::Town, 105),
    ("bottle", Sheet::Dungeon, 113),
    ("bow", Sheet::Town, 118),
    ("bucket", Sheet::Town, 130),
    ("chest", Sheet::Dungeon, 89),
    ("coin", Sheet::Town, 93),
    ("dagger", Sheet::Dungeon, 103),
    ("hammer", Sheet::Dungeon, 117),
    ("key", Sheet::Town, 117),
    ("mana-potion", Sheet::Dungeon, 116),
    ("mimic", Sheet::Dungeon, 92),
    ("potion", Sheet::Dungeon, 115),
    ("shield", Sheet::Dungeon, 102),
    ("spear", Sheet::Dungeon, 131),
    ("sword", Sheet::Dungeon, 104),
    ("wand", Sheet::Dungeon, 129),
    // Places and furniture.
    ("altar", Sheet::Dungeon, 55),
    ("anvil", Sheet::Dungeon, 74),
    ("bookshelf", Sheet::Dungeon, 75),
    ("crystal", Sheet::Dungeon, 56),
    ("grave", Sheet::Dungeon, 64),
    ("gravestone", Sheet::Dungeon, 65),
    ("sign", Sheet::Town, 83),
    ("table", Sheet::Dungeon, 72),
    ("well", Sheet::Town, 104),
];

/// Looks two squares tall: the tile drawn in the square above, from the same sheet.
/// Their own tile alone would look cut off.
const TOPS: &[(&str, u32)] = &[("bookshelf", 63), ("well", 92)];

const TREE: u32 = 28;
const BUSH: u32 = 5;

/// What players look like until the story says otherwise, in the order they joined.
pub const PLAYER_LOOKS: [&str; 6] = ["knight", "wizard", "ranger", "dwarf", "monk", "villager"];

pub fn is_look(name: &str) -> bool {
    LOOKS.iter().any(|(look, _, _)| *look == name)
}

pub fn look_names() -> Vec<&'static str> {
    LOOKS.iter().map(|(look, _, _)| *look).collect()
}

/// The sprite for a look, if it's one of `LOOKS`, and for a tall one, its top half.
pub fn look(name: &str) -> Option<(RgbaImage, Option<RgbaImage>)> {
    let &(_, sheet, index) = LOOKS.iter().find(|(look, _, _)| *look == name)?;
    let top = TOPS.iter().find(|(look, _)| *look == name).map(|&(_, top)| sprite(sheet, top));
    Some((sprite(sheet, index), top))
}

/// The tiles a map character is drawn with: its ground, and anything standing on it,
/// like a tree on grass. `None` for ground the packs don't have, which is drawn in code.
/// `x` and `y` vary plain grass a little, so a field isn't one tile repeated.
pub fn ground(tile: char, x: usize, y: usize) -> Option<(RgbaImage, Option<RgbaImage>)> {
    let grass = || {
        let variety = (x * 7 + y * 13 + x * y) % 17;
        sprite(Sheet::Town, match variety {
            0 => 2,
            1 | 2 => 1,
            _ => 0,
        })
    };
    Some(match tile {
        '.' => (grass(), None),
        ',' => (sprite(Sheet::Town, 25), None),
        '_' => (sprite(Sheet::Town, 109), None),
        '#' => (sprite(Sheet::Dungeon, 40), None),
        '+' => (sprite(Sheet::Dungeon, 45), None),
        'c' => (sprite(Sheet::Dungeon, 22), None),
        // Single-square trees and bushes. The sheet's other trees are halves of
        // two-square ones, which look cut off alone.
        'T' => (grass(), Some(sprite(Sheet::Town, if (x * 3 + y * 5).is_multiple_of(7) { BUSH } else { TREE }))),
        _ => return None,
    })
}

fn sheets() -> &'static [RgbaImage; 3] {
    static SHEETS: OnceLock<[RgbaImage; 3]> = OnceLock::new();
    SHEETS.get_or_init(|| {
        let load = |bytes: &[u8]| image::load_from_memory(bytes).expect("bundled tile sheet is a valid image").to_rgba8();
        [
            load(include_bytes!("../../assets/dm/tiny-town.png")),
            load(include_bytes!("../../assets/dm/tiny-dungeon.png")),
            load(include_bytes!("../../assets/dm/tiny-creatures.png")),
        ]
    })
}

fn sprite(sheet: Sheet, index: u32) -> RgbaImage {
    let image = &sheets()[sheet as usize];
    let (column, row) = (index % sheet.columns(), index / sheet.columns());
    image::imageops::crop_imm(image, column * SIZE, row * SIZE, SIZE, SIZE).to_image()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_look_is_on_its_sheet_and_drawn() {
        for &(name, sheet, index) in LOOKS {
            let image = &sheets()[sheet as usize];
            let rows = image.height() / SIZE;
            assert!(index < sheet.columns() * rows, "{name} is off its sheet");
            let sprite = sprite(sheet, index);
            assert!(sprite.pixels().any(|pixel| pixel[3] > 0), "{name} is blank");
        }
        let mut names = look_names();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), LOOKS.len(), "look names are unique");
        assert!(PLAYER_LOOKS.iter().all(|look| is_look(look)));
    }

    /// Opaque pixels along each edge: top, bottom, left, right.
    fn edges(sprite: &RgbaImage) -> [usize; 4] {
        let row = |y: u32| (0..SIZE).filter(|&x| sprite.get_pixel(x, y)[3] > 0).count();
        let column = |x: u32| (0..SIZE).filter(|&y| sprite.get_pixel(x, y)[3] > 0).count();
        [row(0), row(SIZE - 1), column(0), column(SIZE - 1)]
    }

    #[test]
    fn looks_stand_alone() {
        for &(name, _, _) in LOOKS {
            let (sprite, top) = look(name).unwrap();
            let [top_edge, _, left, right] = edges(&sprite);
            // A whole square of wall or floor, like the sheet's wall banners, looks like a
            // stray block out in a field.
            let full = SIZE as usize;
            assert!(top.is_some() || !(top_edge == full && left == full && right == full), "{name} is a block of wall or floor");
            // A tall look's lower half carries on upward, and its top half stops.
            if let Some(top) = top {
                assert!(top_edge > 8, "{name}'s lower half doesn't need a top");
                assert!(edges(&top)[0] < full, "{name}'s top is cut off too");
            }
        }
    }

    /// A whole sprite rounds off or comes to a point at its top edge. Half of a taller
    /// one, like the tall tree's lower half that was once drawn as a tree, runs across it.
    fn cut_off_at_the_top(sprite: &RgbaImage) -> bool {
        (0..SIZE).filter(|&x| sprite.get_pixel(x, 0)[3] > 0).count() > 8
    }

    #[test]
    fn trees_are_whole() {
        for x in 0..10 {
            for y in 0..10 {
                let (_, tree) = ground('T', x, y).unwrap();
                assert!(!cut_off_at_the_top(&tree.unwrap()), "the tree at ({x}, {y}) is cut off");
            }
        }
        assert!(cut_off_at_the_top(&sprite(Sheet::Town, 16)), "the check catches the tall tree's lower half");
        let kinds: std::collections::HashSet<Vec<u8>> = (0..10).map(|x| ground('T', x, 2).unwrap().1.unwrap().into_raw()).collect();
        assert_eq!(kinds.len(), 2, "forests mix trees and bushes");
    }

    #[test]
    fn ground_has_tiles_for_most_map_characters() {
        for tile in ['.', ',', '_', '#', '+', 'c', 'T'] {
            let (ground, _) = ground(tile, 3, 4).unwrap_or_else(|| panic!("no tile for {tile:?}"));
            assert_eq!(ground.dimensions(), (SIZE, SIZE));
            assert!(ground.pixels().all(|pixel| pixel[3] == 255), "{tile:?}'s ground has no holes");
        }
        assert!(ground('T', 0, 0).unwrap().1.is_some(), "trees stand on grass");
        // Drawn in code instead.
        for tile in ['~', '^', '*', ' '] {
            assert!(ground(tile, 0, 0).is_none());
        }
    }
}
