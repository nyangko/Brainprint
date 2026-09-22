//! An integration test target: a third crate target in one package.

use bp_contracts::Runner;
use bp_core::runner::Worker;

#[test]
fn a_worker_reports_its_seed() {
    assert_eq!(Worker::new(5).run(), 5);
}
