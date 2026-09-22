//! Module hierarchy, re-exports, and the trait implementations the
//! acceptance turns on.

pub mod model;
pub mod runner;

mod nested;

/// A named re-export: a consumer reaching `bp_core::Model` must land on
/// the declaration in `bp_contracts`, not on this line.
pub use bp_contracts::Model;

/// An aliased re-export of a local item.
pub use crate::runner::Worker as PublicWorker;

/// A glob re-export, kept as a deliberate PARTIAL case.
pub use crate::nested::*;

/// A declarative macro, local to this crate.
#[macro_export]
macro_rules! doubled {
    ($value:expr) => {
        $value * 2
    };
}

/// Only compiled with the `extra` feature: one file, two cfg worlds.
#[cfg(feature = "extra")]
pub fn extra_only() -> u32 {
    7
}

#[cfg(not(feature = "extra"))]
pub fn extra_only() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::runner::Worker;
    use bp_contracts::Runner;

    #[test]
    fn a_worker_runs() {
        // Bound to locals: a call written inside `assert_eq!` is an
        // opaque token tree to the structural tier.
        let worker = Worker::new(2);
        let seed = worker.run();
        assert_eq!(seed, 2);
    }
}
