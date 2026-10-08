-- Brainprint 0.3.0: candidate durable knowledge schema — DESIGN ONLY (#94).
-- SOURCE OF TRUTH: docs/design/storage-v03-field-map.md and current code contracts.
-- This file creates an additive module on a DISPOSABLE SQLite database.
-- It is not a schema migration, not a replacement of the 46 current business tables.
-- The application must enforce owner ACL, typed knowledge payloads, scope precedence,
-- CAS approvals, cross-file references, import row parity, and provenance.
-- Tested separately against SQLite 3.46.1 (syntax/negative constraint fixtures).
PRAGMA foreign_keys = ON;

CREATE TABLE knowledge_scope (
  id INTEGER PRIMARY KEY,
  uid BLOB NOT NULL UNIQUE CHECK(typeof(uid)='blob' AND length(uid)=16),
  scope_kind TEXT NOT NULL CHECK(length(scope_kind)>0),
  scope_key TEXT NOT NULL,
  parent_scope_id INTEGER REFERENCES knowledge_scope(id),
  UNIQUE(scope_kind, scope_key),
  CHECK(parent_scope_id IS NULL OR parent_scope_id<>id)
);

CREATE TABLE source_ref (
  id INTEGER PRIMARY KEY,
  uid BLOB NOT NULL UNIQUE CHECK(typeof(uid)='blob' AND length(uid)=16),
  source_kind TEXT NOT NULL,
  current_locator TEXT,
  availability TEXT NOT NULL CHECK(availability IN ('PRESENT','MISSING','UNAVAILABLE','REDACTED','UNKNOWN')),
  classification TEXT NOT NULL DEFAULT 'PRIVATE',
  first_observed_at TEXT NOT NULL
);

CREATE TABLE source_revision (
  id INTEGER PRIMARY KEY,
  source_id INTEGER NOT NULL REFERENCES source_ref(id),
  revision_key TEXT NOT NULL,
  content_hash TEXT,
  observed_at TEXT NOT NULL,
  analyzer_profile TEXT,
  availability_at_observation TEXT NOT NULL,
  UNIQUE(source_id,revision_key)
);

CREATE TABLE observation (
  id INTEGER PRIMARY KEY,
  uid BLOB NOT NULL UNIQUE CHECK(typeof(uid)='blob' AND length(uid)=16),
  source_revision_id INTEGER NOT NULL REFERENCES source_revision(id),
  segment_key TEXT NOT NULL,
  observation_kind TEXT NOT NULL,
  extractor_profile TEXT NOT NULL,
  payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
  payload_sha256 TEXT NOT NULL,
  observed_at TEXT NOT NULL,
  UNIQUE(source_revision_id,segment_key,observation_kind,extractor_profile)
);

CREATE TABLE knowledge_item (
  id INTEGER PRIMARY KEY,
  uid BLOB NOT NULL UNIQUE CHECK(typeof(uid)='blob' AND length(uid)=16),
  kind TEXT NOT NULL,
  owner_kind TEXT NOT NULL CHECK(owner_kind IN ('USER','PROJECT')),
  owner_key TEXT NOT NULL,
  stable_key TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(owner_kind,owner_key,kind,stable_key)
);

CREATE TABLE knowledge_revision (
  id INTEGER PRIMARY KEY,
  item_id INTEGER NOT NULL REFERENCES knowledge_item(id),
  revision_no INTEGER NOT NULL CHECK(revision_no>=1),
  previous_revision_no INTEGER,
  payload_type TEXT NOT NULL,
  payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
  content_sha256 TEXT NOT NULL,
  epistemic_basis TEXT NOT NULL CHECK(epistemic_basis IN
    ('USER_EXPLICIT','AUTHORITATIVE_ARTIFACT','DETERMINISTIC','OBSERVED','UNVERIFIED','LEGACY_IMPORTED')),
  recorded_at TEXT NOT NULL,
  UNIQUE(item_id,revision_no),
  FOREIGN KEY(item_id,previous_revision_no) REFERENCES knowledge_revision(item_id,revision_no),
  CHECK(previous_revision_no IS NULL OR previous_revision_no<revision_no)
);

CREATE TABLE knowledge_event (
  id INTEGER PRIMARY KEY,
  uid BLOB NOT NULL UNIQUE CHECK(typeof(uid)='blob' AND length(uid)=16),
  item_id INTEGER NOT NULL,
  revision_no INTEGER NOT NULL,
  event_kind TEXT NOT NULL CHECK(event_kind IN
    ('LEGACY_IMPORT','PROPOSE','APPROVE','REJECT','SUPERSEDE','REVOKE','REVALIDATE')),
  actor_ref TEXT NOT NULL,
  expected_head_version INTEGER NOT NULL CHECK(expected_head_version>=0),
  request_uid BLOB NOT NULL UNIQUE CHECK(typeof(request_uid)='blob' AND length(request_uid)=16),
  recorded_at TEXT NOT NULL,
  reason TEXT,
  UNIQUE(item_id,revision_no,id),
  FOREIGN KEY(item_id,revision_no) REFERENCES knowledge_revision(item_id,revision_no)
);

-- Mutable projection/head; not a second authority. Compare-and-swap expected
-- version in the *same owner DB transaction* as new revision/event/evidence.
CREATE TABLE knowledge_head (
  item_id INTEGER PRIMARY KEY REFERENCES knowledge_item(id),
  revision_no INTEGER NOT NULL,
  last_event_id INTEGER NOT NULL REFERENCES knowledge_event(id),
  version_no INTEGER NOT NULL DEFAULT 1 CHECK(version_no>=1),
  lifecycle_state TEXT NOT NULL,
  FOREIGN KEY(item_id,revision_no) REFERENCES knowledge_revision(item_id,revision_no),
  FOREIGN KEY(item_id,revision_no,last_event_id) REFERENCES knowledge_event(item_id,revision_no,id)
);

CREATE TABLE knowledge_evidence (
  id INTEGER PRIMARY KEY,
  revision_id INTEGER NOT NULL REFERENCES knowledge_revision(id),
  evidence_key TEXT NOT NULL,
  source_revision_id INTEGER REFERENCES source_revision(id),
  observation_id INTEGER REFERENCES observation(id),
  authorization_event_id INTEGER REFERENCES knowledge_event(id),
  external_locator TEXT,
  segment_start INTEGER,
  segment_end INTEGER,
  segment_hash TEXT,
  role TEXT NOT NULL,
  UNIQUE(revision_id,evidence_key),
  CHECK((source_revision_id IS NOT NULL)+(observation_id IS NOT NULL)
    +(authorization_event_id IS NOT NULL)+(external_locator IS NOT NULL)=1),
  CHECK(segment_start IS NULL OR (segment_start>=0 AND segment_end IS NOT NULL AND segment_end>=segment_start))
);

CREATE TABLE knowledge_scope_binding (
  revision_id INTEGER NOT NULL REFERENCES knowledge_revision(id),
  scope_id INTEGER NOT NULL REFERENCES knowledge_scope(id),
  dimension TEXT NOT NULL,
  PRIMARY KEY(revision_id,dimension,scope_id)
);

-- Versioned adapter interprets story time, branch, perspective and any
-- nonchronological range. NULL-aware uniqueness uses nonnullable validity_key.
CREATE TABLE knowledge_validity (
  id INTEGER PRIMARY KEY,
  revision_id INTEGER NOT NULL REFERENCES knowledge_revision(id),
  domain_axis TEXT NOT NULL,
  branch_key TEXT,
  start_key TEXT,
  end_key TEXT,
  perspective_key TEXT,
  axis_schema_version TEXT NOT NULL,
  validity_key TEXT NOT NULL,
  UNIQUE(revision_id,validity_key)
);

CREATE TABLE knowledge_link (
  id INTEGER PRIMARY KEY,
  from_item_id INTEGER NOT NULL REFERENCES knowledge_item(id),
  target_local_item_id INTEGER REFERENCES knowledge_item(id),
  target_owner_kind TEXT,
  target_external_uid BLOB CHECK(target_external_uid IS NULL OR
    (typeof(target_external_uid)='blob' AND length(target_external_uid)=16)),
  link_kind TEXT NOT NULL,
  recorded_at TEXT NOT NULL,
  CHECK((target_local_item_id IS NOT NULL)<>(target_external_uid IS NOT NULL)),
  CHECK(target_external_uid IS NULL OR target_owner_kind IS NOT NULL)
);

CREATE UNIQUE INDEX idx_knowledge_link_local_unique
  ON knowledge_link(from_item_id,target_local_item_id,link_kind)
  WHERE target_local_item_id IS NOT NULL;
CREATE UNIQUE INDEX idx_knowledge_link_external_unique
  ON knowledge_link(from_item_id,target_owner_kind,target_external_uid,link_kind)
  WHERE target_external_uid IS NOT NULL;

-- Typed states stay separate from human-approved canon/policy.
CREATE TABLE state_fact (
  id INTEGER PRIMARY KEY,
  uid BLOB NOT NULL UNIQUE CHECK(typeof(uid)='blob' AND length(uid)=16),
  owner_kind TEXT NOT NULL CHECK(owner_kind IN ('PROJECT','WORKSPACE')),
  owner_key TEXT NOT NULL,
  scope_kind TEXT NOT NULL,
  scope_key TEXT NOT NULL,
  state_key TEXT NOT NULL,
  value_type TEXT NOT NULL,
  value_json TEXT NOT NULL CHECK(json_valid(value_json)),
  lifecycle_state TEXT NOT NULL,
  source_kind TEXT NOT NULL,
  source_locator TEXT,
  observed_revision TEXT,
  legacy_updated_at TEXT NOT NULL,
  UNIQUE(owner_kind,owner_key,scope_kind,scope_key,state_key)
);

CREATE TABLE knowledge_relation_revision (
  revision_id INTEGER PRIMARY KEY REFERENCES knowledge_revision(id),
  subject_entity_uid BLOB NOT NULL CHECK(typeof(subject_entity_uid)='blob' AND length(subject_entity_uid)=16),
  predicate TEXT NOT NULL,
  object_entity_uid BLOB CHECK(object_entity_uid IS NULL OR
    (typeof(object_entity_uid)='blob' AND length(object_entity_uid)=16)),
  object_literal_json TEXT CHECK(object_literal_json IS NULL OR json_valid(object_literal_json)),
  CHECK((object_entity_uid IS NOT NULL)<>(object_literal_json IS NOT NULL))
);

CREATE TABLE migration_id_map (
  id INTEGER PRIMARY KEY,
  source_db_kind TEXT NOT NULL,
  source_table TEXT NOT NULL,
  source_key_json TEXT NOT NULL CHECK(json_valid(source_key_json)),
  source_row_sha256 TEXT NOT NULL,
  target_kind TEXT NOT NULL,
  target_item_uid BLOB CHECK(target_item_uid IS NULL OR
    (typeof(target_item_uid)='blob' AND length(target_item_uid)=16)),
  migrated_at TEXT NOT NULL,
  UNIQUE(source_db_kind,source_table,source_key_json)
);

CREATE INDEX idx_knowledge_revision_item_desc ON knowledge_revision(item_id,revision_no DESC);
CREATE INDEX idx_knowledge_event_item_rev ON knowledge_event(item_id,revision_no,id);
CREATE INDEX idx_knowledge_head_lifecycle ON knowledge_head(lifecycle_state,item_id);
CREATE INDEX idx_knowledge_evidence_source ON knowledge_evidence(source_revision_id);
CREATE INDEX idx_observation_source_rev ON observation(source_revision_id,segment_key);
CREATE INDEX idx_knowledge_scope_binding_scope ON knowledge_scope_binding(scope_id,dimension,revision_id);
CREATE INDEX idx_knowledge_link_from_kind ON knowledge_link(from_item_id,link_kind);
CREATE INDEX idx_knowledge_link_target_local ON knowledge_link(target_local_item_id);
CREATE INDEX idx_knowledge_relation_subject_pred ON knowledge_relation_revision(subject_entity_uid,predicate);
CREATE INDEX idx_knowledge_relation_pred_object ON knowledge_relation_revision(predicate,object_entity_uid);

-- NOT IMPLEMENTED BY THIS SQL:
-- immutable-event UPDATE/DELETE prevention; ACL; typed JSON interpretation;
-- content hashes/digest verification; same-item previous revision and evidence
-- semantic consistency beyond FKs; story-axis adapter logic; cross-store FK;
-- populated-data ETL; approval CAS transaction code; negative/migration benches.
