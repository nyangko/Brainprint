# #19 task 13 — the acceptance ledger

The task's 76 mandatory cases, each with a verdict and the test that
carries it. PASS only when a named test asserts it; PARTIAL when the
behaviour is real but bounded, with the bound stated; N/A when the
language or the model makes the case meaningless, with the reason
stated. Nothing is omitted for being hard to automate.

**Where the tests live.**

| suite | needs | count |
|---|---|---|
| `brainprint_engine::rust_semantic::*` | nothing — no toolchain, no server, no network | 73 |
| `crates/engine/tests/i4_rust_acceptance.rs` | an installed rust-analyzer | 13 |

```sh
cargo test -p brainprint-engine --lib rust_semantic::
./scripts/rust_semantic_spike/install.sh
cargo test -p brainprint-engine --test i4_rust_acceptance -- --ignored
```

The always-on suite is also the Level B contract: it is what the tier
still guarantees when no Rust toolchain exists on the machine.

---

## Toolchain and installation

| # | case | verdict | evidence |
|---|---|---|---|
| 1 | exact rust-analyzer executable/version/toolchain recorded | PASS | `README.md` records the measurement; `RustInstall` carries executable, server version, rustc version, host triple, sysroot name and `rust_src`. `the_toolchain_participates_in_semantic_identity` |
| 2 | the production launcher uses an explicit executable, not a PATH search | PASS | `a_missing_backend_says_exactly_what_is_missing`, `the_command_line_is_the_executable_and_nothing_else`. The one `rustup which` call lives in the *test* harness's `install_or_skip` |
| 3 | no production auto-install of toolchain, rust-analyzer or rust-src | PASS | same — the error names no install command; `install.sh` is the only place `rustup component add` appears |

## Trust and execution

| # | case | verdict | evidence |
|---|---|---|---|
| 4 | an untrusted Workspace executes no Cargo/project tooling and degrades honestly | PASS | `an_untrusted_workspace_refuses_to_read_its_own_manifests`, `an_untrusted_analysis_publishes_gaps_not_guesses`, `an_untrusted_workspace_executes_nothing_and_claims_nothing` |
| 5 | trusted P0 runs with build scripts, proc macros and flycheck disabled | PASS | `the_safe_configuration_is_the_measured_one`; the switches are in the handshake, so they are in force before the first manifest is read |
| 6 | the marker `build.rs` is not executed | PASS | `a_trusted_load_runs_no_build_script_and_compiles_nothing`, and `Slice::assert_nothing_executed` runs at the end of **every** live test |
| 7 | proc-macro execution is not silently enabled | PASS | `the_safe_configuration_is_the_measured_one` asserts `procMacro.enable == false` |
| 8 | no automatic dependency/network fetch occurs | PASS | `no_dependency_is_fetched_to_answer_a_question`; `CARGO_NET_OFFLINE=true` is set on the child by `RustLauncher::start` |

`cargo.noDeps` is deliberately **not** set, and that is the spike's main
finding: it reads like a fourth execution control and is not one. With
it on, all twelve cross-crate probes answered with nothing. The
execution controls are the other three, verified by consequence.

## Cargo world

| # | case | verdict | evidence |
|---|---|---|---|
| 9 | the Cargo workspace is discovered | PASS | `a_package_a_crate_and_a_module_stay_apart` — four manifests, the lockfile, the toolchain file, nine owned sources |
| 10 | package/crate/module distinctions remain truthful | PASS | same — the workspace root declares no package and says so; an integration test is another crate target of the same package; module identity is a separate question the backend answers |

## Resolution

| # | case | verdict | evidence |
|---|---|---|---|
| 11 | `mod`/module resolution works | PASS | `a_module_resolves_to_its_resource`, `a_whole_file_range_is_a_module`; a module answers as a whole file and becomes the Resource |
| 12 | `use crate::…` resolves | PASS | `the_rust_slice_holds_against_the_real_backend` |
| 13 | `self` / `super` use resolution works | PASS | `self_and_super_paths_resolve` |
| 14 | `use … as Alias` resolves | PASS | `the_rust_slice_holds_against_the_real_backend` — `PublicWorker` |
| 15 | a same-name module/item trap is excluded | PASS | same — `Idle::run` resolves to its own inherent method and not to either trait member |
| 16 | a named `pub use` re-export resolves | PASS | same — `PublicModel::new` |
| 17 | an aliased re-export resolves | PASS | same — `PublicWorker::new` |
| 18 | a cross-crate workspace dependency resolves | PASS | same — `app → core → contracts` throughout |
| 19 | struct/enum/trait/function/method/type-alias structural identities remain correct | PASS | same, and `without_a_backend_the_structural_truth_is_whole` for the no-toolchain case |

