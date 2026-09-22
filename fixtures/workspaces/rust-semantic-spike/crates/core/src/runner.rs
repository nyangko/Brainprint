//! Trait implementations, inherent impls, and the same-name traps.

use bp_contracts::{Detailed, Reporter, Runner};

pub struct Worker {
    seed: u32,
}

/// An inherent impl. `run` here is a trap: it must never become a
/// trait implementation, and `Worker` must not IMPLEMENT anything
/// because of it.
impl Worker {
    /// An associated function.
    pub fn new(seed: u32) -> Self {
        Self { seed }
    }

    /// An inherent method whose name matches no trait member.
    pub fn execute(&self) -> u32 {
        self.seed
    }
}

impl Runner for Worker {
    fn run(&self) -> u32 {
        self.seed
    }
}

impl Reporter for Worker {
    /// The same member name, from a different trait. Nothing may
    /// resolve `run` by name alone.
    fn run(&self) -> u32 {
        self.seed + 1
    }
}

impl Detailed for Worker {
    fn detail(&self) -> u32 {
        self.seed + 2
    }
}

/// Implements nothing. Its `run` is a same-name trap on a type with no
/// trait impl at all.
pub struct Idle;

impl Idle {
    pub fn run(&self) -> u32 {
        0
    }
}

/// A second implementer, so "find the implementations" returns a set.
pub struct Other;

impl Runner for Other {
    fn run(&self) -> u32 {
        99
    }
}

/// A trait bound, written inline.
pub fn consume<T: Runner>(value: T) -> u32 {
    value.run()
}

/// The same, written as a `where` clause.
pub fn consume_where<T>(value: T) -> u32
where
    T: Runner,
{
    value.run()
}

/// A trait object: the call below cannot be statically dispatched.
pub fn consume_dyn(value: &dyn Runner) -> u32 {
    value.run()
}
