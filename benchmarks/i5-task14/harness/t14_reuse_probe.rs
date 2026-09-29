// I5 Task 14 measurement only; not part of the product. Copy to
// crates/engine/examples/ in a scratch clone and build with a separate
// CARGO_TARGET_DIR. Counts same-revision source reads (PlannerStats) for
// repeated `inspect`, with reuse disabled vs retained.
//   t14_reuse_probe <global.db> <workspace-id> <resource-path | symbol-name>
use std::path::Path;

use brainprint_core::WorkspaceId;
use brainprint_engine::{
    projection::{
        ProjectionTarget, ResourceTarget, SymbolName, SymbolTarget,
        planner::{ContextRetention, DeliveryBudget, DeliveryLedger, LedgerLimits},
    },
    query_surface::{CoreQuerySurface, DeliveryOptions, InspectRequest, QueryContext},
};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let ws: WorkspaceId = a[2].parse().unwrap();
    let surface = CoreQuerySurface::open(Path::new(&a[1]), ws).unwrap();
    let mut ledger = DeliveryLedger::new(LedgerLimits::new(16, 1024).unwrap());
    let mut prev = surface.planner_stats();
    for mode in [
        ContextRetention::ReuseDisabled,
        ContextRetention::RetainedContext,
    ] {
        for i in 1..=3 {
            let req = InspectRequest {
                context: QueryContext {
                    workspace: ws,
                    correlation: None,
                },
                target: if a[3].contains('/') {
                    ProjectionTarget::Resource(ResourceTarget::Path(a[3].clone()))
                } else {
                    ProjectionTarget::Symbol(SymbolTarget::new(SymbolName::Name(a[3].clone())))
                },
                delivery: DeliveryOptions {
                    budget: DeliveryBudget::new(Some(64), Some(64 * 1024), None).unwrap(),
                    continuation: None,
                    retention: mode,
                    tokens: None,
                },
            };
            let out = surface.inspect(req, &mut ledger);
            let s = surface.planner_stats();
            println!(
                "{:?} #{i} ok={} plans+{} file_reads+{} source_bytes+{}",
                mode,
                out.is_ok(),
                s.plans - prev.plans,
                s.source_file_reads - prev.source_file_reads,
                s.source_bytes - prev.source_bytes
            );
            prev = s;
        }
    }
}
