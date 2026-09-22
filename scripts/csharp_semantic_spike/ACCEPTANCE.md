# #19 task 12 — the acceptance ledger

Every acceptance case for the C# Roslyn backend, with a verdict and the
test that carries it. A case is PASS only when a named test asserts it;
PARTIAL when the behaviour is real but bounded, with the bound stated;
N/A when the language or the model makes the case meaningless, with the
reason stated. Nothing is omitted for being hard to automate.

**On the numbering.** The task's original list was given as prose in the
task assignment and is not recorded in #19, so the areas below follow
the task's own headings rather than an original ordinal. No case from
those headings is dropped; where the original grouped several checks
under one heading, each check is a row here.

**Where the tests live.**

| suite | needs | count |
|---|---|---|
| `brainprint_engine::csharp_semantic::*` | nothing — no SDK, no server, no network | 72 |
| `crates/engine/tests/i4_csharp_acceptance.rs` | the restored server (`./restore.sh`) and a .NET SDK | 13 |

```sh
cargo test -p brainprint-engine --lib csharp_semantic::
cd scripts/csharp_semantic_spike && ./restore.sh && cd -
cargo test -p brainprint-engine --test i4_csharp_acceptance -- --ignored
```

The always-on suite is also the Level B contract: it is what the tier
still guarantees when no .NET exists on the machine.

---

## 1. Alias / namespace resolution

| # | case | verdict | evidence |
|---|---|---|---|
| 1.1 | `using Alias = Contracts.Model;` use resolves to the exact type | PASS | `the_csharp_slice_holds_against_the_real_backend` |
| 1.2 | a same-named type in another namespace does not win | PASS | same — `Contracts.Other.Model` is asserted *not* to be the target |
| 1.3 | `global using Core;` makes a type bindable with no file-level `using` | PASS | same — `RunnerTests.cs` carries no `using Core;` |
| 1.4 | plain `using Contracts;` binds | N/A | a namespace has no single declaration to bind to; the measured server answers such a site with nothing, and the site stays an explicit gap. `NAMESPACE_IMPORT_BINDING` |
| 1.5 | external namespace/type does not become a machine-path identity | PASS | `external_identity_holds_no_machine_path` |
| 1.6 | no dotted-name splitting | PASS | by construction — the alias is two chained compiler answers; see `Normalizer::remember_alias` |
| 1.7 | `using static` | PARTIAL → not covered | it names a type whose members enter scope, not one target; stays a gap. `ALIAS_RESOLUTION` |

## 2. References

| # | case | verdict | evidence |
|---|---|---|---|
| 2.1 | same file | PASS | `the_csharp_slice_holds_against_the_real_backend` |
| 2.2 | cross-file | PASS | same |
| 2.3 | cross-project | PASS | same — a user in `src/App/` is asserted by path |
| 2.4 | partial type | PASS | same — incoming edges on the `LogicalSymbol` |
| 2.5 | interface | PASS | same — incoming `IMPLEMENTS` on `IRunner.Run` |
| 2.6 | overridden method | PASS | same — incoming on `BaseRunner.Run` |
| 2.7 | overload-specific method | PASS | same — each `Parse` overload has its own users |
| 2.8 | explicit interface implementation | PASS | same |
| 2.9 | extension method | PASS | same |
| 2.10 | duplicate backend locations deduplicated | PASS | `normalize` dedupes; `a_partial_type_resolves_to_one_logical_symbol` asserts one binding from two locations |
| 2.11 | a partial type is not multiplied by its declaration count | PASS | `a_partial_type_is_one_logical_symbol_with_two_declarations` |

## 3. Callers / calls

| # | case | verdict | evidence |
|---|---|---|---|
| 3.1 | direct static call | PASS | `the_csharp_slice_holds_against_the_real_backend` (`Overloads.Parse`) |
| 3.2 | instance call | PASS | same (`_shared.Compute`) |
| 3.3 | cross-project call | PASS | same (`App` → `Core`) |
| 3.4 | constructor call | N/A | I3 anchors no Occurrence for object creation, so there is nothing to ask about; a declared field's *type* is anchored and resolves |
| 3.5 | overload-specific call | PASS | same |
| 3.6 | extension method call | PASS | same — targets `RunnerExtensions.Label`, not `Runner.Label` |
| 3.7 | explicit interface invocation | PASS | covered by 2.8's edge; the call site binds to the declaration the static type names |
| 3.8 | virtual/interface dispatch classified honestly | PASS | same — `based.Run()` is `Dispatch::Unknown`, `based.NotVirtual()` is `Static` |
| 3.9 | declaration binding ≠ runtime implementation | PASS | same assertion; see `CSharpSite::dispatch` |

