//! BioMerge engine: everything that does not depend on the GUI.

pub mod diff;
pub mod dirscan;
pub mod document;
pub mod extract;
pub mod fileops;
pub mod filter;
pub mod highlight;
pub mod ignore_rules;
pub mod json;
pub mod merge3;
pub mod textedit;
pub mod wrap;

/// One of the two compared sides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub fn index(self) -> usize {
        match self {
            Side::Left => 0,
            Side::Right => 1,
        }
    }

    pub fn other(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }
}
