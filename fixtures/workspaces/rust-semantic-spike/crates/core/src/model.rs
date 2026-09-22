//! A module written as `model.rs` rather than `model/mod.rs`.

use bp_contracts::Level;

/// A generic struct.
pub struct Boxed<T> {
    pub value: T,
}

impl<T> Boxed<T> {
    /// An associated function on a generic type.
    pub fn new(value: T) -> Self {
        Self { value }
    }
}

/// A generic free function.
pub fn identity<T>(value: T) -> T {
    value
}

pub type Tally = u32;

pub fn level_of(count: u32) -> Level {
    if count > 3 {
        Level::High
    } else {
        Level::Low
    }
}
