# Task 14 acceptance ledger — shared runtime, multi-backend, concurrency

#19 task 14. Base `fa7aa4c`.

Every case the task listed has a verdict and the name of the test that
holds it. Where a verdict is not PASS the reason is written out. No case
was dropped for being awkward.

Two suites carry the whole ledger:

```text
crates/engine/src/runtime.rs           tests::*        55 tests, always on
crates/engine/tests/i4_fleet_acceptance.rs             15 always on + 3 real
```

The always-on half needs no toolchain at all. The three `#[ignore]`d
tests put the real Python, TypeScript, Svelte, C# and Rust launchers in
one supervisor, and skip with a printed reason when a backend is absent.

Nothing in either suite sleeps to make a race go away. Every wait is a
`Barrier`, a `Condvar`, a channel, an explicit event the fake announces,
or the supervisor's own quiescence signal. Where repetition adds value
it is *in addition to* the synchronization: the cold-start stampede runs
8 rounds, the Busy-versus-Ready scenario 16, the eviction/shutdown race
24, and concurrent owner publication 8.

---

## Shared startup and clients

| # | case | verdict | test |
|---|---|---|---|
| 1 | five concurrent cold acquisitions launch one host | PASS | `five_clients_racing_a_cold_context_produce_exactly_one_backend` — 8 rounds, all five released by one barrier; `starts_attempted == 1` |
| 2 | five clients share one context without client identity | PASS | `five_clients_come_and_go_without_ever_naming_themselves`, `nothing_about_the_caller_can_produce_a_second_runtime` |
| 3 | dropping one lease does not stop another client's runtime | PASS | `five_clients_come_and_go_without_ever_naming_themselves` — a client leaving is not a shutdown |
| 4 | a later client reuses the warm runtime | PASS | same — the latecomer's `launch_count` is still 1 |
| 5 | same backend family, different project root, distinct contexts | PASS | `one_backend_family_with_two_project_roots_is_two_runtimes` |

## Deduplication and cancellation

| # | case | verdict | test |
|---|---|---|---|
| 6 | five identical requests execute once | PASS | `five_identical_questions_are_asked_once_and_answered_five_times` |
| 7 | a different basis token does not dedupe | PASS | `a_question_about_a_newer_basis_never_joins_an_older_answer` — a parallel host, so both are provably inside the backend |
| 8 | one cancellation leaves the other waiters alive | PASS | `one_client_leaving_never_takes_another_clients_answer_with_it` |
| 9 | one timeout leaves the other waiters alive | PASS | same — a zero deadline, so the timeout is arithmetic rather than a race |
| 10 | shared work is cancelled only when the final waiter withdraws | PASS | `shared_work_stops_only_when_the_last_waiter_is_gone` — the fake counts requests that *saw* their cancel token set: 0 with one waiter left, 1 when all five have gone |
| 11 | dedupe telemetry matches the documented execution semantics | PASS | case 6 asserts `requests_started == 1` and `dedupe_hits == 4`; `fleet_telemetry_adds_up_what_the_runtimes_say_and_nothing_else` asserts the same at fleet scale |

## Scheduling

| # | case | verdict | test |
|---|---|---|---|
| 12 | interactive work jumps a queued background backlog | PASS | `interactive_work_is_not_trapped_behind_a_background_backlog` (task 2, unchanged) |
| 13 | another context has a separate queue | PASS | `one_slow_context_does_not_serialize_the_others` |
| 14 | a parallel host overlaps distinct requests | PASS | `a_parallel_host_really_overlaps_and_still_refuses_to_ask_twice` — one barrier for three requests, so serialized execution would never finish and passing *is* the proof |
| 15 | an identical request on a parallel host still dedupes | PASS | same — a fourth caller joins while two of three are provably still inside |

## Capacity and idle

| # | case | verdict | test |
|---|---|---|---|
| 16 | the configurable live cap is enforced | PASS | `capacity_retires_what_nobody_is_using_and_starts_what_is_wanted` |
| 17 | an idle, unleased runtime is an eviction candidate | PASS | same |
| 18 | a leased runtime is never capacity-evicted | PASS | `a_fleet_where_every_runtime_is_working_refuses_rather_than_kills` |
| 19 | a runtime with an active request is never capacity-evicted | PASS | same — protected by a request after its lease was dropped |
| 20 | all protected is an explicit capacity failure | PASS | same — `StartFailure::Capacity { limit: 2, live: 3 }`, and `starts_attempted` stays 0 |
| 21 | a completed acquisition never leaves the live count over the cap | PASS | `capacity_retires_…`, `the_capacity_cap_is_fleet_wide_and_privileges_no_language`; STARTING occupies its slot, so two cold acquisitions cannot each see room for one |
| 22 | an idle sweep shuts a host down exactly once | PASS | `an_idle_sweep_shuts_a_host_down_once_and_only_once` |
| 23 | idle unload preserves persistent semantic truth | PASS | `a_retired_runtime_leaves_everything_it_published_exactly_where_it_was` |
| 24 | a persisted current query does not wake an evicted backend | PASS | same — three reads, `launch_count` unchanged, runtime still STOPPED |

