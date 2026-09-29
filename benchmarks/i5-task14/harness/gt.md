T1 ground truth (repository truth @278f4cb, verified by exhaustive text search):
- signature: pub fn load_workspace_config(paths: &WorkspacePaths) -> Result<WorkspaceConfig, ConfigError>  @ crates/engine/src/config.rs:213
- production call sites (2): crates/daemon/src/query/lifecycle.rs:115 in WorkspaceLifecycle::load ; crates/engine/src/query_surface.rs:543 in CoreQuerySurface::open
- test call sites (6): crates/engine/src/config.rs:446,462,472,525,538,550 (config.rs #[cfg(test)] module; 538/550 inside assert! macro args)
- non-call textual hits: `use` imports at lifecycle.rs:24, query_surface.rs:29; p0_39 test string literals (not calls)
T2 ground truth: same as @482347a (agent telemetry unchanged) -- verify lines below at run time