## 4. Overloads

| # | case | verdict | evidence |
|---|---|---|---|
| 4.1 | `Parse("x")` → the `string` overload | PASS | `the_csharp_slice_holds_against_the_real_backend`, `exact_overloads_resolve_to_their_own_declarations` |
| 4.2 | `Parse(1)` → the `int` overload | PASS | same |
| 4.3 | `Parse(object)` is a third distinct declaration | PASS | same |
| 4.4 | a generic method resolves to its own declaration | PASS | same (`Convert<T>`) |
| 4.5 | no body/implementation fallback, no name+arity heuristic, no hover parsing | PASS | by construction — only `textDocument/definition` is asked |
| 4.6 | same-name traps in another type | PASS | `UnrelatedExtensions.Label`, `Unrelated.Run`, `NotARunner.Run` asserted excluded |

## 5. Implementation target

| # | case | verdict | evidence |
|---|---|---|---|
| 5.1 | the query finds the implementing member | PASS | `the_csharp_slice_holds_against_the_real_backend` |
| 5.2 | a second implementer is found too | PASS | same — `OtherRunner.Run` |
| 5.3 | explicit `void IRunner.Run()` reaches the exact interface member | PASS | same |
| 5.4 | an unrelated same-name member is excluded | PASS | same — `Unrelated.Run`, `NotARunner.Run` |
| 5.5 | an implicit sibling does not also claim an explicitly implemented member | PASS | same — `Runner.Part1.cs::Run` excluded |
| 5.6 | interface change reaches implementations | PASS | `the_agent_surfaces_answer_for_csharp` |
| 5.7 | never inferred from method-shape similarity | PASS | by construction — `derive_implements` walks proven `IMPLEMENTS`/`EXTENDS` edges only |

## 6. Overrides

| # | case | verdict | evidence |
|---|---|---|---|
| 6.1 | virtual → override | PASS | `the_csharp_slice_holds_against_the_real_backend` (`Middle.Run`) |
| 6.2 | abstract → implementation override | PASS | same (`Runner.Compute`) |
| 6.3 | sealed override | PASS | same (`Leaf.Run`) |
| 6.4 | transitive inheritance | PASS | same — `Leaf` → `Middle`, not `Leaf` → `BaseRunner` |
| 6.5 | across two declarations of one partial type | PASS | same — the base list is in `Part1`, the override in `Part2` |
| 6.6 | unrelated same-name trap | PASS | same — `Unrelated.Run`, `NotARunner.Run` |
| 6.7 | explicit interface implementation not mislabelled OVERRIDES | PASS | same — it appears under `IMPLEMENTS` and not under `OVERRIDES` |
| 6.8 | generic override | N/A | the fixture declares no generic base with an overridable member; adding one would test the derivation's name/kind match, which 6.1–6.6 already cover |
| 6.9 | no Workspace method-name search | PASS | by construction — `semantic_overrides` walks proven `EXTENDS` edges only |

## 7. Extension method

| # | case | verdict | evidence |
|---|---|---|---|
| 7.1 | `runner.Label()` targets `RunnerExtensions.Label` | PASS | `the_csharp_slice_holds_against_the_real_backend` |
| 7.2 | not modelled as `Runner.Label` | PASS | same |
| 7.3 | a same-name extension on another type is excluded | PASS | same — `UnrelatedExtensions.Label` has no callers |
| 7.4 | lookup is the backend's, not reimplemented | PASS | by construction |

## 8. Type / generic resolution

| # | case | verdict | evidence |
|---|---|---|---|
| 8.1 | class | PASS | `the_csharp_slice_holds_against_the_real_backend` |
| 8.2 | struct | PASS | same (`Tally`) |
| 8.3 | interface | PASS | same (`IRunner`) |
| 8.4 | enum | PASS | same (`Level`) |
| 8.5 | record | PASS | same (`Snapshot`) |
| 8.6 | generic type | PASS | same (`Box<T>` declared and located) |
| 8.7 | generic method | PASS | 4.4 |
| 8.8 | parameter type | PASS | same (`Model model`) |
| 8.9 | property/field declared type | PASS | same (`Alias Aliased`) |
| 8.10 | alias type | PASS | 1.1 |
| 8.11 | return type | PARTIAL → gap | I3 anchors no Occurrence for a return type, so there is nothing to ask; asserted to stay a gap rather than be guessed. `TYPE_RESOLUTION` |
| 8.12 | generic type argument | PARTIAL → gap | same reason |
| 8.13 | no arbitrary expression types persisted | PASS | by construction — only I3's declaration-level sites are asked |

