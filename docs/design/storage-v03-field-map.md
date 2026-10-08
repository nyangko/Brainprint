# Brainprint 0.3.0 — Physical field disposition and durable normalization plan

> **DESIGN / REVIEW REQUIRED — not approved migration SQL, not executable ETL.**
> Source: user-provided `brainprint-schema-audit.json` from git `231843185d4c407dc73cfd8acceb272add97835b`, SQLite 3.54.0.
> Owner: issue [#94](https://github.com/nyangko/Brainprint/issues/94). Companion SQL reference: `storage-v03-schema.sql`.

## Non-negotiable interpretation

- This is a **one-row-per-original-column** disposition manifest. `K` means the column remains in the original typed table in phase 1; `M` means a proposed destination in the new durable model; `B` means keep the typed table and add a stable UID/provenance bridge after parity tests. **No row or column deletion is authorized.**
- Mapping `id INTEGER` to a new row PK is unsafe across databases; store original typed composite key in `migration_id_map.source_key_json` and map legacy local references in two passes. For `uid BLOB`, preserve all 16 bytes without regeneration unless legacy UID is null/malformed; unexpected values → refuse/quarantine, never silently zero.
- Import existing `status` **as a legacy snapshot** and insert a `LEGACY_IMPORT` event at migration time. Never fabricate a historical `APPROVE` event or rewrite a legacy `updated_at` to the import timestamp.
- Every transferred TEXT/JSON string retains the **exact original column value**, including whitespace, nullable state and raw structured JSON text, under a typed field in `knowledge_revision.payload_json`. Typed validation/projections are performed in a separate acceptance step; semantic equivalence is not assumed.
- `source_kind` + `source_locator` + `source_revision` map to provenance/evidence lookup only when the source is actually available and verifiable. Original values must remain in the import payload; unverified source must not be minted as a present `source_revision`.
- Policy/Decision/Blueprint typed resolution and protection remain obligations from current Rust `knowledge/{mod,project,global,resolve}.rs`; no **two independently writable canonical stores** after cutover. `knowledge_item/knowledge_revision` does not by itself guarantee resolver parity.
- **Historical results policy** (`09_STORAGE_DATA_POLICY.md`) keeps extracted analysis and relations; `index.db` does not automatically become disposable. FTS/vector are optional rebuildable search accelerators, unlike retained analyzed observations.
- SQLite per-DB foreign keys cannot link Global↔Project↔Workspace/Index UIDs. Any cross-owner link uses a typed stable UID plus explicit application-level resolution/repair; do not pretend to enforce cross-file FKs.

## Planned destination contracts

| Target family | Migration payload / invariant |
|---|---|
| `knowledge_item` | One stable UID per original Policy/Decision/Blueprint/Preference. Import key `legacy:{db}:{table}:{uidHex}` is collision-safe; **do not use nullable `policy_key` or titles as the unique key**. Owner/scope remains explicit. |
| `knowledge_revision` | One immutable imported snapshot per row, with exact original typed fields inside payload JSON; future changes append revisions. Never infer invisible historical revisions. |
| `knowledge_event` + `knowledge_head` | `LEGACY_IMPORT` event and reconstructed current state; new approvals/revocations CAS by head version, append-only. Authorship/provenance must not be fabricated. |
| `knowledge_scope` + `knowledge_scope_binding` | Preserve `scope_kind/scope_key`. `NULL` scope values normalized into a typed canonical scope key only with an explicit reversible mapping; store original null. |
| `knowledge_evidence` / `source_ref` / `source_revision` | Provenance linkage via verified stable source UID/revision only; otherwise retain original locator as unresolved evidence. |
| `knowledge_link` | Relation table's local `*_id` resolved through `migration_id_map` with owner + table namespace; preserves `link_kind`. Transactional validation of missing target IDs. |
| `state_fact` | Preserve project/workspace state scope, typed value, status and observation; do **not** conflate worktree-local state with project-wide authoritative state. |
| `migration_id_map` | Source DB kind, table and *exact* PK tuple → new stable target; row hash/column-type/null parity. Distinct old snapshots preserved read-only through rollback window. |
| retained typed code index | `resource`, `symbol`, `relation`, `occurrence`, generation/currentness structures stay typed and indexed. Durable observation snapshot design still requires explicit proof of parity. |

## Full physical field mapping (46 audited domain tables)

Legend: `K` typed preserved, `M` migrate shadow-copy after equivalence, `B` typed retained + durable handle/evidence bridge, `R` retained pending historical retention proof. `db_meta` and `schema_migration` are excluded from 46 business tables; both are retained and migration-ledger checksums must be preserved.

### global.db (7 tables)

#### `global.blueprint` — 15 columns; PK `id`; local FK 0; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **M** → migration_id_map.source_key_json (legacy INTEGER PK) |
| `uid` | `BLOB` NOT NULL | **M** → knowledge_item.uid (exact 16-byte UID) |
| `scope_kind` | `TEXT` NOT NULL | **M** → knowledge_scope.scope_kind + knowledge_scope_binding (legacy NULL preserved) |
| `scope_key` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_scope.scope_key + knowledge_scope_binding (legacy NULL preserved) |
| `blueprint_key` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.blueprint_key (typed TEXT, raw preservation) |
| `title` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.title (typed TEXT, raw preservation) |
| `intent` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.intent (typed TEXT, raw preservation) |
| `definition_json` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.definition_json (typed TEXT, raw preservation) |
| `status` | `TEXT` NOT NULL | **M** → knowledge_head.lifecycle_state via LEGACY_IMPORT, plus payload_json.status |
| `version` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.version (typed TEXT, raw preservation) |
| `source_kind` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.source_kind + unresolved/verified knowledge_evidence |
| `source_locator` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.source_locator + unresolved/verified knowledge_evidence |
| `source_revision` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.source_revision + unresolved/verified knowledge_evidence |
| `created_at` | `TEXT` NOT NULL | **M** → knowledge_item.created_at + knowledge_revision.payload_json.created_at (exact legacy) |
| `updated_at` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.updated_at (legacy time, NOT imported_at) |

**Row verification:**
original rows global.blueprint == migrated/preserved rows; FK-target parity for 0 local FK declarations; old PK mapping injective; preserve all 15 typed columns and NULLs; status M before cutover.

#### `global.project_git_lineage` — 5 columns; PK `id`; local FK 1; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **K** → global.project_git_lineage.id unchanged; mandatory preservation of data and references |
| `git_common_dir` | `TEXT` NOT NULL | **K** → global.project_git_lineage.git_common_dir unchanged; mandatory preservation of data and references |
| `project_uid` | `BLOB` NOT NULL | **K** → global.project_git_lineage.project_uid unchanged; mandatory preservation of data and references |
| `created_at` | `TEXT` NOT NULL | **K** → global.project_git_lineage.created_at unchanged; mandatory preservation of data and references |
| `updated_at` | `TEXT` NOT NULL | **K** → global.project_git_lineage.updated_at unchanged; mandatory preservation of data and references |

**Row verification:**
original rows global.project_git_lineage == migrated/preserved rows; FK-target parity for 1 local FK declarations; old PK mapping injective; preserve all 5 typed columns and NULLs; status K before cutover.

#### `global.project_registry` — 7 columns; PK `id`; local FK 0; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **K** → global.project_registry.id unchanged; mandatory preservation of data and references |
| `project_uid` | `BLOB` NOT NULL | **K** → global.project_registry.project_uid unchanged; mandatory preservation of data and references |
| `home_locator` | `TEXT` NOT NULL | **K** → global.project_registry.home_locator unchanged; mandatory preservation of data and references |
| `created_at` | `TEXT` NOT NULL | **K** → global.project_registry.created_at unchanged; mandatory preservation of data and references |
| `updated_at` | `TEXT` NOT NULL | **K** → global.project_registry.updated_at unchanged; mandatory preservation of data and references |
| `state` | `TEXT` NOT NULL | **K** → global.project_registry.state unchanged; mandatory preservation of data and references |
| `last_seen_at` | `TEXT` NULL permitted (PK rules apply) | **K** → global.project_registry.last_seen_at unchanged; mandatory preservation of data and references |

**Row verification:**
original rows global.project_registry == migrated/preserved rows; FK-target parity for 0 local FK declarations; old PK mapping injective; preserve all 7 typed columns and NULLs; status K before cutover.

#### `global.user_policy` — 16 columns; PK `id`; local FK 0; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **M** → migration_id_map.source_key_json (legacy INTEGER PK) |
| `uid` | `BLOB` NOT NULL | **M** → knowledge_item.uid (exact 16-byte UID) |
| `scope_kind` | `TEXT` NOT NULL | **M** → knowledge_scope.scope_kind + knowledge_scope_binding (legacy NULL preserved) |
| `scope_key` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_scope.scope_key + knowledge_scope_binding (legacy NULL preserved) |
| `policy_key` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.policy_key (typed TEXT, raw preservation) |
| `title` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.title (typed TEXT, raw preservation) |
| `rule_text` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.rule_text (typed TEXT, raw preservation) |
| `structured_rule_json` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.structured_rule_json (typed TEXT, raw preservation) |
| `protection_class` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.protection_class (typed TEXT, raw preservation) |
| `priority_class` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.priority_class (typed TEXT, raw preservation) |
| `status` | `TEXT` NOT NULL | **M** → knowledge_head.lifecycle_state via LEGACY_IMPORT, plus payload_json.status |
| `source_kind` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.source_kind + unresolved/verified knowledge_evidence |
| `source_locator` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.source_locator + unresolved/verified knowledge_evidence |
| `source_revision` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.source_revision + unresolved/verified knowledge_evidence |
| `created_at` | `TEXT` NOT NULL | **M** → knowledge_item.created_at + knowledge_revision.payload_json.created_at (exact legacy) |
| `updated_at` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.updated_at (legacy time, NOT imported_at) |

**Row verification:**
original rows global.user_policy == migrated/preserved rows; FK-target parity for 0 local FK declarations; old PK mapping injective; preserve all 16 typed columns and NULLs; status M before cutover.

#### `global.user_policy_link` — 3 columns; PK `user_policy_id`, `related_user_policy_id`, `link_kind`; local FK 2; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `user_policy_id` **PK** | `INTEGER` NOT NULL | **M** → knowledge_link.from_item_id (resolve via migration_id_map old row PK) |
| `related_user_policy_id` **PK** | `INTEGER` NOT NULL | **M** → knowledge_link.target_local_item_id (resolve via migration_id_map old row PK) |
| `link_kind` **PK** | `TEXT` NOT NULL | **M** → knowledge_link.link_kind (copy text exactly) |

**Row verification:**
original rows global.user_policy_link == migrated/preserved rows; FK-target parity for 2 local FK declarations; old PK mapping injective; preserve all 3 typed columns and NULLs; status M before cutover.

#### `global.user_preference` — 13 columns; PK `id`; local FK 0; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **M** → migration_id_map.source_key_json (legacy INTEGER PK) |
| `uid` | `BLOB` NOT NULL | **M** → knowledge_item.uid (exact 16-byte UID) |
| `scope_kind` | `TEXT` NOT NULL | **M** → knowledge_scope.scope_kind + knowledge_scope_binding (legacy NULL preserved) |
| `scope_key` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_scope.scope_key + knowledge_scope_binding (legacy NULL preserved) |
| `preference_key` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.preference_key (typed TEXT, raw preservation) |
| `value_type` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.value_type (typed TEXT, raw preservation) |
| `value_json` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.value_json (typed TEXT, raw preservation) |
| `status` | `TEXT` NOT NULL | **M** → knowledge_head.lifecycle_state via LEGACY_IMPORT, plus payload_json.status |
| `source_kind` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.source_kind + unresolved/verified knowledge_evidence |
| `source_locator` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.source_locator + unresolved/verified knowledge_evidence |
| `source_revision` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.source_revision + unresolved/verified knowledge_evidence |
| `created_at` | `TEXT` NOT NULL | **M** → knowledge_item.created_at + knowledge_revision.payload_json.created_at (exact legacy) |
| `updated_at` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.updated_at (legacy time, NOT imported_at) |

**Row verification:**
original rows global.user_preference == migrated/preserved rows; FK-target parity for 0 local FK declarations; old PK mapping injective; preserve all 13 typed columns and NULLs; status M before cutover.

#### `global.workspace_registry` — 9 columns; PK `id`; local FK 1; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **K** → global.workspace_registry.id unchanged; mandatory preservation of data and references |
| `workspace_uid` | `BLOB` NOT NULL | **K** → global.workspace_registry.workspace_uid unchanged; mandatory preservation of data and references |
| `project_uid` | `BLOB` NOT NULL | **K** → global.workspace_registry.project_uid unchanged; mandatory preservation of data and references |
| `locator` | `TEXT` NOT NULL | **K** → global.workspace_registry.locator unchanged; mandatory preservation of data and references |
| `is_project_home` | `INTEGER` NOT NULL | **K** → global.workspace_registry.is_project_home unchanged; mandatory preservation of data and references |
| `created_at` | `TEXT` NOT NULL | **K** → global.workspace_registry.created_at unchanged; mandatory preservation of data and references |
| `updated_at` | `TEXT` NOT NULL | **K** → global.workspace_registry.updated_at unchanged; mandatory preservation of data and references |
| `state` | `TEXT` NOT NULL | **K** → global.workspace_registry.state unchanged; mandatory preservation of data and references |
| `last_seen_at` | `TEXT` NULL permitted (PK rules apply) | **K** → global.workspace_registry.last_seen_at unchanged; mandatory preservation of data and references |

**Row verification:**
original rows global.workspace_registry == migrated/preserved rows; FK-target parity for 1 local FK declarations; old PK mapping injective; preserve all 9 typed columns and NULLs; status K before cutover.

### project.db (8 tables)

#### `project.blueprint` — 15 columns; PK `id`; local FK 0; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **M** → migration_id_map.source_key_json (legacy INTEGER PK) |
| `uid` | `BLOB` NOT NULL | **M** → knowledge_item.uid (exact 16-byte UID) |
| `scope_kind` | `TEXT` NOT NULL | **M** → knowledge_scope.scope_kind + knowledge_scope_binding (legacy NULL preserved) |
| `scope_key` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_scope.scope_key + knowledge_scope_binding (legacy NULL preserved) |
| `blueprint_key` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.blueprint_key (typed TEXT, raw preservation) |
| `title` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.title (typed TEXT, raw preservation) |
| `intent` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.intent (typed TEXT, raw preservation) |
| `definition_json` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.definition_json (typed TEXT, raw preservation) |
| `status` | `TEXT` NOT NULL | **M** → knowledge_head.lifecycle_state via LEGACY_IMPORT, plus payload_json.status |
| `version` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.version (typed TEXT, raw preservation) |
| `source_kind` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.source_kind + unresolved/verified knowledge_evidence |
| `source_locator` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.source_locator + unresolved/verified knowledge_evidence |
| `source_revision` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.source_revision + unresolved/verified knowledge_evidence |
| `created_at` | `TEXT` NOT NULL | **M** → knowledge_item.created_at + knowledge_revision.payload_json.created_at (exact legacy) |
| `updated_at` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.updated_at (legacy time, NOT imported_at) |

**Row verification:**
original rows project.blueprint == migrated/preserved rows; FK-target parity for 0 local FK declarations; old PK mapping injective; preserve all 15 typed columns and NULLs; status M before cutover.

#### `project.blueprint_application` — 13 columns; PK `id`; local FK 0; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **K** → project.blueprint_application.id unchanged; mandatory preservation of data and references |
| `uid` | `BLOB` NOT NULL | **K** → project.blueprint_application.uid unchanged; mandatory preservation of data and references |
| `blueprint_uid` | `BLOB` NOT NULL | **K** → project.blueprint_application.blueprint_uid unchanged; mandatory preservation of data and references |
| `scope_kind` | `TEXT` NOT NULL | **K** → project.blueprint_application.scope_kind unchanged; mandatory preservation of data and references |
| `scope_key` | `TEXT` NULL permitted (PK rules apply) | **K** → project.blueprint_application.scope_key unchanged; mandatory preservation of data and references |
| `status` | `TEXT` NOT NULL | **K** → project.blueprint_application.status unchanged; mandatory preservation of data and references |
| `application_summary` | `TEXT` NOT NULL | **K** → project.blueprint_application.application_summary unchanged; mandatory preservation of data and references |
| `source_kind` | `TEXT` NOT NULL | **K** → project.blueprint_application.source_kind unchanged; mandatory preservation of data and references |
| `source_locator` | `TEXT` NULL permitted (PK rules apply) | **K** → project.blueprint_application.source_locator unchanged; mandatory preservation of data and references |
| `created_at` | `TEXT` NOT NULL | **K** → project.blueprint_application.created_at unchanged; mandatory preservation of data and references |
| `updated_at` | `TEXT` NOT NULL | **K** → project.blueprint_application.updated_at unchanged; mandatory preservation of data and references |
| `blueprint_owner_kind` | `TEXT` NOT NULL | **K** → project.blueprint_application.blueprint_owner_kind unchanged; mandatory preservation of data and references |
| `source_revision` | `TEXT` NULL permitted (PK rules apply) | **K** → project.blueprint_application.source_revision unchanged; mandatory preservation of data and references |

**Row verification:**
original rows project.blueprint_application == migrated/preserved rows; FK-target parity for 0 local FK declarations; old PK mapping injective; preserve all 13 typed columns and NULLs; status K before cutover.

#### `project.decision` — 13 columns; PK `id`; local FK 0; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **M** → migration_id_map.source_key_json (legacy INTEGER PK) |
| `uid` | `BLOB` NOT NULL | **M** → knowledge_item.uid (exact 16-byte UID) |
| `scope_kind` | `TEXT` NOT NULL | **M** → knowledge_scope.scope_kind + knowledge_scope_binding (legacy NULL preserved) |
| `scope_key` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_scope.scope_key + knowledge_scope_binding (legacy NULL preserved) |
| `topic` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.topic (typed TEXT, raw preservation) |
| `chosen_summary` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.chosen_summary (typed TEXT, raw preservation) |
| `rationale` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.rationale (typed TEXT, raw preservation) |
| `status` | `TEXT` NOT NULL | **M** → knowledge_head.lifecycle_state via LEGACY_IMPORT, plus payload_json.status |
| `source_kind` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.source_kind + unresolved/verified knowledge_evidence |
| `source_locator` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.source_locator + unresolved/verified knowledge_evidence |
| `source_revision` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.source_revision + unresolved/verified knowledge_evidence |
| `created_at` | `TEXT` NOT NULL | **M** → knowledge_item.created_at + knowledge_revision.payload_json.created_at (exact legacy) |
| `updated_at` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.updated_at (legacy time, NOT imported_at) |

**Row verification:**
original rows project.decision == migrated/preserved rows; FK-target parity for 0 local FK declarations; old PK mapping injective; preserve all 13 typed columns and NULLs; status M before cutover.

#### `project.decision_link` — 3 columns; PK `decision_id`, `related_decision_id`, `link_kind`; local FK 2; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `decision_id` **PK** | `INTEGER` NOT NULL | **M** → knowledge_link.from_item_id (resolve via migration_id_map old row PK) |
| `related_decision_id` **PK** | `INTEGER` NOT NULL | **M** → knowledge_link.target_local_item_id (resolve via migration_id_map old row PK) |
| `link_kind` **PK** | `TEXT` NOT NULL | **M** → knowledge_link.link_kind (copy text exactly) |

**Row verification:**
original rows project.decision_link == migrated/preserved rows; FK-target parity for 2 local FK declarations; old PK mapping injective; preserve all 3 typed columns and NULLs; status M before cutover.

#### `project.knowledge_promotion` — 17 columns; PK `id`; local FK 0; unique indexes 2

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **K** → project.knowledge_promotion.id unchanged; mandatory preservation of data and references |
| `uid` | `BLOB` NOT NULL | **K** → project.knowledge_promotion.uid unchanged; mandatory preservation of data and references |
| `workspace_uid` | `BLOB` NOT NULL | **K** → project.knowledge_promotion.workspace_uid unchanged; mandatory preservation of data and references |
| `work_note_uid` | `BLOB` NOT NULL | **K** → project.knowledge_promotion.work_note_uid unchanged; mandatory preservation of data and references |
| `work_note_kind` | `TEXT` NOT NULL | **K** → project.knowledge_promotion.work_note_kind unchanged; mandatory preservation of data and references |
| `work_note_source_kind` | `TEXT` NOT NULL | **K** → project.knowledge_promotion.work_note_source_kind unchanged; mandatory preservation of data and references |
| `work_note_fingerprint` | `TEXT` NOT NULL | **K** → project.knowledge_promotion.work_note_fingerprint unchanged; mandatory preservation of data and references |
| `target_kind` | `TEXT` NOT NULL | **K** → project.knowledge_promotion.target_kind unchanged; mandatory preservation of data and references |
| `target_uid` | `BLOB` NOT NULL | **K** → project.knowledge_promotion.target_uid unchanged; mandatory preservation of data and references |
| `request_fingerprint` | `TEXT` NOT NULL | **K** → project.knowledge_promotion.request_fingerprint unchanged; mandatory preservation of data and references |
| `promotion_basis` | `TEXT` NOT NULL | **K** → project.knowledge_promotion.promotion_basis unchanged; mandatory preservation of data and references |
| `authority_source_kind` | `TEXT` NOT NULL | **K** → project.knowledge_promotion.authority_source_kind unchanged; mandatory preservation of data and references |
| `authority_source_locator` | `TEXT` NULL permitted (PK rules apply) | **K** → project.knowledge_promotion.authority_source_locator unchanged; mandatory preservation of data and references |
| `authority_source_revision` | `TEXT` NULL permitted (PK rules apply) | **K** → project.knowledge_promotion.authority_source_revision unchanged; mandatory preservation of data and references |
| `lineage_kind` | `TEXT` NULL permitted (PK rules apply) | **K** → project.knowledge_promotion.lineage_kind unchanged; mandatory preservation of data and references |
| `lineage_target_uid` | `BLOB` NULL permitted (PK rules apply) | **K** → project.knowledge_promotion.lineage_target_uid unchanged; mandatory preservation of data and references |
| `created_at` | `TEXT` NOT NULL | **K** → project.knowledge_promotion.created_at unchanged; mandatory preservation of data and references |

**Row verification:**
original rows project.knowledge_promotion == migrated/preserved rows; FK-target parity for 0 local FK declarations; old PK mapping injective; preserve all 17 typed columns and NULLs; status K before cutover.

#### `project.policy` — 16 columns; PK `id`; local FK 0; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **M** → migration_id_map.source_key_json (legacy INTEGER PK) |
| `uid` | `BLOB` NOT NULL | **M** → knowledge_item.uid (exact 16-byte UID) |
| `scope_kind` | `TEXT` NOT NULL | **M** → knowledge_scope.scope_kind + knowledge_scope_binding (legacy NULL preserved) |
| `scope_key` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_scope.scope_key + knowledge_scope_binding (legacy NULL preserved) |
| `policy_key` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.policy_key (typed TEXT, raw preservation) |
| `title` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.title (typed TEXT, raw preservation) |
| `rule_text` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.rule_text (typed TEXT, raw preservation) |
| `structured_rule_json` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.structured_rule_json (typed TEXT, raw preservation) |
| `protection_class` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.protection_class (typed TEXT, raw preservation) |
| `priority_class` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.priority_class (typed TEXT, raw preservation) |
| `status` | `TEXT` NOT NULL | **M** → knowledge_head.lifecycle_state via LEGACY_IMPORT, plus payload_json.status |
| `source_kind` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.source_kind + unresolved/verified knowledge_evidence |
| `source_locator` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.source_locator + unresolved/verified knowledge_evidence |
| `source_revision` | `TEXT` NULL permitted (PK rules apply) | **M** → knowledge_revision.payload_json.source_revision + unresolved/verified knowledge_evidence |
| `created_at` | `TEXT` NOT NULL | **M** → knowledge_item.created_at + knowledge_revision.payload_json.created_at (exact legacy) |
| `updated_at` | `TEXT` NOT NULL | **M** → knowledge_revision.payload_json.updated_at (legacy time, NOT imported_at) |

**Row verification:**
original rows project.policy == migrated/preserved rows; FK-target parity for 0 local FK declarations; old PK mapping injective; preserve all 16 typed columns and NULLs; status M before cutover.

#### `project.policy_link` — 3 columns; PK `policy_id`, `related_policy_id`, `link_kind`; local FK 2; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `policy_id` **PK** | `INTEGER` NOT NULL | **M** → knowledge_link.from_item_id (resolve via migration_id_map old row PK) |
| `related_policy_id` **PK** | `INTEGER` NOT NULL | **M** → knowledge_link.target_local_item_id (resolve via migration_id_map old row PK) |
| `link_kind` **PK** | `TEXT` NOT NULL | **M** → knowledge_link.link_kind (copy text exactly) |

**Row verification:**
original rows project.policy_link == migrated/preserved rows; FK-target parity for 2 local FK declarations; old PK mapping injective; preserve all 3 typed columns and NULLs; status M before cutover.

#### `project.project_state` — 12 columns; PK `id`; local FK 0; unique indexes 3

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **M** → migration_id_map.source_key_json (old INTEGER PK; retain old value) |
| `state_key` | `TEXT` NOT NULL | **M** → state_fact.state_key (raw legacy retained) |
| `scope_kind` | `TEXT` NOT NULL | **M** → state_fact.scope_kind (raw legacy retained) |
| `scope_key` | `TEXT` NULL permitted (PK rules apply) | **M** → state_fact.scope_key (raw legacy retained) |
| `value_type` | `TEXT` NOT NULL | **M** → state_fact.value_type (raw legacy retained) |
| `value_json` | `TEXT` NOT NULL | **M** → state_fact.value_json (raw legacy retained) |
| `status` | `TEXT` NOT NULL | **M** → state_fact.lifecycle_state (raw legacy retained) |
| `source_kind` | `TEXT` NOT NULL | **M** → state_fact.source_kind (raw legacy retained) |
| `source_locator` | `TEXT` NULL permitted (PK rules apply) | **M** → state_fact.source_locator (raw legacy retained) |
| `observed_revision` | `TEXT` NULL permitted (PK rules apply) | **M** → state_fact.observed_revision (raw legacy retained) |
| `updated_at` | `TEXT` NOT NULL | **M** → state_fact.legacy_updated_at (raw legacy retained) |
| `uid` | `BLOB` NULL permitted (PK rules apply) | **M** → state_fact.uid (if null: new UID + exact legacy NULL in snapshot) |

**Row verification:**
original rows project.project_state == migrated/preserved rows; FK-target parity for 0 local FK declarations; old PK mapping injective; preserve all 12 typed columns and NULLs; status M before cutover.

### workspace.db (10 tables)

#### `workspace.verification_job` — 10 columns; PK `id`; local FK 0; unique indexes 2

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **K** → workspace.verification_job.id unchanged; mandatory preservation of data and references |
| `uid` | `BLOB` NOT NULL | **K** → workspace.verification_job.uid unchanged; mandatory preservation of data and references |
| `idempotency_key` | `TEXT` NOT NULL | **K** → workspace.verification_job.idempotency_key unchanged; mandatory preservation of data and references |
| `request_fingerprint` | `BLOB` NOT NULL | **K** → workspace.verification_job.request_fingerprint unchanged; mandatory preservation of data and references |
| `state` | `TEXT` NOT NULL | **K** → workspace.verification_job.state unchanged; mandatory preservation of data and references |
| `command_count` | `INTEGER` NOT NULL | **K** → workspace.verification_job.command_count unchanged; mandatory preservation of data and references |
| `final_summary` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.verification_job.final_summary unchanged; mandatory preservation of data and references |
| `created_at` | `TEXT` NOT NULL | **K** → workspace.verification_job.created_at unchanged; mandatory preservation of data and references |
| `updated_at` | `TEXT` NOT NULL | **K** → workspace.verification_job.updated_at unchanged; mandatory preservation of data and references |
| `finished_at` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.verification_job.finished_at unchanged; mandatory preservation of data and references |

**Row verification:**
original rows workspace.verification_job == migrated/preserved rows; FK-target parity for 0 local FK declarations; old PK mapping injective; preserve all 10 typed columns and NULLs; status K before cutover.

#### `workspace.verification_job_event` — 5 columns; PK `job_id`, `seq`; local FK 1; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `job_id` **PK** | `INTEGER` NOT NULL | **K** → workspace.verification_job_event.job_id unchanged; mandatory preservation of data and references |
| `seq` **PK** | `INTEGER` NOT NULL | **K** → workspace.verification_job_event.seq unchanged; mandatory preservation of data and references |
| `kind` | `TEXT` NOT NULL | **K** → workspace.verification_job_event.kind unchanged; mandatory preservation of data and references |
| `payload_json` | `TEXT` NOT NULL | **K** → workspace.verification_job_event.payload_json unchanged; mandatory preservation of data and references |
| `created_at` | `TEXT` NOT NULL | **K** → workspace.verification_job_event.created_at unchanged; mandatory preservation of data and references |

**Row verification:**
original rows workspace.verification_job_event == migrated/preserved rows; FK-target parity for 1 local FK declarations; old PK mapping injective; preserve all 5 typed columns and NULLs; status K before cutover.

#### `workspace.work_handoff` — 7 columns; PK `id`; local FK 1; unique indexes 0

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **K** → workspace.work_handoff.id unchanged; mandatory preservation of data and references |
| `work_item_id` | `INTEGER` NOT NULL | **K** → workspace.work_handoff.work_item_id unchanged; mandatory preservation of data and references |
| `handoff_summary` | `TEXT` NOT NULL | **K** → workspace.work_handoff.handoff_summary unchanged; mandatory preservation of data and references |
| `remaining_summary` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.work_handoff.remaining_summary unchanged; mandatory preservation of data and references |
| `blocker_summary` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.work_handoff.blocker_summary unchanged; mandatory preservation of data and references |
| `next_scope_hint` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.work_handoff.next_scope_hint unchanged; mandatory preservation of data and references |
| `created_at` | `TEXT` NOT NULL | **K** → workspace.work_handoff.created_at unchanged; mandatory preservation of data and references |

**Row verification:**
original rows workspace.work_handoff == migrated/preserved rows; FK-target parity for 1 local FK declarations; old PK mapping injective; preserve all 7 typed columns and NULLs; status K before cutover.

#### `workspace.work_item` — 9 columns; PK `id`; local FK 0; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **K** → workspace.work_item.id unchanged; mandatory preservation of data and references |
| `uid` | `BLOB` NOT NULL | **K** → workspace.work_item.uid unchanged; mandatory preservation of data and references |
| `source_kind` | `TEXT` NOT NULL | **K** → workspace.work_item.source_kind unchanged; mandatory preservation of data and references |
| `source_ref` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.work_item.source_ref unchanged; mandatory preservation of data and references |
| `title` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.work_item.title unchanged; mandatory preservation of data and references |
| `goal` | `TEXT` NOT NULL | **K** → workspace.work_item.goal unchanged; mandatory preservation of data and references |
| `status` | `TEXT` NOT NULL | **K** → workspace.work_item.status unchanged; mandatory preservation of data and references |
| `created_at` | `TEXT` NOT NULL | **K** → workspace.work_item.created_at unchanged; mandatory preservation of data and references |
| `closed_at` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.work_item.closed_at unchanged; mandatory preservation of data and references |

**Row verification:**
original rows workspace.work_item == migrated/preserved rows; FK-target parity for 0 local FK declarations; old PK mapping injective; preserve all 9 typed columns and NULLs; status K before cutover.

#### `workspace.work_note` — 13 columns; PK `id`; local FK 1; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **K** → workspace.work_note.id unchanged; mandatory preservation of data and references |
| `uid` | `BLOB` NOT NULL | **K** → workspace.work_note.uid unchanged; mandatory preservation of data and references |
| `work_item_id` | `INTEGER` NOT NULL | **K** → workspace.work_note.work_item_id unchanged; mandatory preservation of data and references |
| `kind` | `TEXT` NOT NULL | **K** → workspace.work_note.kind unchanged; mandatory preservation of data and references |
| `note_text` | `TEXT` NOT NULL | **K** → workspace.work_note.note_text unchanged; mandatory preservation of data and references |
| `status` | `TEXT` NOT NULL | **K** → workspace.work_note.status unchanged; mandatory preservation of data and references |
| `source_kind` | `TEXT` NOT NULL | **K** → workspace.work_note.source_kind unchanged; mandatory preservation of data and references |
| `source_ref` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.work_note.source_ref unchanged; mandatory preservation of data and references |
| `source_revision` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.work_note.source_revision unchanged; mandatory preservation of data and references |
| `promoted_item_kind` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.work_note.promoted_item_kind unchanged; mandatory preservation of data and references |
| `promoted_item_uid` | `BLOB` NULL permitted (PK rules apply) | **K** → workspace.work_note.promoted_item_uid unchanged; mandatory preservation of data and references |
| `created_at` | `TEXT` NOT NULL | **K** → workspace.work_note.created_at unchanged; mandatory preservation of data and references |
| `updated_at` | `TEXT` NOT NULL | **K** → workspace.work_note.updated_at unchanged; mandatory preservation of data and references |

**Row verification:**
original rows workspace.work_note == migrated/preserved rows; FK-target parity for 1 local FK declarations; old PK mapping injective; preserve all 13 typed columns and NULLs; status K before cutover.

#### `workspace.work_resource` — 7 columns; PK `id`; local FK 1; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **K** → workspace.work_resource.id unchanged; mandatory preservation of data and references |
| `work_item_id` | `INTEGER` NOT NULL | **K** → workspace.work_resource.work_item_id unchanged; mandatory preservation of data and references |
| `resource_uid` | `BLOB` NOT NULL | **K** → workspace.work_resource.resource_uid unchanged; mandatory preservation of data and references |
| `role` | `TEXT` NOT NULL | **K** → workspace.work_resource.role unchanged; mandatory preservation of data and references |
| `locator_hint` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.work_resource.locator_hint unchanged; mandatory preservation of data and references |
| `first_observed_revision` | `TEXT` NOT NULL | **K** → workspace.work_resource.first_observed_revision unchanged; mandatory preservation of data and references |
| `last_observed_revision` | `TEXT` NOT NULL | **K** → workspace.work_resource.last_observed_revision unchanged; mandatory preservation of data and references |

**Row verification:**
original rows workspace.work_resource == migrated/preserved rows; FK-target parity for 1 local FK declarations; old PK mapping injective; preserve all 7 typed columns and NULLs; status K before cutover.

#### `workspace.work_result` — 14 columns; PK `id`; local FK 2; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **K** → workspace.work_result.id unchanged; mandatory preservation of data and references |
| `work_item_id` | `INTEGER` NOT NULL | **K** → workspace.work_result.work_item_id unchanged; mandatory preservation of data and references |
| `result_status` | `TEXT` NOT NULL | **K** → workspace.work_result.result_status unchanged; mandatory preservation of data and references |
| `result_summary` | `TEXT` NOT NULL | **K** → workspace.work_result.result_summary unchanged; mandatory preservation of data and references |
| `commit_id` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.work_result.commit_id unchanged; mandatory preservation of data and references |
| `change_set_fingerprint` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.work_result.change_set_fingerprint unchanged; mandatory preservation of data and references |
| `verification_summary` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.work_result.verification_summary unchanged; mandatory preservation of data and references |
| `result_workspace_revision` | `TEXT` NOT NULL | **K** → workspace.work_result.result_workspace_revision unchanged; mandatory preservation of data and references |
| `result_generation_no` | `INTEGER` NULL permitted (PK rules apply) | **K** → workspace.work_result.result_generation_no unchanged; mandatory preservation of data and references |
| `created_at` | `TEXT` NOT NULL | **K** → workspace.work_result.created_at unchanged; mandatory preservation of data and references |
| `remaining_dirty_fingerprint` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.work_result.remaining_dirty_fingerprint unchanged; mandatory preservation of data and references |
| `remaining_dirty_state` | `TEXT` NOT NULL | **K** → workspace.work_result.remaining_dirty_state unchanged; mandatory preservation of data and references |
| `result_index_incarnation_uid` | `BLOB` NULL permitted (PK rules apply) | **K** → workspace.work_result.result_index_incarnation_uid unchanged; mandatory preservation of data and references |
| `verification_job_id` | `INTEGER` NULL permitted (PK rules apply) | **K** → workspace.work_result.verification_job_id unchanged; mandatory preservation of data and references |

**Row verification:**
original rows workspace.work_result == migrated/preserved rows; FK-target parity for 2 local FK declarations; old PK mapping injective; preserve all 14 typed columns and NULLs; status K before cutover.

#### `workspace.working_state` — 15 columns; PK `id`; local FK 1; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **K** → workspace.working_state.id unchanged; mandatory preservation of data and references |
| `work_item_id` | `INTEGER` NOT NULL | **K** → workspace.working_state.work_item_id unchanged; mandatory preservation of data and references |
| `baseline_workspace_revision` | `TEXT` NOT NULL | **K** → workspace.working_state.baseline_workspace_revision unchanged; mandatory preservation of data and references |
| `baseline_generation_no` | `INTEGER` NOT NULL | **K** → workspace.working_state.baseline_generation_no unchanged; mandatory preservation of data and references |
| `baseline_head` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.working_state.baseline_head unchanged; mandatory preservation of data and references |
| `baseline_dirty_fingerprint` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.working_state.baseline_dirty_fingerprint unchanged; mandatory preservation of data and references |
| `current_step` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.working_state.current_step unchanged; mandatory preservation of data and references |
| `progress_summary` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.working_state.progress_summary unchanged; mandatory preservation of data and references |
| `remaining_summary` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.working_state.remaining_summary unchanged; mandatory preservation of data and references |
| `blocker_summary` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.working_state.blocker_summary unchanged; mandatory preservation of data and references |
| `owner_agent` | `TEXT` NULL permitted (PK rules apply) | **K** → workspace.working_state.owner_agent unchanged; mandatory preservation of data and references |
| `last_observed_workspace_revision` | `TEXT` NOT NULL | **K** → workspace.working_state.last_observed_workspace_revision unchanged; mandatory preservation of data and references |
| `updated_at` | `TEXT` NOT NULL | **K** → workspace.working_state.updated_at unchanged; mandatory preservation of data and references |
| `baseline_dirty_state` | `TEXT` NOT NULL | **K** → workspace.working_state.baseline_dirty_state unchanged; mandatory preservation of data and references |
| `baseline_index_incarnation_uid` | `BLOB` NULL permitted (PK rules apply) | **K** → workspace.working_state.baseline_index_incarnation_uid unchanged; mandatory preservation of data and references |

**Row verification:**
original rows workspace.working_state == migrated/preserved rows; FK-target parity for 1 local FK declarations; old PK mapping injective; preserve all 15 typed columns and NULLs; status K before cutover.

#### `workspace.workspace_project_state` — 12 columns; PK `id`; local FK 0; unique indexes 2

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **M** → migration_id_map.source_key_json (old INTEGER PK; retain old value) |
| `uid` | `BLOB` NOT NULL | **M** → state_fact.uid (if null: new UID + exact legacy NULL in snapshot) |
| `state_key` | `TEXT` NOT NULL | **M** → state_fact.state_key (raw legacy retained) |
| `scope_kind` | `TEXT` NOT NULL | **M** → state_fact.scope_kind (raw legacy retained) |
| `scope_key` | `TEXT` NULL permitted (PK rules apply) | **M** → state_fact.scope_key (raw legacy retained) |
| `value_type` | `TEXT` NOT NULL | **M** → state_fact.value_type (raw legacy retained) |
| `value_json` | `TEXT` NOT NULL | **M** → state_fact.value_json (raw legacy retained) |
| `status` | `TEXT` NOT NULL | **M** → state_fact.lifecycle_state (raw legacy retained) |
| `source_kind` | `TEXT` NOT NULL | **M** → state_fact.source_kind (raw legacy retained) |
| `source_locator` | `TEXT` NULL permitted (PK rules apply) | **M** → state_fact.source_locator (raw legacy retained) |
| `observed_revision` | `TEXT` NULL permitted (PK rules apply) | **M** → state_fact.observed_revision (raw legacy retained) |
| `updated_at` | `TEXT` NOT NULL | **M** → state_fact.legacy_updated_at (raw legacy retained) |

**Row verification:**
original rows workspace.workspace_project_state == migrated/preserved rows; FK-target parity for 0 local FK declarations; old PK mapping injective; preserve all 12 typed columns and NULLs; status M before cutover.

#### `workspace.workspace_state` — 3 columns; PK `id`; local FK 0; unique indexes 0

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **K** → workspace.workspace_state.id unchanged; mandatory preservation of data and references |
| `created_at` | `TEXT` NOT NULL | **K** → workspace.workspace_state.created_at unchanged; mandatory preservation of data and references |
| `updated_at` | `TEXT` NOT NULL | **K** → workspace.workspace_state.updated_at unchanged; mandatory preservation of data and references |

**Row verification:**
original rows workspace.workspace_state == migrated/preserved rows; FK-target parity for 0 local FK declarations; old PK mapping injective; preserve all 3 typed columns and NULLs; status K before cutover.

### index.db (21 tables)

#### `index.analysis_profile` — 13 columns; PK `id`; local FK 0; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **B** → index.analysis_profile.id local; migration_id_map index-scoped key; never sole durable ID |
| `profile_key` | `TEXT` NOT NULL | **B** → index.analysis_profile.profile_key typed hot path; optional extracted-result snapshot once validated |
| `language` | `TEXT` NOT NULL | **B** → index.analysis_profile.language typed hot path; optional extracted-result snapshot once validated |
| `analysis_mode` | `TEXT` NOT NULL | **B** → index.analysis_profile.analysis_mode typed hot path; optional extracted-result snapshot once validated |
| `structural_backend` | `TEXT` NOT NULL | **B** → index.analysis_profile.structural_backend typed hot path; optional extracted-result snapshot once validated |
| `structural_backend_version` | `TEXT` NOT NULL | **B** → index.analysis_profile.structural_backend_version typed hot path; optional extracted-result snapshot once validated |
| `semantic_backend` | `TEXT` NULL permitted (PK rules apply) | **B** → index.analysis_profile.semantic_backend typed hot path; optional extracted-result snapshot once validated |
| `semantic_backend_version` | `TEXT` NULL permitted (PK rules apply) | **B** → index.analysis_profile.semantic_backend_version typed hot path; optional extracted-result snapshot once validated |
| `extractor_semantics_version` | `TEXT` NOT NULL | **B** → index.analysis_profile.extractor_semantics_version typed hot path; optional extracted-result snapshot once validated |
| `adapter_semantics_version` | `TEXT` NOT NULL | **B** → index.analysis_profile.adapter_semantics_version typed hot path; optional extracted-result snapshot once validated |
| `backend_compatibility_class` | `TEXT` NOT NULL | **B** → index.analysis_profile.backend_compatibility_class typed hot path; optional extracted-result snapshot once validated |
| `capability_fingerprint` | `TEXT` NOT NULL | **B** → index.analysis_profile.capability_fingerprint typed hot path; optional extracted-result snapshot once validated |
| `created_at` | `TEXT` NOT NULL | **B** → index.analysis_profile.created_at typed hot path; optional extracted-result snapshot once validated |

**Row verification:**
original rows index.analysis_profile == migrated/preserved rows; FK-target parity for 0 local FK declarations; old PK mapping injective; preserve all 13 typed columns and NULLs; status B before cutover.

#### `index.change_journal` — 13 columns; PK `seq`; local FK 2; unique indexes 0

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `seq` **PK** | `INTEGER` NULL permitted (PK rules apply) | **R** → index.change_journal.seq unchanged; classify retention and rebuild preconditions |
| `workspace_revision` | `TEXT` NOT NULL | **R** → index.change_journal.workspace_revision unchanged; classify retention and rebuild preconditions |
| `event_kind` | `TEXT` NOT NULL | **R** → index.change_journal.event_kind unchanged; classify retention and rebuild preconditions |
| `resource_uid` | `BLOB` NULL permitted (PK rules apply) | **R** → index.change_journal.resource_uid unchanged; classify retention and rebuild preconditions |
| `path_before` | `TEXT` NULL permitted (PK rules apply) | **R** → index.change_journal.path_before unchanged; classify retention and rebuild preconditions |
| `path_after` | `TEXT` NULL permitted (PK rules apply) | **R** → index.change_journal.path_after unchanged; classify retention and rebuild preconditions |
| `observed_size` | `INTEGER` NULL permitted (PK rules apply) | **R** → index.change_journal.observed_size unchanged; classify retention and rebuild preconditions |
| `observed_mtime_ns` | `INTEGER` NULL permitted (PK rules apply) | **R** → index.change_journal.observed_mtime_ns unchanged; classify retention and rebuild preconditions |
| `candidate_fingerprint` | `TEXT` NULL permitted (PK rules apply) | **R** → index.change_journal.candidate_fingerprint unchanged; classify retention and rebuild preconditions |
| `processing_state` | `TEXT` NOT NULL | **R** → index.change_journal.processing_state unchanged; classify retention and rebuild preconditions |
| `coalesced_into_seq` | `INTEGER` NULL permitted (PK rules apply) | **R** → index.change_journal.coalesced_into_seq unchanged; classify retention and rebuild preconditions |
| `observed_at` | `TEXT` NOT NULL | **R** → index.change_journal.observed_at unchanged; classify retention and rebuild preconditions |
| `applied_generation_id` | `INTEGER` NULL permitted (PK rules apply) | **R** → index.change_journal.applied_generation_id unchanged; classify retention and rebuild preconditions |

**Row verification:**
original rows index.change_journal == migrated/preserved rows; FK-target parity for 2 local FK declarations; old PK mapping injective; preserve all 13 typed columns and NULLs; status R before cutover.

#### `index.component_state` — 11 columns; PK `id`; local FK 1; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **R** → index.component_state.id unchanged; classify retention and rebuild preconditions |
| `component_kind` | `TEXT` NOT NULL | **R** → index.component_state.component_kind unchanged; classify retention and rebuild preconditions |
| `scope_kind` | `TEXT` NOT NULL | **R** → index.component_state.scope_kind unchanged; classify retention and rebuild preconditions |
| `scope_key` | `TEXT` NOT NULL | **R** → index.component_state.scope_key unchanged; classify retention and rebuild preconditions |
| `basis_workspace_revision` | `TEXT` NOT NULL | **R** → index.component_state.basis_workspace_revision unchanged; classify retention and rebuild preconditions |
| `stable_generation_id` | `INTEGER` NULL permitted (PK rules apply) | **R** → index.component_state.stable_generation_id unchanged; classify retention and rebuild preconditions |
| `processing_state` | `TEXT` NOT NULL | **R** → index.component_state.processing_state unchanged; classify retention and rebuild preconditions |
| `freshness_state` | `TEXT` NOT NULL | **R** → index.component_state.freshness_state unchanged; classify retention and rebuild preconditions |
| `last_error_code` | `TEXT` NULL permitted (PK rules apply) | **R** → index.component_state.last_error_code unchanged; classify retention and rebuild preconditions |
| `updated_at` | `TEXT` NOT NULL | **R** → index.component_state.updated_at unchanged; classify retention and rebuild preconditions |
| `detail_state` | `TEXT` NULL permitted (PK rules apply) | **R** → index.component_state.detail_state unchanged; classify retention and rebuild preconditions |

**Row verification:**
original rows index.component_state == migrated/preserved rows; FK-target parity for 1 local FK declarations; old PK mapping injective; preserve all 11 typed columns and NULLs; status R before cutover.

#### `index.domain_entity` — 6 columns; PK `id`; local FK 0; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **B** → index.domain_entity.id local; migration_id_map index-scoped key; never sole durable ID |
| `kind` | `TEXT` NOT NULL | **B** → index.domain_entity.kind typed hot path; optional extracted-result snapshot once validated |
| `normalized_identity` | `TEXT` NOT NULL | **B** → index.domain_entity.normalized_identity typed hot path; optional extracted-result snapshot once validated |
| `namespace` | `TEXT` NULL permitted (PK rules apply) | **B** → index.domain_entity.namespace typed hot path; optional extracted-result snapshot once validated |
| `method` | `TEXT` NULL permitted (PK rules apply) | **B** → index.domain_entity.method typed hot path; optional extracted-result snapshot once validated |
| `display_label` | `TEXT` NOT NULL | **B** → index.domain_entity.display_label typed hot path; optional extracted-result snapshot once validated |

**Row verification:**
original rows index.domain_entity == migrated/preserved rows; FK-target parity for 0 local FK declarations; old PK mapping injective; preserve all 6 typed columns and NULLs; status B before cutover.

#### `index.external_entity` — 8 columns; PK `id`; local FK 0; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **B** → index.external_entity.id local; migration_id_map index-scoped key; never sole durable ID |
| `package_identity` | `TEXT` NOT NULL | **B** → index.external_entity.package_identity typed hot path; optional extracted-result snapshot once validated |
| `module_path` | `TEXT` NULL permitted (PK rules apply) | **B** → index.external_entity.module_path typed hot path; optional extracted-result snapshot once validated |
| `symbol_name` | `TEXT` NULL permitted (PK rules apply) | **B** → index.external_entity.symbol_name typed hot path; optional extracted-result snapshot once validated |
| `qualified_name` | `TEXT` NULL permitted (PK rules apply) | **B** → index.external_entity.qualified_name typed hot path; optional extracted-result snapshot once validated |
| `kind` | `TEXT` NOT NULL | **B** → index.external_entity.kind typed hot path; optional extracted-result snapshot once validated |
| `resolved_version` | `TEXT` NULL permitted (PK rules apply) | **B** → index.external_entity.resolved_version typed hot path; optional extracted-result snapshot once validated |
| `declaration_locator` | `TEXT` NULL permitted (PK rules apply) | **B** → index.external_entity.declaration_locator typed hot path; optional extracted-result snapshot once validated |

**Row verification:**
original rows index.external_entity == migrated/preserved rows; FK-target parity for 0 local FK declarations; old PK mapping injective; preserve all 8 typed columns and NULLs; status B before cutover.

#### `index.generation` — 7 columns; PK `id`; local FK 0; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **R** → index.generation.id unchanged; classify retention and rebuild preconditions |
| `generation_no` | `INTEGER` NOT NULL | **R** → index.generation.generation_no unchanged; classify retention and rebuild preconditions |
| `basis_workspace_revision` | `TEXT` NOT NULL | **R** → index.generation.basis_workspace_revision unchanged; classify retention and rebuild preconditions |
| `state` | `TEXT` NOT NULL | **R** → index.generation.state unchanged; classify retention and rebuild preconditions |
| `created_at` | `TEXT` NOT NULL | **R** → index.generation.created_at unchanged; classify retention and rebuild preconditions |
| `published_at` | `TEXT` NULL permitted (PK rules apply) | **R** → index.generation.published_at unchanged; classify retention and rebuild preconditions |
| `aborted_reason` | `TEXT` NULL permitted (PK rules apply) | **R** → index.generation.aborted_reason unchanged; classify retention and rebuild preconditions |

**Row verification:**
original rows index.generation == migrated/preserved rows; FK-target parity for 0 local FK declarations; old PK mapping injective; preserve all 7 typed columns and NULLs; status R before cutover.

#### `index.graph_entity` — 7 columns; PK `id`; local FK 5; unique indexes 5

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **B** → index.graph_entity.id local; migration_id_map index-scoped key; never sole durable ID |
| `entity_kind` | `TEXT` NOT NULL | **B** → index.graph_entity.entity_kind typed hot path; optional extracted-result snapshot once validated |
| `resource_id` | `INTEGER` NULL permitted (PK rules apply) | **B** → index.graph_entity.resource_id typed hot path; optional extracted-result snapshot once validated |
| `symbol_id` | `INTEGER` NULL permitted (PK rules apply) | **B** → index.graph_entity.symbol_id typed hot path; optional extracted-result snapshot once validated |
| `external_entity_id` | `INTEGER` NULL permitted (PK rules apply) | **B** → index.graph_entity.external_entity_id typed hot path; optional extracted-result snapshot once validated |
| `domain_entity_id` | `INTEGER` NULL permitted (PK rules apply) | **B** → index.graph_entity.domain_entity_id typed hot path; optional extracted-result snapshot once validated |
| `logical_symbol_id` | `INTEGER` NULL permitted (PK rules apply) | **B** → index.graph_entity.logical_symbol_id typed hot path; optional extracted-result snapshot once validated |

**Row verification:**
original rows index.graph_entity == migrated/preserved rows; FK-target parity for 5 local FK declarations; old PK mapping injective; preserve all 7 typed columns and NULLs; status B before cutover.

#### `index.logical_symbol` — 7 columns; PK `id`; local FK 1; unique indexes 2

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **B** → index.logical_symbol.id local; migration_id_map index-scoped key; never sole durable ID |
| `uid` | `BLOB` NOT NULL | **B** → index.logical_symbol.uid preserved; durable stable source/entity handle only after identity proof |
| `identity_fingerprint` | `TEXT` NOT NULL | **B** → index.logical_symbol.identity_fingerprint typed hot path; optional extracted-result snapshot once validated |
| `context_key` | `TEXT` NOT NULL | **B** → index.logical_symbol.context_key typed hot path; optional extracted-result snapshot once validated |
| `kind` | `TEXT` NOT NULL | **B** → index.logical_symbol.kind typed hot path; optional extracted-result snapshot once validated |
| `display_name` | `TEXT` NOT NULL | **B** → index.logical_symbol.display_name typed hot path; optional extracted-result snapshot once validated |
| `created_generation` | `INTEGER` NOT NULL | **B** → index.logical_symbol.created_generation + source/generation/profile revision bridge (no blind rewrites) |

**Row verification:**
original rows index.logical_symbol == migrated/preserved rows; FK-target parity for 1 local FK declarations; old PK mapping injective; preserve all 7 typed columns and NULLs; status B before cutover.

#### `index.logical_symbol_declaration` — 4 columns; PK `logical_symbol_id`, `symbol_id`; local FK 3; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `logical_symbol_id` **PK** | `INTEGER` NOT NULL | **B** → index.logical_symbol_declaration.logical_symbol_id typed hot path; optional extracted-result snapshot once validated |
| `symbol_id` **PK** | `INTEGER` NOT NULL | **B** → index.logical_symbol_declaration.symbol_id typed hot path; optional extracted-result snapshot once validated |
| `context_key` | `TEXT` NOT NULL | **B** → index.logical_symbol_declaration.context_key typed hot path; optional extracted-result snapshot once validated |
| `generation_id` | `INTEGER` NOT NULL | **B** → index.logical_symbol_declaration.generation_id + source/generation/profile revision bridge (no blind rewrites) |

**Row verification:**
original rows index.logical_symbol_declaration == migrated/preserved rows; FK-target parity for 3 local FK declarations; old PK mapping injective; preserve all 4 typed columns and NULLs; status B before cutover.

#### `index.occurrence` — 15 columns; PK `id`; local FK 6; unique indexes 0

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **B** → index.occurrence.id local; migration_id_map index-scoped key; never sole durable ID |
| `resource_id` | `INTEGER` NOT NULL | **B** → index.occurrence.resource_id typed hot path; optional extracted-result snapshot once validated |
| `containing_symbol_id` | `INTEGER` NULL permitted (PK rules apply) | **B** → index.occurrence.containing_symbol_id typed hot path; optional extracted-result snapshot once validated |
| `kind` | `TEXT` NOT NULL | **B** → index.occurrence.kind typed hot path; optional extracted-result snapshot once validated |
| `start_byte` | `INTEGER` NOT NULL | **B** → index.occurrence.start_byte typed hot path; optional extracted-result snapshot once validated |
| `end_byte` | `INTEGER` NOT NULL | **B** → index.occurrence.end_byte typed hot path; optional extracted-result snapshot once validated |
| `start_line` | `INTEGER` NOT NULL | **B** → index.occurrence.start_line typed hot path; optional extracted-result snapshot once validated |
| `start_col` | `INTEGER` NOT NULL | **B** → index.occurrence.start_col typed hot path; optional extracted-result snapshot once validated |
| `end_line` | `INTEGER` NOT NULL | **B** → index.occurrence.end_line typed hot path; optional extracted-result snapshot once validated |
| `end_col` | `INTEGER` NOT NULL | **B** → index.occurrence.end_col typed hot path; optional extracted-result snapshot once validated |
| `relation_id` | `INTEGER` NULL permitted (PK rules apply) | **B** → index.occurrence.relation_id typed hot path; optional extracted-result snapshot once validated |
| `analysis_profile_id` | `INTEGER` NOT NULL | **B** → index.occurrence.analysis_profile_id + source/generation/profile revision bridge (no blind rewrites) |
| `resolution_context_id` | `INTEGER` NULL permitted (PK rules apply) | **B** → index.occurrence.resolution_context_id typed hot path; optional extracted-result snapshot once validated |
| `resource_revision` | `TEXT` NOT NULL | **B** → index.occurrence.resource_revision + source/generation/profile revision bridge (no blind rewrites) |
| `generation` | `INTEGER` NOT NULL | **B** → index.occurrence.generation + source/generation/profile revision bridge (no blind rewrites) |

**Row verification:**
original rows index.occurrence == migrated/preserved rows; FK-target parity for 6 local FK declarations; old PK mapping injective; preserve all 15 typed columns and NULLs; status B before cutover.

#### `index.relation` — 7 columns; PK `id`; local FK 3; unique indexes 3

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **B** → index.relation.id local; migration_id_map index-scoped key; never sole durable ID |
| `kind` | `TEXT` NOT NULL | **B** → index.relation.kind typed hot path; optional extracted-result snapshot once validated |
| `source_entity_id` | `INTEGER` NOT NULL | **B** → index.relation.source_entity_id typed hot path; optional extracted-result snapshot once validated |
| `target_entity_id` | `INTEGER` NOT NULL | **B** → index.relation.target_entity_id typed hot path; optional extracted-result snapshot once validated |
| `dispatch` | `TEXT` NULL permitted (PK rules apply) | **B** → index.relation.dispatch typed hot path; optional extracted-result snapshot once validated |
| `target_scope` | `TEXT` NULL permitted (PK rules apply) | **B** → index.relation.target_scope typed hot path; optional extracted-result snapshot once validated |
| `created_generation` | `INTEGER` NOT NULL | **B** → index.relation.created_generation + source/generation/profile revision bridge (no blind rewrites) |

**Row verification:**
original rows index.relation == migrated/preserved rows; FK-target parity for 3 local FK declarations; old PK mapping injective; preserve all 7 typed columns and NULLs; status B before cutover.

#### `index.relation_candidate` — 5 columns; PK `id`; local FK 2; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **R** → index.relation_candidate.id unchanged; classify retention and rebuild preconditions |
| `unresolved_reference_id` | `INTEGER` NOT NULL | **R** → index.relation_candidate.unresolved_reference_id unchanged; classify retention and rebuild preconditions |
| `target_entity_id` | `INTEGER` NOT NULL | **R** → index.relation_candidate.target_entity_id unchanged; classify retention and rebuild preconditions |
| `evidence_kind` | `TEXT` NOT NULL | **R** → index.relation_candidate.evidence_kind unchanged; classify retention and rebuild preconditions |
| `ordinal` | `INTEGER` NOT NULL | **R** → index.relation_candidate.ordinal unchanged; classify retention and rebuild preconditions |

**Row verification:**
original rows index.relation_candidate == migrated/preserved rows; FK-target parity for 2 local FK declarations; old PK mapping injective; preserve all 5 typed columns and NULLs; status R before cutover.

#### `index.resolution_context` — 10 columns; PK `id`; local FK 1; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **R** → index.resolution_context.id unchanged; classify retention and rebuild preconditions |
| `context_key` | `TEXT` NOT NULL | **R** → index.resolution_context.context_key unchanged; classify retention and rebuild preconditions |
| `language` | `TEXT` NOT NULL | **R** → index.resolution_context.language unchanged; classify retention and rebuild preconditions |
| `scope_key` | `TEXT` NOT NULL | **R** → index.resolution_context.scope_key unchanged; classify retention and rebuild preconditions |
| `config_fingerprint` | `TEXT` NOT NULL | **R** → index.resolution_context.config_fingerprint unchanged; classify retention and rebuild preconditions |
| `dependency_fingerprint` | `TEXT` NOT NULL | **R** → index.resolution_context.dependency_fingerprint unchanged; classify retention and rebuild preconditions |
| `environment_fingerprint` | `TEXT` NOT NULL | **R** → index.resolution_context.environment_fingerprint unchanged; classify retention and rebuild preconditions |
| `module_resolution_fingerprint` | `TEXT` NOT NULL | **R** → index.resolution_context.module_resolution_fingerprint unchanged; classify retention and rebuild preconditions |
| `backend_snapshot_token` | `TEXT` NULL permitted (PK rules apply) | **R** → index.resolution_context.backend_snapshot_token unchanged; classify retention and rebuild preconditions |
| `created_generation` | `INTEGER` NOT NULL | **R** → index.resolution_context.created_generation unchanged; classify retention and rebuild preconditions |

**Row verification:**
original rows index.resolution_context == migrated/preserved rows; FK-target parity for 1 local FK declarations; old PK mapping injective; preserve all 10 typed columns and NULLs; status R before cutover.

#### `index.resource` — 15 columns; PK `id`; local FK 1; unique indexes 2

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **B** → index.resource.id local; migration_id_map index-scoped key; never sole durable ID |
| `uid` | `BLOB` NOT NULL | **B** → index.resource.uid preserved; durable stable source/entity handle only after identity proof |
| `path_rel` | `TEXT` NOT NULL | **B** → index.resource.path_rel typed hot path; optional extracted-result snapshot once validated |
| `path_key` | `TEXT` NOT NULL | **B** → index.resource.path_key typed hot path; optional extracted-result snapshot once validated |
| `kind` | `TEXT` NOT NULL | **B** → index.resource.kind typed hot path; optional extracted-result snapshot once validated |
| `role` | `TEXT` NOT NULL | **B** → index.resource.role typed hot path; optional extracted-result snapshot once validated |
| `language` | `TEXT` NULL permitted (PK rules apply) | **B** → index.resource.language typed hot path; optional extracted-result snapshot once validated |
| `size` | `INTEGER` NOT NULL | **B** → index.resource.size typed hot path; optional extracted-result snapshot once validated |
| `mtime_ns` | `INTEGER` NOT NULL | **B** → index.resource.mtime_ns typed hot path; optional extracted-result snapshot once validated |
| `fingerprint` | `TEXT` NOT NULL | **B** → index.resource.fingerprint typed hot path; optional extracted-result snapshot once validated |
| `content_hash` | `TEXT` NULL permitted (PK rules apply) | **B** → index.resource.content_hash typed hot path; optional extracted-result snapshot once validated |
| `state` | `TEXT` NOT NULL | **B** → index.resource.state typed hot path; optional extracted-result snapshot once validated |
| `resource_revision` | `TEXT` NOT NULL | **B** → index.resource.resource_revision + source/generation/profile revision bridge (no blind rewrites) |
| `generated_kind` | `TEXT` NULL permitted (PK rules apply) | **B** → index.resource.generated_kind typed hot path; optional extracted-result snapshot once validated |
| `container_resource_id` | `INTEGER` NULL permitted (PK rules apply) | **B** → index.resource.container_resource_id typed hot path; optional extracted-result snapshot once validated |

**Row verification:**
original rows index.resource == migrated/preserved rows; FK-target parity for 1 local FK declarations; old PK mapping injective; preserve all 15 typed columns and NULLs; status B before cutover.

#### `index.semantic_conflict` — 9 columns; PK `id`; local FK 6; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **R** → index.semantic_conflict.id unchanged; classify retention and rebuild preconditions |
| `context_key` | `TEXT` NOT NULL | **R** → index.semantic_conflict.context_key unchanged; classify retention and rebuild preconditions |
| `occurrence_id` | `INTEGER` NOT NULL | **R** → index.semantic_conflict.occurrence_id unchanged; classify retention and rebuild preconditions |
| `relation_kind` | `TEXT` NOT NULL | **R** → index.semantic_conflict.relation_kind unchanged; classify retention and rebuild preconditions |
| `source_entity_id` | `INTEGER` NOT NULL | **R** → index.semantic_conflict.source_entity_id unchanged; classify retention and rebuild preconditions |
| `structural_target_entity_id` | `INTEGER` NOT NULL | **R** → index.semantic_conflict.structural_target_entity_id unchanged; classify retention and rebuild preconditions |
| `semantic_target_entity_id` | `INTEGER` NOT NULL | **R** → index.semantic_conflict.semantic_target_entity_id unchanged; classify retention and rebuild preconditions |
| `analysis_profile_id` | `INTEGER` NOT NULL | **R** → index.semantic_conflict.analysis_profile_id unchanged; classify retention and rebuild preconditions |
| `generation_id` | `INTEGER` NOT NULL | **R** → index.semantic_conflict.generation_id unchanged; classify retention and rebuild preconditions |

**Row verification:**
original rows index.semantic_conflict == migrated/preserved rows; FK-target parity for 6 local FK declarations; old PK mapping injective; preserve all 9 typed columns and NULLs; status R before cutover.

#### `index.semantic_evidence` — 12 columns; PK `id`; local FK 4; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **B** → index.semantic_evidence.id local; migration_id_map index-scoped key; never sole durable ID |
| `context_key` | `TEXT` NOT NULL | **B** → index.semantic_evidence.context_key typed hot path; optional extracted-result snapshot once validated |
| `occurrence_id` | `INTEGER` NOT NULL | **B** → index.semantic_evidence.occurrence_id typed hot path; optional extracted-result snapshot once validated |
| `relation_id` | `INTEGER` NOT NULL | **B** → index.semantic_evidence.relation_id typed hot path; optional extracted-result snapshot once validated |
| `capability` | `TEXT` NOT NULL | **B** → index.semantic_evidence.capability typed hot path; optional extracted-result snapshot once validated |
| `proof_role` | `TEXT` NOT NULL | **B** → index.semantic_evidence.proof_role typed hot path; optional extracted-result snapshot once validated |
| `analysis_profile_id` | `INTEGER` NOT NULL | **B** → index.semantic_evidence.analysis_profile_id + source/generation/profile revision bridge (no blind rewrites) |
| `generation_id` | `INTEGER` NOT NULL | **B** → index.semantic_evidence.generation_id + source/generation/profile revision bridge (no blind rewrites) |
| `displaced_intended_kind` | `TEXT` NULL permitted (PK rules apply) | **B** → index.semantic_evidence.displaced_intended_kind typed hot path; optional extracted-result snapshot once validated |
| `displaced_lookup_name` | `TEXT` NULL permitted (PK rules apply) | **B** → index.semantic_evidence.displaced_lookup_name typed hot path; optional extracted-result snapshot once validated |
| `displaced_module_hint` | `TEXT` NULL permitted (PK rules apply) | **B** → index.semantic_evidence.displaced_module_hint typed hot path; optional extracted-result snapshot once validated |
| `displaced_reason` | `TEXT` NULL permitted (PK rules apply) | **B** → index.semantic_evidence.displaced_reason typed hot path; optional extracted-result snapshot once validated |

**Row verification:**
original rows index.semantic_evidence == migrated/preserved rows; FK-target parity for 4 local FK declarations; old PK mapping injective; preserve all 12 typed columns and NULLs; status B before cutover.

#### `index.semantic_publication` — 13 columns; PK `id`; local FK 3; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **R** → index.semantic_publication.id unchanged; classify retention and rebuild preconditions |
| `context_key` | `TEXT` NOT NULL | **R** → index.semantic_publication.context_key unchanged; classify retention and rebuild preconditions |
| `owner_resource_id` | `INTEGER` NOT NULL | **R** → index.semantic_publication.owner_resource_id unchanged; classify retention and rebuild preconditions |
| `workspace_uid` | `BLOB` NOT NULL | **R** → index.semantic_publication.workspace_uid unchanged; classify retention and rebuild preconditions |
| `analysis_profile_id` | `INTEGER` NOT NULL | **R** → index.semantic_publication.analysis_profile_id unchanged; classify retention and rebuild preconditions |
| `generation_id` | `INTEGER` NOT NULL | **R** → index.semantic_publication.generation_id unchanged; classify retention and rebuild preconditions |
| `basis_workspace_revision` | `TEXT` NOT NULL | **R** → index.semantic_publication.basis_workspace_revision unchanged; classify retention and rebuild preconditions |
| `basis_fingerprint` | `TEXT` NOT NULL | **R** → index.semantic_publication.basis_fingerprint unchanged; classify retention and rebuild preconditions |
| `config_fingerprint` | `TEXT` NOT NULL | **R** → index.semantic_publication.config_fingerprint unchanged; classify retention and rebuild preconditions |
| `environment_fingerprint` | `TEXT` NOT NULL | **R** → index.semantic_publication.environment_fingerprint unchanged; classify retention and rebuild preconditions |
| `inventory_fingerprint` | `TEXT` NULL permitted (PK rules apply) | **R** → index.semantic_publication.inventory_fingerprint unchanged; classify retention and rebuild preconditions |
| `support` | `TEXT` NOT NULL | **R** → index.semantic_publication.support unchanged; classify retention and rebuild preconditions |
| `published_at` | `TEXT` NOT NULL | **R** → index.semantic_publication.published_at unchanged; classify retention and rebuild preconditions |

**Row verification:**
original rows index.semantic_publication == migrated/preserved rows; FK-target parity for 3 local FK declarations; old PK mapping injective; preserve all 13 typed columns and NULLs; status R before cutover.

#### `index.semantic_publication_source` — 3 columns; PK `publication_id`, `resource_id`; local FK 2; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `publication_id` **PK** | `INTEGER` NOT NULL | **R** → index.semantic_publication_source.publication_id unchanged; classify retention and rebuild preconditions |
| `resource_id` **PK** | `INTEGER` NOT NULL | **R** → index.semantic_publication_source.resource_id unchanged; classify retention and rebuild preconditions |
| `resource_revision` | `TEXT` NOT NULL | **R** → index.semantic_publication_source.resource_revision unchanged; classify retention and rebuild preconditions |

**Row verification:**
original rows index.semantic_publication_source == migrated/preserved rows; FK-target parity for 2 local FK declarations; old PK mapping injective; preserve all 3 typed columns and NULLs; status R before cutover.

#### `index.symbol` — 18 columns; PK `id`; local FK 3; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **B** → index.symbol.id local; migration_id_map index-scoped key; never sole durable ID |
| `uid` | `BLOB` NOT NULL | **B** → index.symbol.uid preserved; durable stable source/entity handle only after identity proof |
| `resource_id` | `INTEGER` NOT NULL | **B** → index.symbol.resource_id typed hot path; optional extracted-result snapshot once validated |
| `parent_symbol_id` | `INTEGER` NULL permitted (PK rules apply) | **B** → index.symbol.parent_symbol_id typed hot path; optional extracted-result snapshot once validated |
| `kind` | `TEXT` NOT NULL | **B** → index.symbol.kind typed hot path; optional extracted-result snapshot once validated |
| `name` | `TEXT` NOT NULL | **B** → index.symbol.name typed hot path; optional extracted-result snapshot once validated |
| `qualified_name` | `TEXT` NOT NULL | **B** → index.symbol.qualified_name typed hot path; optional extracted-result snapshot once validated |
| `signature` | `TEXT` NULL permitted (PK rules apply) | **B** → index.symbol.signature typed hot path; optional extracted-result snapshot once validated |
| `visibility` | `TEXT` NOT NULL | **B** → index.symbol.visibility typed hot path; optional extracted-result snapshot once validated |
| `exported` | `INTEGER` NOT NULL | **B** → index.symbol.exported typed hot path; optional extracted-result snapshot once validated |
| `start_byte` | `INTEGER` NOT NULL | **B** → index.symbol.start_byte typed hot path; optional extracted-result snapshot once validated |
| `end_byte` | `INTEGER` NOT NULL | **B** → index.symbol.end_byte typed hot path; optional extracted-result snapshot once validated |
| `start_line` | `INTEGER` NOT NULL | **B** → index.symbol.start_line typed hot path; optional extracted-result snapshot once validated |
| `start_col` | `INTEGER` NOT NULL | **B** → index.symbol.start_col typed hot path; optional extracted-result snapshot once validated |
| `end_line` | `INTEGER` NOT NULL | **B** → index.symbol.end_line typed hot path; optional extracted-result snapshot once validated |
| `end_col` | `INTEGER` NOT NULL | **B** → index.symbol.end_col typed hot path; optional extracted-result snapshot once validated |
| `resource_revision` | `TEXT` NOT NULL | **B** → index.symbol.resource_revision + source/generation/profile revision bridge (no blind rewrites) |
| `analysis_profile_id` | `INTEGER` NOT NULL | **B** → index.symbol.analysis_profile_id + source/generation/profile revision bridge (no blind rewrites) |

**Row verification:**
original rows index.symbol == migrated/preserved rows; FK-target parity for 3 local FK declarations; old PK mapping injective; preserve all 18 typed columns and NULLs; status B before cutover.

#### `index.unresolved_reference` — 8 columns; PK `id`; local FK 2; unique indexes 1

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **R** → index.unresolved_reference.id unchanged; classify retention and rebuild preconditions |
| `occurrence_id` | `INTEGER` NOT NULL | **R** → index.unresolved_reference.occurrence_id unchanged; classify retention and rebuild preconditions |
| `intended_relation_kind` | `TEXT` NOT NULL | **R** → index.unresolved_reference.intended_relation_kind unchanged; classify retention and rebuild preconditions |
| `lookup_name` | `TEXT` NOT NULL | **R** → index.unresolved_reference.lookup_name unchanged; classify retention and rebuild preconditions |
| `module_hint` | `TEXT` NULL permitted (PK rules apply) | **R** → index.unresolved_reference.module_hint unchanged; classify retention and rebuild preconditions |
| `reason` | `TEXT` NOT NULL | **R** → index.unresolved_reference.reason unchanged; classify retention and rebuild preconditions |
| `resolution_context_id` | `INTEGER` NULL permitted (PK rules apply) | **R** → index.unresolved_reference.resolution_context_id unchanged; classify retention and rebuild preconditions |
| `candidate_truncated` | `INTEGER` NOT NULL | **R** → index.unresolved_reference.candidate_truncated unchanged; classify retention and rebuild preconditions |

**Row verification:**
original rows index.unresolved_reference == migrated/preserved rows; FK-target parity for 2 local FK declarations; old PK mapping injective; preserve all 8 typed columns and NULLs; status R before cutover.

#### `index.workspace_clock` — 7 columns; PK `id`; local FK 1; unique indexes 0

| Source column | SQL type / null | Target / transformation |
|---|---|---|
| `id` **PK** | `INTEGER` NULL permitted (PK rules apply) | **R** → index.workspace_clock.id unchanged; classify retention and rebuild preconditions |
| `current_workspace_revision` | `TEXT` NOT NULL | **R** → index.workspace_clock.current_workspace_revision unchanged; classify retention and rebuild preconditions |
| `stable_generation_id` | `INTEGER` NULL permitted (PK rules apply) | **R** → index.workspace_clock.stable_generation_id unchanged; classify retention and rebuild preconditions |
| `last_change_seq` | `INTEGER` NOT NULL | **R** → index.workspace_clock.last_change_seq unchanged; classify retention and rebuild preconditions |
| `last_reconcile_seq` | `INTEGER` NOT NULL | **R** → index.workspace_clock.last_reconcile_seq unchanged; classify retention and rebuild preconditions |
| `last_full_reconcile_at` | `TEXT` NULL permitted (PK rules apply) | **R** → index.workspace_clock.last_full_reconcile_at unchanged; classify retention and rebuild preconditions |
| `watcher_continuity_state` | `TEXT` NOT NULL | **R** → index.workspace_clock.watcher_continuity_state unchanged; classify retention and rebuild preconditions |

**Row verification:**
original rows index.workspace_clock == migrated/preserved rows; FK-target parity for 1 local FK declarations; old PK mapping injective; preserve all 7 typed columns and NULLs; status R before cutover.

## SQL implementation guidance (candidate, not the production migration)

1. One owner DB transaction contains `knowledge_revision` + `knowledge_event` + evidence bindings + CAS update `knowledge_head`; no global/project/workspace wide atomicity is promised.
2. Nullable scope/branch/natural-key columns must **not** rely on ordinary `UNIQUE` over NULLs. Use normalized non-null keys or versioned canonical identity/validity keys and regression fixtures. `uid` BLOB length check is mandatory.
3. `knowledge_event(item_id,revision_no)` must reference a revision of the **same item**, and `knowledge_head` must refer to a matching revision/event pair. Use composite FK and CAS version checks. No event should be invented retroactively.
4. New relational PKs stay internal. For transferred links, store `(source_db, source_table, original_pk)` mapping explicitly; validate the mapping before changing any reference.
5. Existing code relations retain `(kind,source,target)` identity; an author-approved generic relation is a revisioned knowledge item, possibly with multiple evidence records. Do **not** substitute vector similarity for proof.
6. Define and test an explicit shadow-read parity contract for legacy `Policy`, `Decision`, `Blueprint`, state, promotions and selected source analysis; keep old stores read-only for rollback before destructive migration.
7. `knowledge_validity` and narrative scope are **opt-in domain adapters**, not required columns in hot code tables.
8. The candidate SQL is a **standalone module** for a disposable SQLite DB. It omits existing `db_meta`, migration checksums, ownership bootstrap, and application enforcement for ACL, cross-owner refs, semantic payload types and provenance. Those must be implemented and tested before adoption.

## Gate matrix

| Gate | Now |
|---|---|
| 25 actual source SQL migrations in disposable SQLite | user report PASS 25/25; not compiled Rust or populated migration |
| Source table names / columns tracked | 453 of 453 physical fields across 46 business tables have a destination **proposal** below |
| Concrete durable schema candidate | available as companion SQL; syntax/negative constraints must be smoke-tested, not shipped |
| Lossless ETL implementation, dual-read parity | NOT DONE |
| Existing historical approvals reconstructed | NOT POSSIBLE without underlying event records; import snapshot only |
| Existing live DB duplicate/UID audit | NOT DONE; avoid production-data claims |
| A/B/C file layouts and optional vector S0–S4 benchmarks | NOT DONE; no selection |
| Release gate and production migration | NOT AUTHORIZED |

## Source/worktree scope and known limitations

- Audited source HEAD: `231843185d4c407dc73cfd8acceb272add97835b`, **dirty worktree**. Schema source SHA256: `global=cc027ce508376532849c7401b6ecc47fa0bb104cb3754160109bffc8b2f1888b`; `project=6bf81614b01f3eb11f071ca6b39dcb88a3078d256f17ee54083956c0d4da7782`; `workspace=b1bc8f5f582ebf2bfbd852282c48d675be163c96cc49d6ca00ec8489b579ba24`; `index=35e932f614da45c51458521d34ac6335d354df094bd3f6842fde8879a5258a5e`.
- The column targets above are **field-level design paths**, not verified transforms. Every B/R field stays in legacy typed storage until historical-replay/evidence retention semantics are settled.
- A prior v0 conceptual proposal refers to `source_ref` and `index.resource`. Those must not become independent canonical owners of the same physical asset. The bridge is a verified stable resource handle + immutable source revision, with source locator treated as mutable.
- The compact SQL prototype is intentionally not equivalent to a full domain-specific `Policy` schema. Resolver/priority/protection and query indexing must be separately benchmarked. Compiled Rust tests, populated migrations, ACL/retention tests and token economics are still required.
