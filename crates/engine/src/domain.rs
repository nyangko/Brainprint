//! Cheap domain relations: `USES_ENV` and `USES_CONFIG` (#17 task 12).
//!
//! An environment or configuration key is one of the few things an
//! Agent asks about that the syntax already settles: `os.getenv("X")`
//! is not a guess about `X`, it is a statement about it. This extractor
//! turns exactly those statements into canonical relations from the
//! code that reads the key to a [`DomainEntity`] for the key itself.
//!
//! ## A string literal is not a key
//!
//! Recognition starts at the **API**, never at the string. A relation
//! is emitted only when the surrounding syntax names an access whose
//! identity is structurally certain -- `process.env.X`,
//! `os.environ["X"]`, `std::env::var("X")`,
//! `Environment.GetEnvironmentVariable("X")`,
//! `ConfigurationManager.AppSettings["X"]`. A bare `"DATABASE_URL"` in
//! a log line, a `foo.get("KEY")` whose receiver is only *named*
//! `config`, a dictionary lookup that happens to contain a shouty
//! string: none of those are evidence, and none of them produce
//! anything here.
//!
//! The config side is deliberately narrow. Under the current structural
//! tier only C#'s `ConfigurationManager` stores have an API identity
//! that syntax alone establishes, so that is all that is claimed.
//! Under-resolving is the contract; inventing framework semantics is
//! not (#17 "USES_ENV / USES_CONFIG").
//!
//! ## Static keys only
//!
//! `os.getenv(name)` is recognized as an access and then deliberately
//! dropped: there is no key to point at, and a `DomainEntity` called
//! `"unknown"` would be a lie with an identity. It comes back as
//! [`KeyLiteral::Dynamic`] so that a caller can report coverage, and it
//! is written nowhere -- the unresolved vocabulary (#17 task 7) has no
//! honest reason code for "the key is an expression", and this task is
//! not the place to invent one.
//!
//! ## What the relation means
//!
//! `code uses key X`. Not that X is defined, valid, or has a value
//! anywhere. Nothing here reads a `.env` file, a config file, or a
//! runtime value, and no key needs to be declared elsewhere for the
//! usage to be a fact.

use brainprint_core::ResourceId;
use tree_sitter::Node;

use crate::{
    extract::span_of,
    graph::{DomainEntity, GraphEndpoint, Relation, RelationKind},
    parser::{ParseTree, ParserDialect, SourceSpan},
    resolution::Dispatch,
    symbol::{Occurrence, OccurrenceKind},
};

/// Which domain a key belongs to. The two are separate identities: ENV
/// `DATABASE_URL` and CONFIG `DATABASE_URL` are different things that
/// happen to share a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainKind {
    Env,
    Config,
}

impl DomainKind {
    /// The `domain_entity.kind` this is stored as.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Env => "ENV",
            Self::Config => "CONFIG",
        }
    }

    #[must_use]
    pub const fn relation_kind(self) -> RelationKind {
        match self {
            Self::Env => RelationKind::UsesEnv,
            Self::Config => RelationKind::UsesConfig,
        }
    }
}

/// The recognized access that makes a literal a key.
///
/// Each variant is an API whose identity the syntax establishes on its
/// own. There is no variant for "a method called get".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyApi {
    /// Python `os.getenv("K")` / `os.environ.get("K")`.
    PythonOsGetenv,
    /// Python `os.environ["K"]`.
    PythonOsEnviron,
    /// JS/TS `process.env.K`.
    JsProcessEnvMember,
    /// JS/TS `process.env["K"]`.
    JsProcessEnvIndex,
    /// Rust `std::env::var("K")` / `env::var("K")`, and the `_os`
    /// variant.
    RustEnvVar,
    /// C# `Environment.GetEnvironmentVariable("K")`.
    CSharpEnvironment,
    /// C# `ConfigurationManager.AppSettings["K"]`.
    CSharpAppSettings,
    /// C# `ConfigurationManager.ConnectionStrings["K"]`.
    CSharpConnectionStrings,
}

impl KeyApi {
    #[must_use]
    pub const fn domain(self) -> DomainKind {
        match self {
            Self::PythonOsGetenv
            | Self::PythonOsEnviron
            | Self::JsProcessEnvMember
            | Self::JsProcessEnvIndex
            | Self::RustEnvVar
            | Self::CSharpEnvironment => DomainKind::Env,
            Self::CSharpAppSettings | Self::CSharpConnectionStrings => DomainKind::Config,
        }
    }

    /// The store the key lives in, when the API names one. Part of the
    /// entity's natural key: an app setting and a connection string
    /// with the same name are different keys.
    #[must_use]
    pub const fn namespace(self) -> Option<&'static str> {
        match self {
            Self::CSharpAppSettings => Some("AppSettings"),
            Self::CSharpConnectionStrings => Some("ConnectionStrings"),
            _ => None,
        }
    }
}

