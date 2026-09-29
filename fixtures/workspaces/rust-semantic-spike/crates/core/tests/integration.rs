//! An integration test target: a third crate target in one package.

use bp_contracts::Runner;
use bp_core::runner::Worker;

#[test]
fn a_worker_reports_its_seed() {
    // Bound to locals so this test exercises ordinary call sites. A
    // call written inside a macro's arguments is a call *candidate*
    // (#47), and target_probe.rs is where those shapes live.
    let worker = Worker::new(5);
    let seed = worker.run();
    assert_eq!(seed, 5);
}