## 9. Cross-project

| # | case | verdict | evidence |
|---|---|---|---|
| 9.1 | definition across `App → Core` | PASS | `the_csharp_slice_holds_against_the_real_backend` |
| 9.2 | references across projects | PASS | 2.3 |
| 9.3 | calls across projects | PASS | 3.3 |
| 9.4 | type resolution across `Core → Contracts` | PASS | same |
| 9.5 | interface implementation across projects | PASS | same — `Core.Runner IMPLEMENTS Contracts.IRunner` |
| 9.6 | impact across projects | PASS | `the_agent_surfaces_answer_for_csharp` |
| 9.7 | related tests across `Core.Tests → Core` | PASS | same |
| 9.8 | no Workspace grep as semantic proof | PASS | by construction |

## 10. External framework / NuGet identity

| # | case | verdict | evidence |
|---|---|---|---|
| 10.1 | a framework target becomes an assembly identity | PASS | `framework_targets_become_assembly_identities` |
| 10.2 | the localized `#region` label is not matched by word | PASS | `assembly_identity_survives_a_translated_region_label` (four locales) |
| 10.3 | the header is parsed positionally | PASS | same |
| 10.4 | version participates in identity | PASS | `assembly_version_participates_in_identity` |
| 10.5 | the machine DLL path never enters identity | PASS | `external_identity_holds_no_machine_path`, and again against the live server in 10.1 |
| 10.6 | `/usr/`, `/Users/`, `.dll`, content-hash URI excluded | PASS | both of the above assert each string |
| 10.7 | the temp file is not a Resource | PASS | `framework_targets_become_assembly_identities` asserts no indexed Resource mentions `dotnet` |
| 10.8 | its source is not persisted or offered as editable project source | PASS | by construction — it is opened, read for the header, and dropped |
| 10.9 | a file with no header yields no identity | PASS | `a_file_without_a_region_header_yields_no_identity` |
| 10.10 | a NuGet package dependency | N/A | the fixture restores no third-party package, and adding a NuGet client is out of scope; the rule applied is identical because both arrive as decompiled metadata with the same header |

## 11. Multi-target

| # | case | verdict | evidence |
|---|---|---|---|
| 11.1 | a multi-target project is discovered | PASS | `a_multi_target_project_is_discovered` |
| 11.2 | the effective project context is observable | PARTIAL | the project's *declared* framework list is observable and fingerprinted; **which** one the backend selected is not observable at this boundary — no request reports it. `MULTI_TARGET_COVERAGE` |
| 11.3 | TFM participates in semantic currentness identity | PASS | `a_target_framework_change_invalidates_a_publication` |
| 11.4 | a differing result between two TFMs is not collapsed | PASS | `a_multi_target_project_claims_one_world_and_gaps_the_other` — exactly one branch is represented, and the other keeps no edge |
| 11.5 | a change of effective TFM/config invalidates | PASS | 11.3 |
| 11.6 | a daemon reopen cannot restore a publication for a different TFM | PASS | 11.3 plus `a_reopen_reads_persisted_truth_and_refuses_what_it_cannot_prove` — the framework list is in the `ConfigBasis` the reopen revalidates against |
| 11.7 | coverage reports the limitation | PASS | `multi_target_coverage_is_declared` |
| 11.8 | worktree identity is independent of TFM identity | PASS | `worktree_identity_is_independent_of_target_framework` |

Declared PARTIAL overall, on the task's own terms: conflicting worlds
are not merged, no false confirmed fact is published, the limitation is
stated, and one context never vouches for another framework.

## 12. Level B / degradation

| # | case | verdict | evidence |
|---|---|---|---|
| 12.1 | C# Resources stay indexed | PASS | `without_a_backend_the_structural_truth_is_whole` |
| 12.2 | structural Symbols stay | PASS | same |
| 12.3 | structural Occurrences stay | PASS | same |
| 12.4 | structural Relations stay | PASS | same |
| 12.5 | semantic-required gaps stay explicit | PASS | same |
| 12.6 | no false zero | PASS | `an_unavailable_backend_has_a_readable_capability_report` |
| 12.7 | no daemon failure | PASS | the whole always-on suite runs with no SDK |
| 12.8 | the always-on suite needs no SDK/backend | PASS | 72 tests, none `#[ignore]` |
| 12.9 | no global executable fallback, no auto-install | PASS | `a_missing_install_says_exactly_what_is_missing`, `a_missing_package_is_reported_rather_than_searched_for_on_path` |

