//! The trait vocabulary every other crate depends on.

/// The central trait. `Worker` implements it; `Idle` does not.
pub trait Runner {
    fn run(&self) -> u32;

    /// A default method, so "implements" and "overrides" can be told
    /// apart: an impl that does not write this still has it.
    fn describe(&self) -> u32 {
        self.run()
    }
}

/// A second trait with the *same* member name. Nothing may resolve
/// `run` by name alone.
pub trait Reporter {
    fn run(&self) -> u32;
}

/// A supertrait relationship. Rust has no class inheritance, so this
/// must never become EXTENDS.
pub trait Detailed: Runner {
    fn detail(&self) -> u32;
}

/// An associated type, probed rather than assumed.
pub trait Source {
    type Item;
    fn first(&self) -> Self::Item;
}

pub struct Model {
    pub count: u32,
}

impl Model {
    pub fn new(count: u32) -> Self {
        Self { count }
    }
}

pub enum Level {
    Low,
    High,
}
