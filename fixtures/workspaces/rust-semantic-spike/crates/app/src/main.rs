//! The consumer: app -> core -> contracts, through Cargo dependencies.

use bp_contracts::{Reporter, Runner};
use bp_core::model::{identity, Boxed};
use bp_core::runner::{consume, consume_dyn, Idle, Worker};
use bp_core::Model as PublicModel;
use bp_core::PublicWorker;

fn main() {
    // An associated function and an inherent method.
    let worker = Worker::new(4);
    let _inherent = worker.execute();

    // A trait method on a concrete type, and the same name from
    // another trait. These must reach different declarations.
    let _as_runner = Runner::run(&worker);
    let _as_reporter = Reporter::run(&worker);

    // Fully qualified disambiguation.
    let _ufcs = <Worker as Runner>::run(&worker);

    // A same-name inherent method on a type implementing nothing.
    let _idle = Idle.run();

    // A trait method through a generic bound, and through a trait
    // object: one is monomorphized, the other is not.
    let _bound = consume(Worker::new(1));
    let _dynamic = consume_dyn(&Worker::new(2));

    // A generic type and a generic function.
    let boxed = Boxed::new(9_u32);
    let _identity = identity(boxed.value);

    // Through the named re-export, and through the aliased one.
    let _model = PublicModel::new(3);
    let _aliased = PublicWorker::new(6);

    // A declarative macro from another crate.
    let _doubled = bp_core::doubled!(21);
}