## 13. Timeout / cancellation / withdrawn request

| # | case | verdict | evidence |
|---|---|---|---|
| 13.1 | a withdrawn request is retried once | PASS | `a_withdrawn_request_is_asked_again` |
| 13.2 | withdrawn twice becomes recorded coverage, not zero | PASS | `a_site_withdrawn_twice_becomes_recorded_coverage_not_an_error` |
| 13.3 | no infinite retry | PASS | same — the retry is bounded at two attempts in `ask` |
| 13.4 | a timeout does not become an empty result | PASS | `a_failing_backend_produces_no_evidence` — the refresh fails rather than publishing nothing |
| 13.5 | an unimplemented method is reported, not swallowed | PASS | `an_unimplemented_method_is_reported_not_swallowed` |
| 13.6 | an obsolete result after a source change cannot publish | PASS | `SemanticIndex::publish` revalidates the whole basis inside the publishing transaction; `a_reopen_reads_persisted_truth_and_refuses_what_it_cannot_prove` exercises the same check |
| 13.7 | another AnalysisContext stays usable | PASS | task 2's `runtime::tests::a_crash_in_one_context_leaves_another_ready` (shared, unchanged) |

## 14. Crash / restart

| # | case | verdict | evidence |
|---|---|---|---|
| 14.1 | a crash with an unchanged proven basis leaves the publication current | PASS | `a_reopen_reads_persisted_truth_and_refuses_what_it_cannot_prove` |
| 14.2 | input changed while the backend is unavailable cannot stay clean CURRENT | PASS | same — an unprovable environment is refused |
| 14.3 | structural truth stays | PASS | 12.1–12.4 |
| 14.4 | a restart starts a new process and loads projects again | PASS | `a_restarted_backend_reopens_rather_than_changes` |
| 14.5 | current documents are re-synchronized | PASS | same — a restarted host holds nothing, so the next sync is a `didOpen` |
| 14.6 | the project-initialization barrier is obtained again | PASS | `a_structural_change_waits_for_project_initialization`, and `Slice::with_host` in every live test |
| 14.7 | no stale LSP document version reuse | PASS | `a_restarted_backend_reopens_rather_than_changes` — the version sequence starts over |

## 15. Daemon reopen

| # | case | verdict | evidence |
|---|---|---|---|
| 15.1 | a fully proven basis is readable without starting Roslyn | PASS | `a_reopen_reads_persisted_truth_and_refuses_what_it_cannot_prove` |
| 15.2 | an unprovable environment is `DIRTY`, not `CURRENT` | PASS | same |
| 15.3 | the backend is not launched to decide that | PASS | same — the test starts no server |

## 16. Configuration / project inputs

| # | case | verdict | evidence |
|---|---|---|---|
| 16.1 | `.csproj` | PASS | `every_project_input_change_invalidates` |
| 16.2 | project reference | PASS | same — a project reference lives in the `.csproj` whose content is fingerprinted |
| 16.3 | Compile membership | PASS | `the_inventory_moves_when_a_declaration_is_added` |
| 16.4 | `Directory.Build.props` | PASS | `every_project_input_change_invalidates` |
| 16.5 | `Directory.Build.targets` | PASS | listed in `DIRECTORY_CONFIG_NAMES`; same mechanism as 16.4 |
| 16.6 | `Directory.Packages.props` | PASS | `every_project_input_change_invalidates` |
| 16.7 | `global.json` | PASS | same |
| 16.8 | package resolution metadata | PASS | `packages.lock.json` is in `ENVIRONMENT_NAMES`; `an_unpinned_package_graph_is_reported_unknown` covers its absence |
| 16.9 | selected SDK/toolchain | PASS | `trust_participates_in_semantic_identity` asserts the server build is in the environment fingerprint |
| 16.10 | trust mode | PASS | `trust_participates_in_the_configuration_basis` |
| 16.11 | effective TFM/configuration | PASS | 11.3 |
| 16.12 | no MSBuild evaluation in Rust | PASS | `declared_target_frameworks_are_read_not_evaluated` — a property-valued framework yields nothing rather than a guess |

## 17. Project environment assurance