/// The key an access names, when it names one statically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyLiteral {
    /// Written in the source, exactly as stored.
    Static(String),
    /// An expression. No entity, no relation -- a coverage fact only.
    Dynamic,
}

/// One recognized environment or configuration access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyAccess {
    pub api: KeyApi,
    /// The key literal's own span for a static key, and the key
    /// expression's for a dynamic one. Never the whole call or file.
    pub span: SourceSpan,
    pub key: KeyLiteral,
}

impl KeyAccess {
    #[must_use]
    pub const fn domain(&self) -> DomainKind {
        self.api.domain()
    }

    /// The canonical target, for a statically known key.
    ///
    /// Identity is `(kind, key, namespace, method)` -- the existing
    /// `domain_entity` natural key. No source text and no row id take
    /// part in it.
    #[must_use]
    pub fn entity(&self) -> Option<DomainEntity> {
        let KeyLiteral::Static(key) = &self.key else {
            return None;
        };
        let domain = self.domain();
        Some(DomainEntity {
            kind: domain.as_str().to_owned(),
            normalized_identity: key.clone(),
            namespace: self.api.namespace().map(ToOwned::to_owned),
            method: None,
            display_label: match self.api.namespace() {
                Some(namespace) => format!("{} {namespace}:{key}", domain.as_str()),
                None => format!("{} {key}", domain.as_str()),
            },
        })
    }
}

/// Every environment and configuration access one parse tree states, in
/// source order.
///
/// The spans are exactly the ones #16 task 9's walker publishes as
/// `KEY_SITE` Occurrences -- both use [`key_accesses_at`], so evidence
/// and relations cannot disagree about where a key sits.
#[must_use]
pub fn extract_key_accesses(tree: &ParseTree, source: &[u8]) -> Vec<KeyAccess> {
    if !tree.descriptor().capability.covers_whole_file() {
        // A container-only Resource states nothing here. Zero accesses
        // is coverage, not a finding.
        return Vec::new();
    }
    let dialect = tree.descriptor().dialect;
    let mut found = Vec::new();
    walk(tree.syntax_tree().root_node(), &mut |node| {
        found.extend(key_accesses_at(node, dialect, source));
    });
    found.sort_by_key(|access| access.span.start_byte);
    found
}

/// The accesses one node states, if any. Shared with the structural
/// extractor so that every `KEY_SITE` has the same span as the relation
/// evidence bound to it.
pub(crate) fn key_accesses_at(
    node: Node<'_>,
    dialect: ParserDialect,
    source: &[u8],
) -> Vec<KeyAccess> {
    let mut found = Vec::new();
    match (dialect, node.kind()) {
        // --- Python: os.getenv / os.environ.get / os.environ[...]
        (ParserDialect::Python, "call") => {
            let Some(function) = node.child_by_field_name("function") else {
                return found;
            };
            let api = match dotted(function, source).as_deref() {
                Some("os.getenv" | "os.environ.get") => KeyApi::PythonOsGetenv,
                _ => return found,
            };
            if let Some(argument) = first_argument(node, "arguments") {
                found.push(access(api, argument, source));
            }
        }
        (ParserDialect::Python, "subscript") => {
            let Some(value) = node.child_by_field_name("value") else {
                return found;
            };
            if dotted(value, source).as_deref() != Some("os.environ") {
                return found;
            }
            if let Some(index) = node.child_by_field_name("subscript") {
                found.push(access(KeyApi::PythonOsEnviron, index, source));
            }
        }
        // --- JS/TS: process.env.KEY and process.env["KEY"]
        (
            ParserDialect::JavaScript
            | ParserDialect::Jsx
            | ParserDialect::TypeScript
            | ParserDialect::Tsx,
            "member_expression",
        ) => {
            let Some(object) = node.child_by_field_name("object") else {
                return found;
            };
            if dotted(object, source).as_deref() != Some("process.env") {
                return found;
            }
            if let Some(property) = node.child_by_field_name("property") {
                // The property *is* the key, written as an identifier.
                found.push(KeyAccess {
                    api: KeyApi::JsProcessEnvMember,
                    span: span_of(property),
                    key: KeyLiteral::Static(text(property, source)),
                });
            }
        }
        (
            ParserDialect::JavaScript
            | ParserDialect::Jsx
            | ParserDialect::TypeScript
            | ParserDialect::Tsx,
            "subscript_expression",
        ) => {
            let Some(object) = node.child_by_field_name("object") else {
                return found;
            };
            if dotted(object, source).as_deref() != Some("process.env") {
                return found;
            }
            if let Some(index) = node.child_by_field_name("index") {
                found.push(access(KeyApi::JsProcessEnvIndex, index, source));
            }
        }
        // --- Rust: std::env::var("K") and env::var("K")
        (ParserDialect::Rust, "call_expression") => {
            let Some(function) = node.child_by_field_name("function") else {
                return found;
            };
            let path = text(function, source);
            if !matches!(
                path.as_str(),
                "std::env::var" | "env::var" | "std::env::var_os" | "env::var_os"
            ) {
                return found;
            }
            if let Some(argument) = first_argument(node, "arguments") {
                found.push(access(KeyApi::RustEnvVar, argument, source));
            }
        }
        // --- C#: Environment.GetEnvironmentVariable("K")
        (ParserDialect::CSharp, "invocation_expression") => {
            let Some(function) = node.child_by_field_name("function") else {
                return found;
            };
            if dotted(function, source).as_deref() != Some("Environment.GetEnvironmentVariable") {
                return found;
            }
            if let Some(argument) = first_argument(node, "arguments") {
                found.push(access(KeyApi::CSharpEnvironment, argument, source));
            }
        }
        // --- C#: ConfigurationManager.AppSettings["K"] and
        //     ConfigurationManager.ConnectionStrings["K"]. The only
        //     config API this tier can name with certainty.
        (ParserDialect::CSharp, "element_access_expression") => {
            let Some(expression) = node.child_by_field_name("expression") else {
                return found;
            };
            let api = match dotted(expression, source).as_deref() {
                Some("ConfigurationManager.AppSettings") => KeyApi::CSharpAppSettings,
                Some("ConfigurationManager.ConnectionStrings") => KeyApi::CSharpConnectionStrings,
                _ => return found,
            };
            if let Some(argument) = first_argument(node, "subscript") {
                found.push(access(api, argument, source));
            }
        }
        _ => {}
    }
    found
}

