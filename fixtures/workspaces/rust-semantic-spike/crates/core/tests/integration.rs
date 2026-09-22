//! An integration test target: a third crate target in one package.

use bp_contracts::Runner;
use bp_core::runner::Worker;

#[test]
fn a_worker_reports_its_seed() {
    // Bound to locals rather than written inside `assert_eq!`: a call
    // inside a macro invocation is an opaque token tree to the
    // structural tier, so it anchors no occurrence and nothing can be
    // proved about it. The limitation is recorded in the acceptance
    // ledger; this test exercises what a caller can actually reach.
    let worker = Worker::new(5);
    let seed = worker.run();
    assert_eq!(seed, 5);
}