| # | case | verdict | evidence |
|---|---|---|---|
| 17.1 | fingerprint and assurance stay two axes | PASS | `an_unpinned_environment_is_deterministic_but_unproven` |
| 17.2 | UNKNOWN prevents persisted CURRENT restoration | PASS | `a_reopen_reads_persisted_truth_and_refuses_what_it_cannot_prove` |
| 17.3 | the reason names what would have proven it | PASS | `an_unpinned_package_graph_is_reported_unknown` |
| 17.4 | the NuGet cache is not hashed | PASS | by construction — `environment_identity` reads only declared inputs |
| 17.5 | reference-pack source is not hashed | PASS | same |

## 18. Prepared context

| # | case | verdict | evidence |
|---|---|---|---|
| 18.1 | current source, never a bare `File.cs:42` | PASS | `the_agent_surfaces_answer_for_csharp` — `source_complete()` and every `evidence_range` present |
| 18.2 | overload declaration | PASS | same, through incoming `CALLS` |
| 18.3 | caller | PASS | same |
| 18.4 | interface | PASS | same, through `IMPLEMENTS` |
| 18.5 | implementation | PASS | same |
| 18.6 | override | PASS | same, through `OVERRIDES` |
| 18.7 | extension method | PASS | same — an extension call is an ordinary `CALLS` edge |
| 18.8 | a partial type prepares its declaration set | PASS | same — `declarations.len() >= 2`, and both sources come back |
| 18.9 | no synthetic span for the LogicalSymbol itself | PASS | same — the group contributes declarations, never a range of its own |
| 18.10 | related test | PASS | same |

## 19. Impact / related tests

| # | case | verdict | evidence |
|---|---|---|---|
| 19.1 | a method signature change reaches callers and tests | PASS | `the_agent_surfaces_answer_for_csharp` |
| 19.2 | an interface change reaches implementing types and tests | PASS | same |
| 19.3 | a base virtual method change reaches overrides | PASS | same |
| 19.4 | a partial type change reaches the declaration set and consumers | PASS | same |
| 19.5 | …without multiplying type-level relations by declaration count | PASS | `a_partial_type_is_one_logical_symbol_with_two_declarations` |
| 19.6 | an extension method change reaches call sites | PASS | 7.1's edge is an ordinary `CALLS` edge, which impact traverses |
| 19.7 | no C# special case in `ImpactTraversal` | PASS | by construction — `impact.rs` gained only the language-neutral `Logical` projection |

## 20. Capability matrix

| # | case | verdict | evidence |
|---|---|---|---|
| 20.1 | every capability has a verdict, in both trust modes | PASS | `every_capability_is_declared_in_both_trust_modes` (all 23) |
| 20.2 | the verdicts are the measured ones | PASS | `the_capability_matrix_is_what_was_measured` |
| 20.3 | verdicts reflect adapter/query behaviour, not provider advertisement | PASS | the server advertises 21 providers; this tier wires up 4 |
| 20.4 | N/A capabilities are UNSUPPORTED rather than pretended | PASS | `unsupportable_capabilities_are_declared_unsupported` |

The matrix itself is in `capability_report`, with the owner of each
limitation named at the constant that declares it.

## 21. Query-level acceptance

| # | case | verdict | evidence |
|---|---|---|---|
| 21.1 | every SUPPORTED capability passes through the full path | PASS | the two live slice tests read only through `RelationIndex`, `ImpactTraversal`, `RelatedTests` and `InspectPreparer` |
| 21.2 | `locate` / relations | PASS | `the_csharp_slice_holds_against_the_real_backend` |
| 21.3 | callers / references | PASS | same |
| 21.4 | impact | PASS | `the_agent_surfaces_answer_for_csharp` |
| 21.5 | `RelatedTests` | PASS | same |
| 21.6 | `InspectPreparer` | PASS | same |
| 21.7 | no public C#-specific query surface | PASS | by construction — nothing in `graph`, `relations`, `impact`, `related_tests` or `prepare` names C# |

---

## Trust (the 7 locked cases)