/// The relations a set of accesses states, ready for the task 3
/// Resource-owned replacement.
///
/// The source endpoint follows the same rule as every other structural
/// relation: the Symbol the evidence sits in when there is one, the
/// owning Resource otherwise. No module-level Symbol is invented to
/// host a key access.
#[must_use]
pub fn domain_relations(
    owner: ResourceId,
    occurrences: &[Occurrence],
    accesses: &[KeyAccess],
    created_generation: i64,
) -> Vec<(SourceSpan, Relation)> {
    accesses
        .iter()
        .filter_map(|access| {
            // A dynamic key has no entity, so it has no edge.
            let target = GraphEndpoint::Domain(access.entity()?);
            let containing = occurrences
                .iter()
                .find(|occurrence| {
                    occurrence.kind == OccurrenceKind::KeySite
                        && occurrence.span.start_byte == access.span.start_byte
                        && occurrence.span.end_byte == access.span.end_byte
                })
                .and_then(|occurrence| occurrence.containing_symbol_id);
            let source = containing.map_or(GraphEndpoint::Resource(owner), GraphEndpoint::Symbol);
            Some((
                access.span,
                Relation {
                    kind: access.domain().relation_kind(),
                    source,
                    target,
                    // The access is written where it is read: there is
                    // no dispatch to observe.
                    dispatch: Dispatch::Static,
                    created_generation,
                },
            ))
        })
        .collect()
}

/// One access from a key argument node, static or not.
fn access(api: KeyApi, key: Node<'_>, source: &[u8]) -> KeyAccess {
    KeyAccess {
        api,
        span: span_of(key),
        key: string_literal(key, source).map_or(KeyLiteral::Dynamic, KeyLiteral::Static),
    }
}

/// The key a node names, if it is a plain static string.
///
/// Plain means exactly that: one pair of quotes around text with no
/// escape and no interpolation. An f-string, a template literal, a raw
/// or verbatim string, or anything with a backslash in it is left to be
/// a dynamic key rather than decoded here -- unescaping is a language
/// semantics question, and getting it subtly wrong would produce a
/// confident, wrong identity.
fn string_literal(node: Node<'_>, source: &[u8]) -> Option<String> {
    if !matches!(node.kind(), "string" | "string_literal") {
        return None;
    }
    let raw = text(node, source);
    let mut characters = raw.chars();
    let open = characters.next()?;
    let close = characters.next_back()?;
    if !matches!(open, '"' | '\'') || close != open || raw.len() < 2 {
        return None;
    }
    let inner = &raw[open.len_utf8()..raw.len() - close.len_utf8()];
    if inner.is_empty() || inner.contains('\\') || inner.contains(open) {
        return None;
    }
    Some(inner.to_owned())
}

/// The first argument of a call, by the field its grammar uses.
fn first_argument<'a>(node: Node<'a>, field: &str) -> Option<Node<'a>> {
    let arguments = node.child_by_field_name(field)?;
    let mut cursor = arguments.walk();
    let first = arguments.named_children(&mut cursor).next()?;
    // C# wraps each argument in an `argument` node.
    if first.kind() == "argument" {
        let mut inner = first.walk();
        return first.named_children(&mut inner).next();
    }
    Some(first)
}