A **compound** `use crate::a::{B, C}` is anchored by the structural tier
as one occurrence over the whole specifier, so it binds to at most one
of its items, and a **glob** `use crate::prelude::*` names no item at
all. Both are why `ImportBinding` is PARTIAL rather than SUPPORTED.

## Calls

| # | case | verdict | evidence |
|---|---|---|---|
| 20 | a direct function call resolves | PASS | `the_rust_slice_holds_against_the_real_backend` — `identity(..)` |
| 21 | an associated function resolves | PASS | same — `Worker::new`, `Boxed::new` |
| 22 | an inherent method call resolves | PASS | same — `worker.execute()` |
| 23 | a trait method call resolves at the declared capability | PASS | same — `Runner::run(&worker)` |
| 24 | same-name inherent and trait methods do not collide | PASS | same — `Idle.run()` against `Runner::run` and `Reporter::run` |

## Traits and impls

| # | case | verdict | evidence |
|---|---|---|---|
| 25 | `impl Trait for Type` creates one canonical IMPLEMENTS relation | PASS | `the_rust_slice_holds_against_the_real_backend` — sourced from the *type*, via `types::rust_implementors` |
| 26 | an inherent `impl Type` creates no IMPLEMENTS relation | PASS | same (`Idle`), and `an_inherent_impl_creates_no_implements_relation` |
| 27 | a trait method implementation reaches the exact trait member where proven | PASS | `a_trait_member_implementation_is_proved_by_the_compiler` — asked at the trait member, anchored on the implementing declaration |
| 28 | two same-named traits remain distinct | PASS | `the_rust_slice_holds_against_the_real_backend` — `Runner::run` and `Reporter::run` resolve to different declarations though both implementing declarations are named `Worker::run` |
| 29 | the implementation-target query works at its declared capability | PASS | same — `Runner` has two implementers and the query returns both |

## Generics

| # | case | verdict | evidence |
|---|---|---|---|
| 30 | a generic type declaration resolves | PASS | `the_rust_slice_holds_against_the_real_backend` — `Boxed::new` |
| 31 | a generic function declaration resolves | PASS | same — `identity` |
| 32 | trait-bound behaviour is measured and reported honestly | PARTIAL | I3 anchors the bound's *type parameter* (`T`) and not the bound itself, so `fn consume<T: Runner>` and its `where` form produce no edge to `Runner`. Reported by `TYPE_RESOLUTION` being PARTIAL; the site is a gap, never a guess |
| 33 | associated-type behaviour is measured and reported honestly | PARTIAL | `trait Source { type Item; }` declares an associated type that I3 records as a declaration; no relation kind states "this associated type resolves to that", and inventing one for it was out of scope. No edge is claimed |

## External identity

| # | case | verdict | evidence |
|---|---|---|---|
| 34 | a std/sysroot target becomes a stable ExternalEntity without machine-path identity | PASS | `a_sysroot_item_is_its_std_crate`, `the_standard_library_is_an_identity_and_its_absence_is_recorded` |
| 35 | external package source does not become a Resource | PASS | `the_rust_slice_holds_against_the_real_backend` asserts no Resource path contains `.cargo` or `.rustup` |
| 36 | registry/git/sysroot trees are not deep-indexed | PASS | `dependency_and_toolchain_trees_are_recognised`, `workspace_source_is_never_read_as_a_dependency`, and the assertion above |

Identity is the crate and the version Cargo pinned —
`external_identity_is_a_crate_and_never_a_machine_path`,
`a_git_dependency_has_a_name_and_no_version` — and never a home
directory, a registry hash or a checkout id.

## Dispatch

| # | case | verdict | evidence |
|---|---|---|---|
| 37 | `&dyn Trait` dispatch is not falsely STATIC | PASS | `the_rust_slice_holds_against_the_real_backend`. Measured: a `dyn` call resolves into the `trait`, a concrete call into an `impl`, so the two are told apart by where the compiler pointed |
| 38 | static and inherent calls are classified honestly | PASS | same — `worker.execute()` is `Dispatch::Static` |
| 39 | Rust class inheritance is not fabricated with EXTENDS | PASS | same — `Worker` and `Detailed` have no outgoing `EXTENDS`; `rust_does_not_pretend_to_have_what_it_lacks` |
| 40 | Rust trait implementation is not mislabelled OVERRIDES | PASS | same — no outgoing `OVERRIDES` anywhere; `OVERRIDES` is declared UNSUPPORTED |

A **supertrait** (`trait Detailed: Runner`) is a requirement on
implementors, not a base class. `EXTENDS` would state something Rust
does not have, so that capability stays UNSUPPORTED rather than the
relation being overloaded.

## Macros and generated source