Eviction order is oldest `last_activity` first, context key as the tie
breaker, no randomness: `eviction_order_is_oldest_first_and_never_a_coin_toss`.
The cap is supervisor-wide across families:
`the_capacity_cap_is_fleet_wide_and_privileges_no_language`. The default
is `None`, unlimited, because the number is task 15's measurement and
not this task's to invent: `the_default_policy_caps_nothing_because_nobody_has_measured_yet`.

## Failure and recovery

| # | case | verdict | test |
|---|---|---|---|
| 25 | a crash in one context leaves another usable | PASS | `a_crash_in_one_context_leaves_another_ready_deterministically` |
| 26 | a shared crash with five waiters counts one backend crash | PASS | `one_crash_with_five_waiters_is_one_crash_and_five_failures` — blocked, then dead, so all five provably join one execution first |
| 27 | a crash never becomes an empty success | PASS | same — every waiter gets a typed failure, and an `Ok` fails the test |
| 28 | concurrent post-backoff acquisition starts one replacement | PASS | `five_clients_racing_a_restart_start_one_replacement` — zero backoff window, all five eligible at once, `restart_attempts == 1` |
| 29 | the restart budget degrades once per context, not per client | PASS | `a_degraded_context_stops_trying_and_leaves_the_rest_of_the_fleet_alone` — five further acquisitions, `launch_count` still 2 |
| 30 | a degraded context does not poison other contexts | PASS | same — the other family starts and answers |
| 31 | crash and eviction shut a host down once | PASS | cases 22 and 26 assert the per-host counter, not only the launcher-wide tally |
| 32 | the old Busy-versus-Ready scenario has deterministic coverage | PASS | `a_crash_in_one_context_leaves_another_ready_deterministically` — see the conclusion below |

**The task 11 flake, resolved.** It was not a runtime race. A request
used to publish its answer to its waiter and *then* close the entry's
books, so a test sampling the state the instant its own `wait` returned
could legitimately see BUSY while a sibling request was still unwinding.
The old assertion was therefore wrong about *when*, not about *what*.
`run_request` now closes the books first, which makes the state an
answer implies the state that is observable, and the reconstructed test
asserts READY with no settle window over 16 rounds. `settles_ready` is
still there for the one case with a genuine sibling in flight, and now
waits on the supervisor's quiescence signal rather than sleeping.

## Freshness and publication

| # | case | verdict | test |
|---|---|---|---|
| 33 | concurrent owners publish independently | PASS | `two_owners_publishing_at_once_never_answer_for_each_other` — 8 rounds, two connections, one barrier |
| 34 | an unrelated current owner stays readable while another refreshes | PASS | `an_unrelated_owner_stays_readable_while_another_refreshes` — structural relations stay readable too |
| 35 | an obsolete result cannot publish after a newer revision | PASS | `an_answer_about_an_older_revision_cannot_publish_over_a_newer_one` — refused as `Obsolete`, and the backend stays healthy |
| 36 | runtime eviction does not dirty semantic truth | PASS | `a_retired_runtime_leaves_everything_it_published_exactly_where_it_was` |
| 37 | a backend crash cannot make invalidated truth CURRENT | PASS | `a_crash_cannot_make_invalidated_truth_current_again` |
| 38 | proven CURRENT semantic truth is readable while the backend is cold | PASS | case 24 |

**Three real defects, found here and fixed.** All three are reachable
only when two owners of one `AnalysisContext` publish at once, which is
exactly what task 8 made ordinary and what nothing before task 14
exercised. `ensure_profile` read the profile key and then inserted it,
and both owners compute the same key; `begin_generation` read
`MAX(generation_no) + 1` and then inserted it; and `SemanticIndex::publish`
ran a deferred transaction that reads a basis and then writes what the
read authorized, which in WAL fails with `SQLITE_BUSY_SNAPSHOT` that the
busy timeout deliberately does not retry. They are now an upsert, one
statement, and an IMMEDIATE transaction respectively.

## Worktree isolation

