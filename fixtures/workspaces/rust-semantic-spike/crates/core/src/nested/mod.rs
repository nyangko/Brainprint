//! A module written as `nested/mod.rs`, so module identity cannot be
//! assumed to be one file-naming convention.

pub mod deep;

pub struct Nested;

impl Nested {
    pub fn depth(&self) -> u32 {
        1
    }
}