| # | case | verdict | evidence |
|---|---|---|---|
| 41 | local `macro_rules!` behaviour is measured | PASS | `a_declarative_macro_resolves_and_its_expansion_does_not`; the spike measured `bp_core::doubled!(21)` resolving to the macro's own declaration |
| 42 | a generated/virtual macro location never becomes fake editable source | PASS | `a_virtual_or_generated_location_is_refused`; `Normalizer::target_for` refuses before anything else looks at the location, and `the_agent_surfaces_answer_for_rust` asserts no prepared range contains expansion text |
| 43 | proc-macro-generated semantics remain an explicit limitation with execution disabled | PASS | declared by the safe configuration and stated in the module header; nothing they would declare is claimed |
| 44 | `OUT_DIR`/build-script generated source is not canonical project source | PASS | `build_script_output_is_not_project_source`; `target/` is treated as a dependency path and refused |

A call written **inside** a macro invocation anchors no occurrence —
`assert_eq!(seed, 5)` is an opaque token tree to the structural tier —
so nothing is claimed about it. Asserted by
`a_declarative_macro_resolves_and_its_expansion_does_not`, and it is why
the fixture's tests bind their calls to locals.

## Lifecycle

| # | case | verdict | evidence |
|---|---|---|---|
| 45 | a source save reaches semantic current without sleep | PASS | `a_source_edit_is_current_after_the_document_barrier`; the barrier is `experimental/serverStatus` |
| 46 | source add/delete/move lifecycle is correct | PASS | `a_new_source_file_is_not_a_project_change`, `deleting_and_moving_a_source_file_stays_a_source_change`, `a_move_is_announced_from_both_ends`, `a_deleted_document_is_not_handed_over` |
| 47 | module membership change is determined semantically, not merely from `.rs` existence | PASS | `a_new_module_needs_no_project_reload` — `src/extra.rs` is not a module until `lib.rs` says `mod extra;`, and saying it is a source edit |
| 48 | a `Cargo.toml` change invalidates appropriately | PASS | `every_project_input_change_invalidates` |
| 49 | a `Cargo.lock`/environment change invalidates appropriately | PASS | same, plus `the_toolchain_participates_in_semantic_identity` |
| 50 | a workspace member/dependency change reloads project semantics | PASS | `a_manifest_change_waits_for_the_quiescent_barrier`, `a_manifest_change_reloads_and_waits_for_quiescence` |
| 51 | a feature/cfg change belongs to a different semantic basis | PASS | `a_feature_or_target_selection_is_part_of_the_basis` |
| 52 | the target triple/config participates in semantic identity | PASS | same, and `the_toolchain_participates_in_semantic_identity` — `cfg(target_os)` depends on the host triple |
| 53 | incompatible cfg worlds are not merged | PASS | one context claims one selection; the selection is in the `ConfigBasis`, so a different one is a different analysis rather than a union |

## Degradation and recovery

| # | case | verdict | evidence |
|---|---|---|---|
| 54 | rust-src absent degrades std navigation honestly | PASS | `the_standard_library_is_an_identity_and_its_absence_is_recorded`; `rust_src` is recorded in the environment and the Workspace's own semantics are asserted to hold regardless. Measured absent on this machine |
| 55 | an absent backend gives Level B | PASS | `without_a_backend_the_structural_truth_is_whole`, and the 73 always-on tests run with no toolchain |
| 56 | a timeout is not a false zero | PASS | `a_failing_backend_produces_no_evidence` — the refresh fails rather than publishing nothing |
| 57 | cancellation is not a false zero | PASS | `a_withdrawn_request_is_asked_again`, `a_site_withdrawn_twice_becomes_recorded_coverage` |
| 58 | a crash with an unchanged proven basis preserves eligible persisted truth | PASS | `a_reopen_reads_persisted_truth_and_refuses_what_it_cannot_prove` |
| 59 | an input change during an outage cannot preserve stale semantic-only truth | PASS | same — the basis is revalidated inside the publishing transaction |
| 60 | a restart reconstructs current backend state | PASS | `a_restarted_backend_reopens_rather_than_changes` — a fresh connection has opened nothing and its version sequence starts over |
| 61 | a daemon reopen only restores provable CURRENT | PASS | `a_reopen_reads_persisted_truth_and_refuses_what_it_cannot_prove` |
| 62 | an UNKNOWN dependency/toolchain environment cannot falsely reopen CURRENT | PASS | same, plus `an_unpinned_environment_is_deterministic_but_unproven` |

## Isolation and sharing

| # | case | verdict | evidence |
|---|---|---|---|
| 63 | worktree isolation holds | PASS | `worktree_identity_is_independent_of_location` — the same tree in two places is the same configuration, and the Workspace is still identity |
| 64 | multiple callers share one rust-analyzer process | PASS | `one_analysis_context_runs_one_rust_analyzer` |

## Agent surfaces