/// A dotted access path written as plain names, or `None` if anything
/// in it is an expression.
///
/// This is what makes recognition structural: `os.environ` matches only
/// when it is literally the name `os` and the name `environ`, never a
/// variable that happens to be called `os`... which this tier cannot
/// tell apart, and which is why the set of recognized paths is a short
/// closed list of well-known APIs rather than a shape.
fn dotted(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "property_identifier" => Some(text(node, source)),
        "attribute" | "member_expression" | "member_access_expression" => {
            let object = node
                .child_by_field_name("object")
                .or_else(|| node.child_by_field_name("expression"))?;
            let member = node
                .child_by_field_name("attribute")
                .or_else(|| node.child_by_field_name("property"))
                .or_else(|| node.child_by_field_name("name"))?;
            Some(format!(
                "{}.{}",
                dotted(object, source)?,
                dotted(member, source)?
            ))
        }
        _ => None,
    }
}

fn text(node: Node<'_>, source: &[u8]) -> String {
    String::from_utf8_lossy(&source[node.byte_range()]).into_owned()
}

/// Visit every named node, outermost first.
fn walk(node: Node<'_>, visit: &mut impl FnMut(Node<'_>)) {
    visit(node);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, visit);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{
        config::WorkspaceConfig,
        evidence::{EvidenceError, OccurrenceRef, RelationEvidence, replace_resource_evidence},
        generation::{self, PublicationGrant},
        graph::{GraphStore, RelationKey},
        parser::{ParserRegistry, SourceBasis, dialect_for_path},
        relations::{Direction, RelationIndex},
        resolution::EvidenceBasis,
        resource::ResourceStore,
        scan::BaselineScan,
        symbol::SymbolStore,
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    const ENV_PY: &str = "\
import os


def load():
    url = os.getenv(\"DATABASE_URL\")
    again = os.getenv(\"DATABASE_URL\")
    host = os.environ[\"DB_HOST\"]
    port = os.environ.get(\"DB_PORT\")
    missing = os.getenv(\"NEVER_DEFINED_KEY\")
    name = \"DATABASE_URL\"
    dynamic = os.getenv(name)
    return url, again, host, port, missing, dynamic
";

    const APP_TS: &str = "\
console.log(process.env.TOP_LEVEL)

export function boot(): string {
  const url = process.env.DATABASE_URL
  const port = process.env[\"PORT\"]
  const label = \"DATABASE_URL\"
  const fromSettings = settings.get(\"DATABASE_URL\")
  return url + port + label + fromSettings
}
";

    const LIB_RS: &str = "\
pub fn read() -> String {
    std::env::var(\"RUST_LOG\").unwrap_or_default()
}
";

    const LOADER_CS: &str = "\
using System;
using System.Configuration;

public class Loader
{
    public string Read()
    {
        var value = Environment.GetEnvironmentVariable(\"DATABASE_URL\");
        var setting = ConfigurationManager.AppSettings[\"DATABASE_URL\"];
        var connection = ConfigurationManager.ConnectionStrings[\"Primary\"];
        return value + setting + connection;
    }
}
";

    /// No key access at all: a file that only mentions the name.
    const QUIET_TS: &str = "\
export function quiet(): string {
  return \"DATABASE_URL\"
}
";

    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-domain-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(root.join("src")).expect("src");
            let fixture = Self { base, root };
            fixture.write("src/env.py", ENV_PY);
            fixture.write("src/app.ts", APP_TS);
            fixture.write("src/lib.rs", LIB_RS);
            fixture.write("src/Loader.cs", LOADER_CS);
            fixture.write("src/quiet.ts", QUIET_TS);
            BaselineScan::open(&fixture.db_path())
                .expect("index.db")
                .run_initial_scan(
                    &fixture.root,
                    &WorkspaceConfig::default(),
                    "workspace-rev-1",
                )
                .expect("baseline scan");
            fixture
        }

        fn db_path(&self) -> PathBuf {
            self.base.join("data").join("index.db")
        }

        fn write(&self, rel: &str, contents: &str) {
            fs::write(self.root.join(rel), contents).expect("fixture file");
        }

        fn source(&self, rel: &str) -> String {
            fs::read_to_string(self.root.join(rel)).expect("source")
        }

        fn resource(&self, rel: &str) -> crate::resource::Resource {
            ResourceStore::open(&self.db_path())
                .expect("index.db")
                .get_active_by_path_key(rel)
                .expect("lookup")
                .expect("the fixture file is a Resource")
        }

        fn symbol(&self, rel: &str, qualified_name: &str) -> GraphEndpoint {
            GraphEndpoint::Symbol(
                SymbolStore::open(&self.db_path())
                    .expect("index.db")
                    .list_for_resource(self.resource(rel).id)
                    .expect("symbols")
                    .into_iter()
                    .find(|symbol| symbol.qualified_name == qualified_name)
                    .expect("the declaration is indexed")
                    .id,
            )
        }

        fn occurrences(&self, rel: &str) -> Vec<Occurrence> {
            SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_occurrences_for_resource(self.resource(rel).id)
                .expect("occurrences")
        }

        /// Everything the extractor makes of one file.
        fn accesses(&self, rel: &str) -> Vec<KeyAccess> {
            let resource = self.resource(rel);
            let source = self.source(rel);
            let dialect = dialect_for_path(rel).expect("dialect");
            let tree = ParserRegistry::new()
                .parse(dialect, source.as_bytes(), SourceBasis::of(&resource))
                .expect("parse");
            extract_key_accesses(&tree, source.as_bytes())
        }

        fn index(&self) -> RelationIndex {
            RelationIndex::open(&self.db_path()).expect("index.db")
        }

        fn store(&self) -> GraphStore {
            GraphStore::open(&self.db_path()).expect("index.db")
        }

        fn basis(&self, rel: &str, generation_id: i64) -> EvidenceBasis {
            let resource = self.resource(rel);
            let profile_id = self.occurrences(rel).first().map_or_else(
                || {
                    SymbolStore::open(&self.db_path())
                        .expect("index.db")
                        .list_for_resource(resource.id)
                        .expect("symbols")
                        .first()
                        .expect("the file declares something")
                        .analysis_profile_id
                },
                |occurrence| occurrence.analysis_profile_id,
            );
            EvidenceBasis {
                owner_resource: resource.id,
                owner_resource_revision: resource.resource_revision,
                generation_id,
                analysis_profile_id: profile_id,
                resolution_context_key: None,
            }
        }

        /// One publication, with each file's extracted domain evidence.
        fn publish(&self, files: &[&str]) {
            self.publish_with(files, &[]).expect("publish");
        }

        /// The same, optionally with extra evidence that may fail.
        fn publish_with(
            &self,
            files: &[&str],
            extra: &[(&str, RelationEvidence)],
        ) -> Result<(), EvidenceError> {
            let store = self.store();
            let connection = store.connection();
            let revision = generation::current_workspace_revision(connection)
                .expect("clock")
                .expect("bootstrapped");
            let building = generation::begin_generation(connection, &revision).expect("begin");
            let transaction = connection.unchecked_transaction().expect("transaction");
            let (record, grant) =
                generation::grant_publication(&transaction, building.id).expect("grant");

            let outcome = (|| -> Result<(), EvidenceError> {
                for rel in files {
                    let mut evidence = self.evidence_of(rel, building.id, &transaction, &grant);
                    for (owner, item) in extra {
                        if owner == rel {
                            for endpoint in [&item.relation.source, &item.relation.target] {
                                crate::graph::ensure_entity(&transaction, endpoint)
                                    .expect("ensure");
                            }
                            evidence.push(item.clone());
                        }
                    }
                    replace_resource_evidence(
                        &transaction,
                        &grant,
                        &self.basis(rel, building.id),
                        &evidence,
                    )?;
                }
                Ok(())
            })();

            match outcome {
                Ok(()) => {
                    generation::finish_publish_stable(&transaction, &record).expect("stable");
                    transaction.commit().expect("commit");
                    Ok(())
                }
                Err(error) => {
                    drop(transaction);
                    generation::abort_generation(connection, building.id, "test").expect("abort");
                    Err(error)
                }
            }
        }

        fn evidence_of(
            &self,
            rel: &str,
            generation_id: i64,
            transaction: &rusqlite::Connection,
            _grant: &PublicationGrant,
        ) -> Vec<RelationEvidence> {
            let resource = self.resource(rel);
            let occurrences = self.occurrences(rel);
            domain_relations(
                resource.id,
                &occurrences,
                &self.accesses(rel),
                generation_id,
            )
            .into_iter()
            .map(|(span, relation)| {
                for endpoint in [&relation.source, &relation.target] {
                    crate::graph::ensure_entity(transaction, endpoint).expect("ensure");
                }
                RelationEvidence {
                    occurrence: OccurrenceRef {
                        kind: OccurrenceKind::KeySite,
                        start_byte: span.start_byte,
                        end_byte: span.end_byte,
                    },
                    relation,
                }
            })
            .collect()
        }

        fn text_at(&self, rel: &str, span: SourceSpan) -> String {
            self.source(rel)[span.start_byte..span.end_byte].to_owned()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn env_key(key: &str) -> GraphEndpoint {
        GraphEndpoint::Domain(DomainEntity {
            kind: "ENV".to_owned(),
            normalized_identity: key.to_owned(),
            namespace: None,
            method: None,
            display_label: format!("ENV {key}"),
        })
    }

    fn app_setting(key: &str) -> GraphEndpoint {
        GraphEndpoint::Domain(DomainEntity {
            kind: "CONFIG".to_owned(),
            normalized_identity: key.to_owned(),
            namespace: Some("AppSettings".to_owned()),
            method: None,
            display_label: format!("CONFIG AppSettings:{key}"),
        })
    }

    fn keys(accesses: &[KeyAccess]) -> Vec<String> {
        accesses
            .iter()
            .filter_map(|access| match &access.key {
                KeyLiteral::Static(key) => Some(key.clone()),
                KeyLiteral::Dynamic => None,
            })
            .collect()
    }

    #[test]
    fn recognized_python_env_apis_state_their_keys_exactly() {
        let fixture = Fixture::create("python");
        let accesses = fixture.accesses("src/env.py");

        assert_eq!(
            keys(&accesses),
            vec![
                "DATABASE_URL",
                "DATABASE_URL",
                "DB_HOST",
                "DB_PORT",
                "NEVER_DEFINED_KEY"
            ],
            "getenv, environ[...] and environ.get -- and nothing else"
        );
        // The span is the key literal, not the call around it.
        for access in &accesses {
            if let KeyLiteral::Static(key) = &access.key {
                assert_eq!(
                    fixture.text_at("src/env.py", access.span),
                    format!("\"{key}\"")
                );
            }
        }
        assert!(
            accesses
                .iter()
                .all(|access| access.domain() == DomainKind::Env),
            "no config API exists in Python at this tier"
        );
    }

    #[test]
    fn a_dynamic_key_produces_no_entity_and_an_unrelated_literal_produces_nothing() {
        let fixture = Fixture::create("dynamic");
        let accesses = fixture.accesses("src/env.py");

        let dynamic: Vec<&KeyAccess> = accesses
            .iter()
            .filter(|access| access.key == KeyLiteral::Dynamic)
            .collect();
        assert_eq!(dynamic.len(), 1, "os.getenv(name)");
        assert_eq!(fixture.text_at("src/env.py", dynamic[0].span), "name");
        assert!(
            dynamic[0].entity().is_none(),
            "no DomainEntity is fabricated for an unknown key"
        );

        // `name = "DATABASE_URL"` is a string, not a key access.
        assert!(fixture.accesses("src/quiet.ts").is_empty());
        fixture.publish(&["src/env.py", "src/quiet.ts"]);
        assert!(
            fixture
                .index()
                .outgoing(
                    &GraphEndpoint::Resource(fixture.resource("src/quiet.ts").id),
                    &[RelationKind::UsesEnv, RelationKind::UsesConfig],
                )
                .expect("query")
                .confirmed
                .is_empty()
        );
        // And the dynamic access wrote nothing either: no entity with a
        // made-up key exists.
        let unknown: i64 = fixture
            .store()
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM domain_entity WHERE normalized_identity IN \
                 ('unknown', 'name', '')",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(unknown, 0);
    }

    #[test]
    fn js_process_env_member_and_index_access_are_recognized() {
        let fixture = Fixture::create("js");
        let accesses = fixture.accesses("src/app.ts");

        assert_eq!(keys(&accesses), vec!["TOP_LEVEL", "DATABASE_URL", "PORT"]);
        assert_eq!(
            fixture.text_at("src/app.ts", accesses[1].span),
            "DATABASE_URL"
        );
        assert_eq!(fixture.text_at("src/app.ts", accesses[2].span), "\"PORT\"");
        assert!(
            !keys(&accesses).contains(&"settings.get".to_owned()),
            "settings.get(\"DATABASE_URL\") is not a config API"
        );
        assert_eq!(
            accesses.len(),
            3,
            "the bare literal and the .get() call are not accesses"
        );
    }

    #[test]
    fn rust_and_csharp_env_apis_are_recognized() {
        let fixture = Fixture::create("rust-csharp");

        assert_eq!(keys(&fixture.accesses("src/lib.rs")), vec!["RUST_LOG"]);
        assert_eq!(
            fixture.text_at("src/lib.rs", fixture.accesses("src/lib.rs")[0].span),
            "\"RUST_LOG\""
        );

        let csharp = fixture.accesses("src/Loader.cs");
        assert_eq!(
            keys(&csharp),
            vec!["DATABASE_URL", "DATABASE_URL", "Primary"]
        );
        assert_eq!(
            csharp.iter().map(|access| access.api).collect::<Vec<_>>(),
            vec![
                KeyApi::CSharpEnvironment,
                KeyApi::CSharpAppSettings,
                KeyApi::CSharpConnectionStrings
            ]
        );
        // The config key's own literal, not the indexer around it.
        assert_eq!(
            fixture.text_at("src/Loader.cs", csharp[1].span),
            "\"DATABASE_URL\""
        );
        assert_eq!(
            fixture.text_at("src/Loader.cs", csharp[2].span),
            "\"Primary\""
        );
    }

    #[test]
    fn env_and_config_keys_of_the_same_name_are_different_identities() {
        let fixture = Fixture::create("identity");
        let csharp = fixture.accesses("src/Loader.cs");

        let from_env = csharp[0].entity().expect("static key");
        let from_config = csharp[1].entity().expect("static key");
        assert_eq!(
            from_env.normalized_identity,
            from_config.normalized_identity
        );
        assert_ne!(from_env, from_config, "ENV X and CONFIG X are not one key");
        assert_eq!(from_env.kind, "ENV");
        assert_eq!(from_config.kind, "CONFIG");
        assert_eq!(from_config.namespace.as_deref(), Some("AppSettings"));
        // And the two config stores are distinct too.
        assert_ne!(
            csharp[1].entity().expect("static"),
            csharp[2].entity().expect("static")
        );

        fixture.publish(&["src/Loader.cs"]);
        let store = fixture.store();
        assert!(
            store
                .entity(&env_key("DATABASE_URL"))
                .expect("get")
                .is_some()
        );
        assert!(
            store
                .entity(&app_setting("DATABASE_URL"))
                .expect("get")
                .is_some()
        );
    }

    #[test]
    fn one_key_read_twice_is_one_relation_with_two_evidence_locations() {
        let fixture = Fixture::create("repeat");
        fixture.publish(&["src/env.py"]);
        let load = fixture.symbol("src/env.py", "load");

        let answer = fixture
            .index()
            .outgoing(&load, &[RelationKind::UsesEnv])
            .expect("query");

        let database_url = answer
            .confirmed
            .iter()
            .find(|relation| relation.target == env_key("DATABASE_URL"))
            .expect("the key it reads twice");
        assert_eq!(database_url.evidence.len(), 2);
        assert_ne!(
            database_url.evidence[0].span.start_byte,
            database_url.evidence[1].span.start_byte
        );
        for location in &database_url.evidence {
            assert_eq!(location.occurrence_kind, OccurrenceKind::KeySite);
            assert_eq!(
                fixture.text_at("src/env.py", location.span),
                "\"DATABASE_URL\""
            );
        }
        // The containing Symbol is the source, because there is one.
        assert_eq!(database_url.source, load);
        assert_eq!(database_url.kind, RelationKind::UsesEnv);
    }

    #[test]
    fn a_file_level_access_is_owned_by_the_resource_and_invents_no_symbol() {
        let fixture = Fixture::create("file-level");
        let before: i64 = fixture
            .store()
            .connection()
            .query_row("SELECT COUNT(*) FROM symbol", [], |row| row.get(0))
            .expect("count");
        fixture.publish(&["src/app.ts"]);
        let after: i64 = fixture
            .store()
            .connection()
            .query_row("SELECT COUNT(*) FROM symbol", [], |row| row.get(0))
            .expect("count");
        assert_eq!(before, after, "no module Symbol was invented");

        let app = GraphEndpoint::Resource(fixture.resource("src/app.ts").id);
        let answer = fixture
            .index()
            .outgoing(&app, &[RelationKind::UsesEnv])
            .expect("query");
        assert_eq!(
            answer
                .confirmed
                .iter()
                .map(|relation| relation.target.clone())
                .collect::<Vec<_>>(),
            vec![env_key("TOP_LEVEL")],
            "the top-level access belongs to the file itself"
        );

        // The ones inside a function belong to the function.
        let boot = fixture.symbol("src/app.ts", "boot");
        let inside = fixture
            .index()
            .outgoing(&boot, &[RelationKind::UsesEnv])
            .expect("query");
        assert_eq!(inside.confirmed.len(), 2);
    }

    #[test]
    fn forward_and_reverse_queries_use_the_one_canonical_edge() {
        let fixture = Fixture::create("query");
        fixture.publish(&["src/env.py", "src/app.ts", "src/Loader.cs"]);

        // Three Resources read the same environment key.
        let users = fixture
            .index()
            .incoming(&env_key("DATABASE_URL"), &[RelationKind::UsesEnv])
            .expect("query");
        assert_eq!(users.confirmed_count(), 3);
        assert!(
            users
                .confirmed
                .iter()
                .all(|relation| relation.direction == Direction::Incoming
                    && relation.kind == RelationKind::UsesEnv)
        );
        let owners: Vec<ResourceId> = users
            .confirmed
            .iter()
            .flat_map(|relation| &relation.evidence)
            .map(|location| location.resource)
            .collect();
        for rel in ["src/env.py", "src/app.ts", "src/Loader.cs"] {
            assert!(
                owners.contains(&fixture.resource(rel).id),
                "{rel} proves its own use"
            );
        }

        // The config side answers through the same infrastructure.
        let config_users = fixture
            .index()
            .incoming(&app_setting("DATABASE_URL"), &[RelationKind::UsesConfig])
            .expect("query");
        assert_eq!(config_users.confirmed_count(), 1);

        // No reverse kind exists to store or to parse.
        let kinds = stored_kinds(&fixture);
        assert!(!kinds.iter().any(|kind| kind.ends_with("_BY")), "{kinds:?}");
        for forbidden in ["USED_BY", "ENV_USED_BY", "CONFIG_USED_BY"] {
            assert!(RelationKind::parse(forbidden).is_err());
        }
        assert!(kinds.iter().all(|kind| RelationKind::parse(kind).is_ok()));
    }

    #[test]
    fn a_relation_does_not_depend_on_the_key_existing_at_runtime() {
        let fixture = Fixture::create("no-runtime");
        fixture.publish(&["src/env.py"]);

        assert!(
            env::var("NEVER_DEFINED_KEY").is_err(),
            "the key really is not set in this process"
        );
        let users = fixture
            .index()
            .incoming(&env_key("NEVER_DEFINED_KEY"), &[RelationKind::UsesEnv])
            .expect("query");
        assert_eq!(
            users.confirmed_count(),
            1,
            "the relation is about the source, not the runtime"
        );
    }

    #[test]
    fn a_refreshed_owner_replaces_only_its_own_key_evidence() {
        let fixture = Fixture::create("replacement");
        fixture.publish(&["src/env.py", "src/app.ts", "src/Loader.cs"]);
        assert_eq!(
            fixture
                .index()
                .incoming(&env_key("DATABASE_URL"), &[RelationKind::UsesEnv])
                .expect("query")
                .confirmed_count(),
            3
        );

        // app.ts stops reading anything.
        fixture.write(
            "src/app.ts",
            "export function boot(): string {\n  return \"DATABASE_URL\"\n}\n",
        );
        fixture.publish(&["src/app.ts"]);

        let users = fixture
            .index()
            .incoming(&env_key("DATABASE_URL"), &[RelationKind::UsesEnv])
            .expect("query");
        assert_eq!(users.confirmed_count(), 2, "app.ts withdrew its own claim");
        let owners: Vec<ResourceId> = users
            .confirmed
            .iter()
            .flat_map(|relation| &relation.evidence)
            .map(|location| location.resource)
            .collect();
        assert!(!owners.contains(&fixture.resource("src/app.ts").id));
        assert!(owners.contains(&fixture.resource("src/env.py").id));

        // A key only app.ts read is gone entirely; the shared one lives
        // on because others still prove it.
        let store = fixture.store();
        assert!(
            store
                .relation(&RelationKey {
                    kind: RelationKind::UsesEnv,
                    source: &GraphEndpoint::Resource(fixture.resource("src/app.ts").id),
                    target: &env_key("TOP_LEVEL"),
                })
                .expect("get")
                .is_none()
        );
    }

    #[test]
    fn a_failed_publication_leaves_the_previous_state_intact() {
        let fixture = Fixture::create("rollback");
        fixture.publish(&["src/env.py"]);
        let before = fixture
            .index()
            .outgoing(
                &fixture.symbol("src/env.py", "load"),
                &[RelationKind::UsesEnv],
            )
            .expect("query");
        assert!(!before.confirmed.is_empty());

        // An Occurrence that does not exist fails the whole
        // replacement.
        let bogus = RelationEvidence {
            occurrence: OccurrenceRef {
                kind: OccurrenceKind::KeySite,
                start_byte: 99_999,
                end_byte: 100_000,
            },
            relation: Relation {
                kind: RelationKind::UsesEnv,
                source: GraphEndpoint::Resource(fixture.resource("src/env.py").id),
                target: env_key("NOT_WRITTEN"),
                dispatch: Dispatch::Static,
                created_generation: 1,
            },
        };
        let failure = fixture.publish_with(&["src/env.py"], &[("src/env.py", bogus)]);
        assert!(matches!(
            failure,
            Err(EvidenceError::UnknownOccurrence { .. })
        ));

        let after = fixture
            .index()
            .outgoing(
                &fixture.symbol("src/env.py", "load"),
                &[RelationKind::UsesEnv],
            )
            .expect("query");
        assert_eq!(before.confirmed, after.confirmed, "nothing was lost");
        assert!(
            fixture
                .store()
                .entity(&env_key("NOT_WRITTEN"))
                .expect("get")
                .is_none(),
            "and nothing from the failed attempt was kept"
        );
    }

    #[test]
    fn extraction_is_deterministic_and_stores_no_source() {
        let fixture = Fixture::create("deterministic");
        let first = fixture.accesses("src/env.py");
        let second = fixture.accesses("src/env.py");
        assert_eq!(first, second, "same file, same accesses, same order");
        assert_eq!(
            first
                .iter()
                .filter_map(KeyAccess::entity)
                .collect::<Vec<_>>(),
            second
                .iter()
                .filter_map(KeyAccess::entity)
                .collect::<Vec<_>>(),
            "and the same canonical identities"
        );

        fixture.publish(&["src/env.py", "src/app.ts", "src/lib.rs", "src/Loader.cs"]);
        let stored = fs::read(fixture.db_path()).expect("index.db");
        for body in ["os.getenv(", "unwrap_or_default", "return url, again"] {
            assert!(
                !stored
                    .windows(body.len())
                    .any(|window| window == body.as_bytes()),
                "index.db mirrors {body:?}"
            );
        }
        // The keys themselves are identity, and they are stored -- that
        // is the entity, not a source mirror.
        assert!(
            stored
                .windows("DATABASE_URL".len())
                .any(|window| window == b"DATABASE_URL"),
        );
    }

    fn stored_kinds(fixture: &Fixture) -> Vec<String> {
        let store = fixture.store();
        let mut statement = store
            .connection()
            .prepare("SELECT DISTINCT kind FROM relation ORDER BY kind")
            .expect("prepare");
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query");
        rows.map(|row| row.expect("kind")).collect()
    }
}