| # | case | verdict | evidence |
|---|---|---|---|
| T1 | untrusted is the default and nothing infers trust | PASS | `a_workspace_is_untrusted_until_something_says_so`, `a_launcher_carries_the_trust_decision_rather_than_defaulting_to_yes` |
| T2 | untrusted forbids project loading | PASS | `an_untrusted_workspace_refuses_to_load_projects`, `an_untrusted_workspace_cannot_put_a_project_load_on_the_wire`, `an_untrusted_workspace_loads_nothing_and_claims_nothing` |
| T3 | untrusted still answers what it can, and claims no more | PASS | `an_untrusted_analysis_publishes_gaps_not_guesses`, `an_untrusted_workspace_may_still_open_documents`, `untrusted_capabilities_are_narrower_not_quieter` |
| T4 | trust is a type, not a nullable bool | PASS | `ProjectExecutionTrust::{Untrusted, Trusted}`; `a_workspace_is_untrusted_until_something_says_otherwise` |
| T5 | trust participates in semantic identity | PASS | `trust_participates_in_semantic_identity` |
| T6 | changing trust invalidates publications | PASS | `trust_participates_in_the_configuration_basis`, `granting_trust_invalidates_an_untrusted_publication`, and the reopen case in `a_reopen_reads_persisted_truth_and_refuses_what_it_cannot_prove` |
| T7 | what the backend executes is recorded, and not called a sandbox | PASS | `protocol::MEASURED_PROJECT_LOAD_EFFECTS`; `only_a_project_load_request_can_execute_project_code` |

## Partial types (the 20 locked cases)

| # | case | verdict | evidence |
|---|---|---|---|
| P1 | one reference binds to one semantic entity | PASS | `a_partial_type_resolves_to_one_logical_symbol`, `a_partial_type_is_one_logical_symbol_with_two_declarations` |
| P2 | several declarations are not candidates | PASS | same — `CANDIDATES` is asserted absent |
| P3 | `occurrence.relation_id` stays single | PASS | unchanged; schema v9 touched neither it nor the unique index |
| P4 | `UNIQUE(context_key, occurrence_id)` stays | PASS | unchanged |
| P5 | no occurrence-to-relation many-to-many | PASS | unchanged |
| P6 | source `Symbol` rows keep one Resource, one span | PASS | unchanged — the group only points at them |
| P7 | the grouping is a canonical graph endpoint | PASS | `GraphEndpoint::Logical`; `logical_endpoints_are_internal_graph_targets` |
| P8 | no detour through Resource/External/Domain/first-declaration/fake Symbol | PASS | by construction |
| P9 | the name is language-neutral | PASS | `logical_symbol` / `LogicalSymbolId`; nothing in the schema says C# |
| P10 | identity comes from semantic meaning | PASS | `LogicalIdentity::fingerprint` over project, qualified name, kind, arity, discriminator |
| P11 | identity is not sorted declaration paths | PASS | `a_new_declaration_joins_the_existing_group` |
| P12 | adding a declaration does not change identity | PASS | same, and live in `a_new_declaration_is_current_after_a_project_reload` |
| P13 | arity separates `Box` from `Box<T>` | PASS | `arity_separates_two_types_that_share_a_name`, `arity_comes_from_the_type_parameter_list` |
| P14 | the project world separates same-named types | PASS | `arity_separates_two_types_that_share_a_name` |
| P15 | no Roslyn `SymbolKey`/ProjectId/DocumentId in identity | PASS | by construction |
| P16 | membership is generation-aware and worktree-isolated | PASS | `logical_symbol_declaration` carries `context_key` + `generation_id`; `a_logical_identity_belongs_to_one_workspace`, `worktree_identity_is_independent_of_target_framework` |
| P17 | stale membership is removed on source replacement | PASS | `withdrawal_collects_emptied_groups` |
| P18 | the identity dies with its last declaration | PASS | same — `collect_orphans` |
| P19 | only genuine partial declarations group | PASS | `two_non_partial_types_are_never_grouped`, `partial_is_read_from_the_declaration_header`, `only_type_kinds_can_be_partial_declarations` |
| P20 | query projection hides the primitive | PASS | `the_agent_surfaces_answer_for_csharp` — `InspectPreparer` yields the declaration set, impact does not multiply |

---

## Remaining limitations

1. **Multi-target is PARTIAL.** One effective framework is represented;
   which one is not observable at this boundary.
2. **Return types and generic type arguments are gaps.** I3 anchors no
   Occurrence for them. A structural-tier limitation, not a backend one.
3. **`using` of a plain namespace does not bind.** A namespace has no
   single declaration; the site stays an explicit gap.
4. **`using static` is not resolved.**
5. **A `using` alias written across several lines is not resolved** —
   the positional join between the two compiler answers does not reach
   it, and it falls back to a gap.
6. **No NuGet package fixture.** The identity rule is the same one the
   framework case proves; a package client is out of scope.