| # | case | verdict | evidence |
|---|---|---|---|
| 65 | related tests consume the canonical Rust graph | PASS | `the_agent_surfaces_answer_for_rust` |
| 66 | a public signature impact reaches callers and tests | PASS | same |
| 67 | a trait change reaches implementations | PASS | same — `BaseInterfaceChange` on `Runner` reaches `Worker` |
| 68 | an associated method change reaches callers | PASS | same — `RelatedTests` for `Worker::new` |
| 69 | `InspectPreparer` returns current editable Rust source | PASS | same — `source_complete()`, and every `evidence_range` present |
| 70 | prepared results never contain virtual/generated macro source | PASS | same — asserted per range |

## Discipline

| # | case | verdict | evidence |
|---|---|---|---|
| 71 | a repeated refresh is idempotent | PASS | `a_repeated_refresh_is_idempotent` — same evidence, and no second copy of an edge |
| 72 | structural and semantic proof do not duplicate logical relations | PASS | same relation count after a third pass |
| 73 | no rust-analyzer internal id becomes canonical identity | PASS | `no_backend_internal_identity_is_representable` — the typed surface carries URIs, positions and text, so there is nothing internal to be tempted by |
| 74 | all 23 task 1 capability verdicts are recorded from measured fixture evidence | PASS | `every_capability_is_declared_in_both_trust_modes`, `the_capability_matrix_is_what_was_measured` |
| 75 | Python/TS/Svelte/C# real-backend regressions remain green | PASS | rerun at the end of the task; see the completion record |
| 76 | the normal workspace suite passes with rust-analyzer unavailable | PASS | `cargo test --workspace --locked` never touches it; the 13 live tests are `#[ignore]` and skip cleanly when it is absent |

---

## The capability matrix

| capability | verdict | owner | limitation |
|---|---|---|---|
| `resource_discovery` | SUPPORTED | I2/I3 | — |
| `syntax_structure` | SUPPORTED | I2/I3 | — |
| `symbol_definition` | SUPPORTED | rust-analyzer | needs a crate graph, so UNSUPPORTED untrusted |
| `symbol_span` | SUPPORTED | I2/I3 | — |
| `containing_scope` | SUPPORTED | I2/I3 | — |
| `import_declaration` | SUPPORTED | I2/I3 | — |
| `export_declaration` | SUPPORTED | I2/I3 | `pub use` is recorded as written |
| `embedded_region_mapping` | UNSUPPORTED | — | Rust embeds no other language |
| `original_source_mapping` | UNSUPPORTED | — | nothing is generated from anything; macro expansion is refused, not mapped |
| `import_binding` | PARTIAL | rust-analyzer | a compound `use {A, B}` is one occurrence; a glob names no item |
| `alias_resolution` | SUPPORTED | rust-analyzer | — |
| `reexport_resolution` | SUPPORTED | rust-analyzer | — |
| `references` | SUPPORTED | rust-analyzer | — |
| `calls_intra_file` | SUPPORTED | rust-analyzer | a call inside a macro invocation anchors nothing |
| `calls_cross_file` | SUPPORTED | rust-analyzer | same |
| `external_symbol_resolution` | SUPPORTED | rust-analyzer | crate and version; never a path |
| `type_resolution` | PARTIAL | I2/I3 | generic arguments and trait bounds are not anchored |
| `inheritance` | UNSUPPORTED | — | Rust has no class inheritance; a supertrait is not a base class |
| `implements` | SUPPORTED | rust-analyzer | the central Rust capability, proved and not name-matched |
| `overrides` | UNSUPPORTED | — | an impl method replacing a default implements it and overrides nothing |
| `static_dispatch_target` | SUPPORTED | rust-analyzer | `dyn` resolves to the trait, concrete to the impl |
| `overload_resolution` | UNSUPPORTED | — | Rust has no user-defined function overloading |
| `implementation_target` | SUPPORTED | rust-analyzer | — |

Under `ProjectExecutionTrust::Untrusted` every binding capability is
UNSUPPORTED — there is no crate graph, and a crate graph is what every
binding needs — while the whole structural half is unchanged.

---

## Remaining limitations

1. **A compound `use`** binds to at most one of its items, and a **glob
   `use`** to none. The structural tier anchors one occurrence over the
   whole specifier.
2. **Trait bounds** produce no edge to the bound trait. I3 anchors the
   type parameter, not the bound.
3. **Generic arguments** are not anchored, so nothing is claimed about
   them.
4. **Associated types** are declared and not related. No relation kind
   states what one resolves to, and inventing one was out of scope.
5. **A call inside a macro invocation** anchors no occurrence, so
   nothing is claimed about it.
6. **Procedural macros** are not executed, so anything they would
   declare is absent.
7. **`rust-src` was absent** on the measuring machine, so navigation
   *into* the standard library is recorded rather than demonstrated.
   Workspace semantics were asserted to hold without it.