| # | case | verdict | test |
|---|---|---|---|
| 39 | same repository, path and source in different WorkspaceIds is different contexts | PASS | `two_worktrees_never_answer_for_each_other`, `two_worktrees_are_two_runtimes_against_one_cap` |
| 40 | dedupe does not cross worktrees | PASS | `an_identical_question_in_two_worktrees_is_never_deduplicated` — same capability, target and basis; different key, different runtime entry |
| 41 | publication does not cross worktrees | PASS | `two_worktrees_never_answer_for_each_other` — a basis proved in one is refused by the other as `WorkspaceMismatch` |
| 42 | divergent source returns the worktree-local answer | PASS | same |
| 43 | a crash does not cross worktrees | PASS | `a_crash_in_one_worktree_is_invisible_in_the_other` |
| 44 | capacity counts each worktree runtime separately | PASS | `two_worktrees_are_two_runtimes_against_one_cap` |
| 45 | LogicalSymbol identity stays Workspace-bound | PASS | `a_logical_symbol_belongs_to_one_workspace` — identical project, name, kind and arity; different fingerprint |

## Trust

| # | case | verdict | test |
|---|---|---|---|
| 46 | C# Trusted/Untrusted contexts do not cross-reuse | PASS | `trusted_and_untrusted_csharp_and_rust_never_reuse_each_other` (real) — the two trusts produce different toolchain fingerprints, so different context keys |
| 47 | Rust Trusted/Untrusted contexts do not cross-reuse | PASS | same |
| 48 | one client's trust cannot promote another semantic world | PASS | `trusted_and_untrusted_worlds_are_different_contexts` — two runtimes, and a publication in one leaves the other not current |
| 49 | trust does not become supervisor-global | PASS | `trust_belongs_to_a_context_and_the_supervisor_has_no_opinion` — `RuntimePolicy` is the whole of a supervisor's configuration and has no trust field; the default is still `Untrusted` |

## Multi-backend

| # | case | verdict | test |
|---|---|---|---|
| 50 | one supervisor registers all five backend families | PASS | `one_supervisor_serves_every_installed_backend_family` (real) — measured: python, typescript, svelte, csharp, rust |
| 51 | React launches no backend | PASS | `react_registers_no_backend` — the closed vocabulary has no React kind, and `.tsx` is TypeScript |
| 52 | contexts reach READY independently | PASS | `a_context_that_is_still_starting_does_not_hold_up_another`; the real fleet's five all reach READY |
| 53 | a slow start in one context does not block another | PASS | same — TypeScript starts and answers while C# is held inside `launch` |
| 54 | one unavailable backend does not block the others | PASS | `one_missing_backend_family_does_not_make_the_fleet_unavailable`, `a_degraded_context_stops_trying_…` |
| 55 | duplicate acquisition of a real same-context runtime starts no duplicate host | PASS | `one_supervisor_serves_every_installed_backend_family` — every client takes a second lease on the first family; `starts_succeeded == 5` |
| 56 | all five backend families coexist when installed | PASS | same — `live_runtime_count == 5` |
| 57 | 3–5 real concurrent clients finish without deadlock | PASS | same — five threads released by one barrier |
| 58 | per-language canonical result stays correct under shared use | PARTIAL | The fleet suite proves the topology and does not re-derive any language's semantics. Tasks 9–13's own real-backend suites are the per-language proof and were rerun unchanged at the end of this task (34 tests, all green). Driving all five languages' full refresh through one shared supervisor would need a lease-shaped bridge for the C# and Rust *lifecycle* barriers, which the supervisor deliberately does not expose — adding one would be a new Agent-facing backend API, which case 80 forbids. |
| 59 | Svelte's context stays distinct from TS/JS | PASS | `one_supervisor_serves_every_installed_backend_family` — every installed family's context key is unique, Svelte's own TypeScript included |
| 60 | C#/Rust trust configuration stays correct in a mixed fleet | PASS | the real fleet runs both `Untrusted` and they start and stay distinct; cases 46–47 prove the two trusts never share a world |

## Telemetry

| # | case | verdict | test |
|---|---|---|---|
| 61 | the fleet live count is exact | PASS | `fleet_telemetry_adds_up_what_the_runtimes_say_and_nothing_else`, `five_clients_over_five_families_share_one_supervisor` |
| 62 | state counts are exact | PASS | same; `a_degraded_context_stops_trying_…` asserts `degraded`, `ready` and `live_runtimes` together |
| 63 | aggregate active requests, leases and queues are exact | PASS | `fleet_telemetry_adds_up_…` — three waiters, one active request |
| 64 | aggregate starts, restarts, crashes and dedupe are exact | PASS | same, and `a_crash_in_one_worktree_is_invisible_in_the_other` (one crash in the fleet, not two) |
| 65 | unknown resource use stays explicitly unknown | PASS | `a_fleet_that_cannot_measure_its_memory_says_so_instead_of_saying_zero` |
| 66 | known RSS is summed only where known | PASS | `two_measured_hosts_are_a_sum_and_two_unmeasured_ones_are_not_a_zero` — 100 + 200 is 300; 100 + unknown is 100 and one unknown; nothing measured is `None` |
| 67 | reading telemetry does not start a backend | PASS | `reading_telemetry_never_starts_a_backend` |

`ResourceUsage` was left exactly as task 2 built it: two `Option`s,
because neither is portably observable from safe Rust in a workspace
that denies `unsafe_code`. No `ps`, no platform inspection, no zero
standing in for a measurement nobody took. On the measuring machine all
five real hosts reported unknown, and the fleet says so:
`known_rss=None unmeasured=5`.

## Shutdown

| # | case | verdict | test |
|---|---|---|---|
| 68 | shutdown rejects new acquisitions | PASS | `shutdown_releases_a_mixed_fleet_exactly_once_and_takes_no_more_work` |
| 69 | every live host shuts down once | PASS | same — the per-host counter, and shutting down twice does not shut a host down twice |
| 70 | queued serial work does not deadlock shutdown | PASS | same — an in-flight request is released rather than hung |
| 71 | capacity eviction racing shutdown does not double-shutdown | PASS | `capacity_eviction_racing_shutdown_never_shuts_one_host_down_twice` — 24 rounds, both threads reaching for the same live host through one barrier |
| 72 | the persistent semantic index survives shutdown | PASS | `shutting_the_fleet_down_leaves_the_semantic_index_where_it_was` — still CURRENT, and the file's length is unchanged |

A runtime started while shutdown was in flight is now released rather
than left READY behind a stopped supervisor: `acquire` re-checks
`shutting_down` after the launch returns.

## Regression

| # | case | verdict | evidence |
|---|---|---|---|
| 73 | the normal suite passes with no external semantic backends | PASS | `cargo test --workspace --locked` — 1168 passed, 0 failed, 50 ignored; 6 consecutive clean runs |
| 74 | Python real-backend regression | PASS | `i4_python_acceptance` 2, `python_semantic_pyright` 2 |
| 75 | TypeScript transport and semantic regression | PASS | `i4_typescript_acceptance` 3, `typescript_semantic_lsp` 11 |
| 76 | Svelte real-backend regression | PASS | `i4_svelte_acceptance` 3 |
| 77 | C# real-backend regression | PASS | `i4_csharp_acceptance` 13 |
| 78 | Rust real-backend regression | PASS | `i4_rust_acceptance` 13 |
| 79 | I3 structural acceptance remains green | PASS | `i3_acceptance`, in the workspace suite |
| 80 | no new Agent-facing backend-specific API | PASS | The whole public addition is `RuntimePolicy::max_live_runtimes`, `StartFailure::Capacity`, `FleetTelemetry`, `SemanticRuntimeSupervisor::fleet_telemetry` and `::await_quiescent`. Every one is backend-neutral: none names a language, a protocol or a host, and no host handle crosses the supervisor boundary. |

---

## Remaining limitations

1. **Case 58 is PARTIAL.** The shared-fleet suite proves topology, not
   language semantics. Each language's canonical results are proved by
   its own task 9–13 suite, rerun unchanged here. Driving C# and Rust
   *refreshes* through a shared lease would require exposing their
   lifecycle barriers — a project-load barrier and a document-version
   exchange — which live on the concrete host by design. That bridge is
   a design decision, not a test fixture, and it is not task 14's.

2. **Resource observation is unavailable on this platform.** Every real
   host reported `rss_bytes: None`. Fleet telemetry carries that
   faithfully rather than summing to zero, and no policy in this task
   reads RSS. Task 15 owns the external measurement.

3. **`max_live_runtimes` defaults to `None`.** The mechanism is proved;
   the number is not chosen. Picking one before task 15's measurements
   would be inventing a product decision.

4. **The capacity search is a single pass.** It snapshots the fleet,
   sorts the candidates and re-verifies each under its own lock. A
   client that takes a lease on the chosen runtime between the snapshot
   and the eviction wins, and if that leaves the fleet still full the
   caller gets an honest capacity refusal rather than a retry loop. That
   is deliberate: a retry loop under contention is a livelock with
   better manners.
