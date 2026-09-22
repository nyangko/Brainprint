//! Wiring the TypeScript/JavaScript semantic tier to the Workspace
//! lifecycle.
//!
//! The adapter can answer questions about a file. This is what decides
//! *which* files to ask about, *when*, and in what order -- composing
//! the pieces that already exist rather than reimplementing them:
//!
//! ```text
//! filesystem change
//!   → affected owners, from persisted basis + structural dependents
//!   → withdraw their semantic contributions
//!   → structural replacement (I2/I3)
//!   → invalidate owner publications
//!   → one watched-file batch tells the backend the filesystem moved
//!   → refresh only the affected owners
//!   → publish (task 3) → merge (task 4)
//! ```
//!
//! ## Why withdrawal comes first
//!
//! `semantic_evidence.relation_id` deliberately has no
//! `ON DELETE CASCADE`. A cascade would silently drop the displaced
//! gaps task 4 restores on withdrawal, so a semantic edge would vanish
//! and leave a *silence* where an honest unresolved site belongs. The
//! price is an ordering obligation: a contribution pointing at
//! relations the structural replacement is about to remove has to be
//! withdrawn before that replacement runs. That is a contract, not an
//! FK error to catch.
//!
//! ## What is TypeScript's rather than Python's
//!
//! Three things, and only three. The project is selected by a
//! `tsconfig.json`/`jsconfig.json` whose `extends` chain is part of the
//! basis, not by a single file. The dependency environment is a
//! manifest and a lockfile rather than a `site-packages` tree, and it
//! is never walked. And the module inventory spans four extensions in
//! one project, because one server answers `.ts`, `.tsx`, `.js` and
//! `.jsx` alike.
//!
//! Everything else -- the availability vocabulary, withdrawal,
//! revalidation -- is shared, because it was never about a language.
//!
//! ## The backend is optional
//!
//! Every entry point here works with no TypeScript at all. Missing,
//! incompatible, crashed, in backoff or simply cold: the structural
//! answer stays current, the semantic-required gaps stay visible, and
//! the coverage says so. What never happens is an empty semantic
//! success, and what never happens is a stale semantic-only edge served
//! as current.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    path::Path,
};

use brainprint_core::ResourceId;
use rusqlite::{Connection, OptionalExtension, params};

use super::{
    RefreshOutcome, RefreshRequest, TypeScriptSemanticError, adapter,
    protocol::{WatchedChange, WatchedChangeKind, path_to_uri},
};
use crate::{
    db, graph_lifecycle,
    merge::{self, MergeOutcome},
    resource::{Resource, ResourceLanguage},
    semantic::{AnalysisContext, CapabilityReport},
    semantic_index::{
        BACKEND_UNAVAILABLE_CODE, CONFIG_CHANGED_CODE, ConfigBasis, CurrentInputs, SemanticIndex,
        SemanticIndexError, SemanticOwner, SemanticStatus,
    },
};

/// The Level B vocabulary, shared with every other backend rather than
/// spelled twice.
pub use crate::semantic_lifecycle::{BackendReadiness, SemanticAvailability, availability};

// ---------------------------------------------------------------------
// Project configuration
// ---------------------------------------------------------------------

/// Which file selects the project the backend analyzes.
///
/// TypeScript's own precedence: `tsconfig.json` governs a project, and
/// `jsconfig.json` is the JavaScript-first spelling of the same file
/// that applies only when there is no `tsconfig.json`. There is no
/// merge of the two, because the compiler does not merge them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    TsConfig,
    JsConfig,
    /// Neither file selects the project; the backend uses its defaults
    /// over the project root.
    Defaults,
}

impl ConfigSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TsConfig => "TSCONFIG_JSON",
            Self::JsConfig => "JSCONFIG_JSON",
            Self::Defaults => "DEFAULTS",
        }
    }
}

/// The file names TypeScript reads, in its own precedence order.
pub const CONFIG_FILE_NAMES: [(&str, ConfigSource); 2] = [
    ("tsconfig.json", ConfigSource::TsConfig),
    ("jsconfig.json", ConfigSource::JsConfig),
];

/// How deep an `extends` chain may go.
///
/// A bound rather than a guess: a config that extends itself, directly
/// or through a ring, must not become an infinite read. The visited set
/// already stops honest repeats; this stops a pathological chain.
const MAX_EXTENDS_DEPTH: usize = 16;

/// The `compilerOptions` keys that change what a module specifier
/// resolves to, or which files are in the program at all.
///
/// Read as a list of decisions rather than a grab bag: each one can
/// change a *semantic answer* without any source file moving, so each
/// one has to be in the basis or a stale publication would read as
/// current. Options that only change emit -- `target`, `sourceMap`,
/// `declaration` -- are deliberately absent: they cost invalidations
/// and buy nothing.
pub const RESOLUTION_OPTIONS: [&str; 13] = [
    "baseUrl",
    "paths",
    "rootDir",
    "rootDirs",
    "module",
    "moduleResolution",
    "moduleSuffixes",
    "customConditions",
    "resolveJsonModule",
    "allowJs",
    "checkJs",
    "jsx",
    "types",
];

/// The top-level keys that decide which files the project contains.
pub const MEMBERSHIP_KEYS: [&str; 4] = ["files", "include", "exclude", "references"];

/// One file in an `extends` chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFile {
    pub resource: Resource,
    /// Its own `compilerOptions`, as written.
    options: serde_json::Map<String, serde_json::Value>,
    /// Its own membership keys, as written.
    membership: serde_json::Map<String, serde_json::Value>,
}

/// Why an effective configuration could not be fully read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigLimit {
    /// `extends` names something this tier cannot resolve to a
    /// Workspace Resource -- a package, most often. The chain stops
    /// there and the configuration is partial, never guessed.
    UnresolvedExtends { from: String, specifier: String },
    /// The chain came back to a file it had already read.
    CircularExtends { at: String },
    /// A config file's bytes are not readable JSON.
    Unreadable { at: String, detail: String },
}

impl fmt::Display for ConfigLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnresolvedExtends { from, specifier } => {
                write!(
                    formatter,
                    "{from} extends {specifier}, which is not indexed"
                )
            }
            Self::CircularExtends { at } => write!(formatter, "{at} is already in the chain"),
            Self::Unreadable { at, detail } => write!(formatter, "{at} is unreadable: {detail}"),
        }
    }
}

/// The configuration one AnalysisContext is analyzed under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeScriptProjectConfig {
    pub source: ConfigSource,
    /// The chain, root first, then each `extends` target in order.
    pub chain: Vec<ConfigFile>,
    /// What could not be read. A non-empty list means the configuration
    /// is partial; it never means it was completed by guessing.
    pub limits: Vec<ConfigLimit>,
}

impl TypeScriptProjectConfig {
    /// The root config, when a file selects the project.
    #[must_use]
    pub fn root(&self) -> Option<&Resource> {
        self.chain.first().map(|file| &file.resource)
    }

    /// Whether the effective configuration is complete.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.limits.is_empty()
    }

    /// The effective value of one `compilerOptions` key.
    ///
    /// TypeScript's own precedence, which is a shallow per-key
    /// override: the nearest file in the chain that states a key wins
    /// outright, and `paths` is replaced rather than merged. Nothing is
    /// invented on top of that -- inventing a merge the compiler does
    /// not perform would make the basis describe a project the launched
    /// process does not analyze.
    #[must_use]
    pub fn option(&self, name: &str) -> Option<&serde_json::Value> {
        self.chain.iter().find_map(|file| file.options.get(name))
    }

    /// The effective value of one membership key.
    ///
    /// Same precedence, and one real difference from `compilerOptions`
    /// that TypeScript states outright: a child's `files` or `include`
    /// replaces the parent's entirely, and a child that states neither
    /// inherits both.
    #[must_use]
    pub fn membership(&self, name: &str) -> Option<&serde_json::Value> {
        self.chain.iter().find_map(|file| file.membership.get(name))
    }

    /// The task 3 [`ConfigBasis`] this configuration contributes.
    ///
    /// Three layers, and each earns its place. The *selected source*,
    /// because adding a `tsconfig.json` next to an existing
    /// `jsconfig.json` changes which file governs even if neither
    /// file's bytes move. The *content identity of every file in the
    /// chain*, because changing `tsconfig.base.json` must invalidate
    /// owners whose root config did not move -- which is the whole
    /// point of `extends`. And the *effective resolution options*,
    /// because two different chains that agree on every semantic option
    /// are the same configuration and should not force a refresh.
    #[must_use]
    pub fn basis(&self) -> ConfigBasis {
        let mut basis = ConfigBasis::new()
            .with("config_source", self.source.as_str())
            .with(
                "config_chain",
                db::fingerprint(
                    "typescript-config-chain-1",
                    &self
                        .chain
                        .iter()
                        .flat_map(|file| {
                            [
                                ("path", file.resource.path_key.as_str()),
                                (
                                    "content",
                                    file.resource
                                        .content_hash
                                        .as_deref()
                                        .unwrap_or(&file.resource.fingerprint),
                                ),
                            ]
                        })
                        .collect::<Vec<_>>(),
                ),
            );
        for name in RESOLUTION_OPTIONS {
            basis = basis.with(
                format!("option:{name}"),
                self.option(name)
                    .map_or_else(String::new, ToString::to_string),
            );
        }
        for name in MEMBERSHIP_KEYS {
            basis = basis.with(
                format!("membership:{name}"),
                self.membership(name)
                    .map_or_else(String::new, ToString::to_string),
            );
        }
        // A partial configuration is a different configuration from a
        // complete one that happens to say the same things.
        basis.with(
            "config_limits",
            if self.limits.is_empty() {
                String::new()
            } else {
                self.limits
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(";")
            },
        )
    }
}

/// Find the configuration the launched backend will actually use, and
/// read its whole `extends` chain.
///
/// Only Workspace Resources are considered: no editor settings, no user
/// profile, no ambient state, and no config inside `node_modules` --
/// a config that extends a published base is a real shape, and the
/// honest answer there is a partial configuration rather than a
/// dependency tree read into the basis.
///
/// # Errors
/// When the index cannot be read.
pub fn discover_config(
    connection: &Connection,
    workspace_root: &Path,
    project_root_rel: &str,
) -> Result<TypeScriptProjectConfig, LifecycleError> {
    for (name, source) in CONFIG_FILE_NAMES {
        let Some(resource) = active_resource_by_path(connection, &under(project_root_rel, name))?
        else {
            continue;
        };
        let mut config = TypeScriptProjectConfig {
            source,
            chain: Vec::new(),
            limits: Vec::new(),
        };
        read_chain(connection, workspace_root, resource, &mut config);
        return Ok(config);
    }
    Ok(TypeScriptProjectConfig {
        source: ConfigSource::Defaults,
        chain: Vec::new(),
        limits: Vec::new(),
    })
}

fn under(directory: &str, name: &str) -> String {
    if directory.is_empty() || directory == "." {
        name.to_owned()
    } else {
        format!("{directory}/{name}")
    }
}

fn read_chain(
    connection: &Connection,
    workspace_root: &Path,
    root: Resource,
    config: &mut TypeScriptProjectConfig,
) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut next = Some(root);
    for _ in 0..MAX_EXTENDS_DEPTH {
        let Some(resource) = next.take() else { break };
        if !seen.insert(resource.path_key.clone()) {
            config.limits.push(ConfigLimit::CircularExtends {
                at: resource.path_key.clone(),
            });
            break;
        }
        let parsed = match read_config(workspace_root, &resource) {
            Ok(parsed) => parsed,
            Err(detail) => {
                config.limits.push(ConfigLimit::Unreadable {
                    at: resource.path_key.clone(),
                    detail,
                });
                break;
            }
        };
        let extends = parsed.extends.clone();
        let path_key = resource.path_key.clone();
        config.chain.push(ConfigFile {
            resource,
            options: parsed.options,
            membership: parsed.membership,
        });

        // Only the first `extends` entry is followed. TypeScript 5.0
        // allows an array and applies them left to right with the last
        // winning, and modelling that as a *chain* would get the
        // precedence backwards -- so a multi-entry extends is reported
        // as a limit rather than half-understood.
        let Some(specifier) = extends.first() else {
            break;
        };
        if extends.len() > 1 {
            config.limits.push(ConfigLimit::UnresolvedExtends {
                from: path_key.clone(),
                specifier: extends.join(", "),
            });
            break;
        }
        match resolve_extends(connection, &path_key, specifier) {
            Ok(Some(resource)) => next = Some(resource),
            Ok(None) | Err(_) => {
                config.limits.push(ConfigLimit::UnresolvedExtends {
                    from: path_key,
                    specifier: specifier.clone(),
                });
                break;
            }
        }
    }
}

/// Where one `extends` specifier points, as a Workspace Resource.
///
/// Relative specifiers only, and the `.json` extension is optional
/// exactly as TypeScript allows. A bare specifier names a package and
/// resolves inside `node_modules`, which this tier does not index --
/// see [`ConfigLimit::UnresolvedExtends`].
fn resolve_extends(
    connection: &Connection,
    from: &str,
    specifier: &str,
) -> Result<Option<Resource>, LifecycleError> {
    if !specifier.starts_with('.') {
        return Ok(None);
    }
    let directory = from.rsplit_once('/').map_or("", |(head, _)| head);
    let mut parts: Vec<&str> = directory
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    for segment in specifier.split('/') {
        match segment {
            "." | "" => {}
            ".." => {
                if parts.pop().is_none() {
                    return Ok(None);
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    for candidate in [joined.clone(), format!("{joined}.json")] {
        if let Some(resource) = active_resource_by_path(connection, &candidate)? {
            return Ok(Some(resource));
        }
    }
    Ok(None)
}

struct ParsedConfig {
    extends: Vec<String>,
    options: serde_json::Map<String, serde_json::Value>,
    membership: serde_json::Map<String, serde_json::Value>,
}

fn read_config(workspace_root: &Path, resource: &Resource) -> Result<ParsedConfig, String> {
    let text = fs::read_to_string(workspace_root.join(&resource.path_rel))
        .map_err(|error| error.to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(&strip_jsonc(&text)).map_err(|error| error.to_string())?;
    let extends = match value.get("extends") {
        Some(serde_json::Value::String(one)) => vec![one.clone()],
        Some(serde_json::Value::Array(many)) => many
            .iter()
            .filter_map(|entry| entry.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    };
    let options = value
        .get("compilerOptions")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut membership = serde_json::Map::new();
    for name in MEMBERSHIP_KEYS {
        if let Some(found) = value.get(name) {
            membership.insert(name.to_owned(), found.clone());
        }
    }
    Ok(ParsedConfig {
        extends,
        options,
        membership,
    })
}

/// Remove the JSON-with-comments a `tsconfig.json` is allowed to
/// contain.
///
/// Not a convenience: TypeScript documents `tsconfig.json` as JSONC and
/// the file `tsc --init` writes is full of comments, so a strict JSON
/// parse would report the most ordinary config in the ecosystem as
/// unreadable and turn every such project into a partial configuration.
///
/// String-aware, because `"https://example.com"` is not a comment and
/// `"a\\"` does not escape the closing quote. Comment bodies are
/// replaced rather than deleted so byte offsets in a parse error still
/// point at the right place.
#[must_use]
pub fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(character) = characters.next() {
        if in_string {
            out.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => {
                in_string = true;
                out.push(character);
            }
            '/' if characters.peek() == Some(&'/') => {
                for skipped in characters.by_ref() {
                    if skipped == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if characters.peek() == Some(&'*') => {
                characters.next();
                let mut previous = '\0';
                for skipped in characters.by_ref() {
                    if previous == '*' && skipped == '/' {
                        break;
                    }
                    if skipped == '\n' {
                        out.push('\n');
                    }
                    previous = skipped;
                }
            }
            other => out.push(other),
        }
    }
    strip_trailing_commas(&out)
}

/// A trailing comma before `}` or `]`, which JSONC allows and JSON does
/// not.
fn strip_trailing_commas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut pending: Option<usize> = None;
    for character in text.chars() {
        if in_string {
            out.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => {
                pending = None;
                in_string = true;
                out.push(character);
            }
            ',' => {
                pending = Some(out.len());
                out.push(character);
            }
            ']' | '}' => {
                if let Some(at) = pending.take() {
                    out.replace_range(at..=at, " ");
                }
                out.push(character);
            }
            whitespace if whitespace.is_whitespace() => out.push(whitespace),
            other => {
                pending = None;
                out.push(other);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------
// Environment identity
// ---------------------------------------------------------------------

/// How much of the resolution environment could actually be proven.
///
/// Two axes, not one. The *fingerprint* is a deterministic identity of
/// what was seen; the assurance says whether what was seen is enough to
/// prove a persisted publication still describes this environment.
/// Collapsing them would force the choice between claiming currentness
/// nobody verified and randomizing the fingerprint so nothing ever
/// stays current -- and a random fingerprint is not an identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentAssurance {
    /// A manifest and a lockfile were both read, and neither points at
    /// a tree that can change without one of them moving.
    Proven,
    /// Something in the environment can change without the fingerprint
    /// moving, or could not be read at all. The fingerprint is still
    /// deterministic; it is just not evidence.
    Unknown { reason: String },
}

impl EnvironmentAssurance {
    #[must_use]
    pub const fn is_proven(&self) -> bool {
        matches!(self, Self::Proven)
    }
}

/// The environment a TypeScript/JavaScript semantic result depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentIdentity {
    pub fingerprint: String,
    pub assurance: EnvironmentAssurance,
    /// Which lockfile was found, if any. Diagnostic -- the fingerprint
    /// already carries it.
    pub lockfile: Option<String>,
}

/// The lockfiles this tier recognises, in the order it looks for them.
///
/// Opaque current inputs, every one of them: the bytes are fingerprinted
/// and nothing inside is parsed. A package manager's internal format is
/// its own business, and a resolver that had to understand three of them
/// would be wrong about all three within a release.
pub const LOCKFILES: [&str; 4] = [
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lockb",
];

/// The manifest that declares the dependencies.
pub const MANIFEST: &str = "package.json";

/// Fingerprint the toolchain and the tree it resolves packages through.
///
/// Manifest and lockfile *content identity* only. No `node_modules` is
/// walked, nothing inside a dependency is opened, and no recursive hash
/// is computed: the goal is a dependency-resolution identity, not a
/// dependency code index.
///
/// # Errors
/// When the index cannot be read.
pub fn environment_identity(
    connection: &Connection,
    workspace_root: &Path,
    project_root_rel: &str,
    backend_version: &str,
) -> Result<EnvironmentIdentity, LifecycleError> {
    let mut fields: Vec<(String, String)> =
        vec![("backend_version".to_owned(), backend_version.to_owned())];
    let mut unproven: Option<String> = None;
    let note = |reason: String, slot: &mut Option<String>| {
        if slot.is_none() {
            *slot = Some(reason);
        }
    };

    let manifest = active_resource_by_path(connection, &under(project_root_rel, MANIFEST))?;
    match &manifest {
        Some(resource) => {
            fields.push(("manifest".to_owned(), content_identity(resource)));
            if let Some(reason) = mutable_dependency(workspace_root, resource) {
                note(reason, &mut unproven);
            }
        }
        None => note(
            format!("no {MANIFEST} under {project_root_rel:?}"),
            &mut unproven,
        ),
    }

    let mut lockfile = None;
    for name in LOCKFILES {
        if let Some(resource) = active_resource_by_path(connection, &under(project_root_rel, name))?
        {
            fields.push((format!("lock:{name}"), content_identity(&resource)));
            lockfile = Some(name.to_owned());
            break;
        }
    }
    if lockfile.is_none() {
        // Without a lockfile the installed tree is whatever the last
        // install happened to resolve, and a reinstall can change what
        // a dependency import means with no tracked input moving. A
        // conservative UNKNOWN is the honest answer, and the task says
        // so outright.
        note(
            "no lockfile pins the installed dependency tree".to_owned(),
            &mut unproven,
        );
    }

    let borrowed: Vec<(&str, &str)> = fields
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    Ok(EnvironmentIdentity {
        fingerprint: db::fingerprint("typescript-semantic-env-1", &borrowed),
        assurance: match unproven {
            None => EnvironmentAssurance::Proven,
            Some(reason) => EnvironmentAssurance::Unknown { reason },
        },
        lockfile,
    })
}

fn content_identity(resource: &Resource) -> String {
    resource
        .content_hash
        .clone()
        .unwrap_or_else(|| resource.fingerprint.clone())
}

/// Whether the manifest points at package sources that can change
/// underneath an unchanged install.
///
/// The npm equivalent of Python's editable install, and the same rule
/// applies: a `file:`, `link:` or `workspace:` dependency resolves to a
/// tree this observer does not track, and a `workspaces` field links
/// sibling packages the same way.
fn mutable_dependency(workspace_root: &Path, manifest: &Resource) -> Option<String> {
    let text = fs::read_to_string(workspace_root.join(&manifest.path_rel)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    if value.get("workspaces").is_some() {
        return Some(format!(
            "{MANIFEST} declares workspaces, whose linked packages resolve to mutable source"
        ));
    }
    for section in ["dependencies", "devDependencies", "optionalDependencies"] {
        let Some(entries) = value.get(section).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for (name, requirement) in entries {
            let Some(requirement) = requirement.as_str() else {
                continue;
            };
            if ["file:", "link:", "workspace:", "portal:"]
                .iter()
                .any(|prefix| requirement.starts_with(prefix))
            {
                return Some(format!("{name} resolves to a mutable local tree"));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------
// Module inventory
// ---------------------------------------------------------------------

/// The languages one TypeScript backend answers for.
pub const SERVED_LANGUAGES: [ResourceLanguage; 2] =
    [ResourceLanguage::TypeScript, ResourceLanguage::JavaScript];

/// A deterministic identity for the module set a resolution happens
/// against.
///
/// TypeScript resolves modules against the whole program, so a result
/// does not depend only on the Resources it named: adding
/// `src/core/service.ts` can change what `@core/service` means in a
/// file nobody touched. That is exactly what task 3's
/// `inventory_fingerprint` is for.
///
/// Module-resolution context only -- the sorted path keys of the active
/// TypeScript and JavaScript Resources. No source body is read, so
/// adding, moving or renaming a file moves this and editing one does
/// not. A Markdown or Rust file moves nothing.
///
/// # Errors
/// When the index cannot be read.
pub fn inventory_fingerprint(connection: &Connection) -> Result<String, rusqlite::Error> {
    let mut statement = connection.prepare(
        "SELECT path_key FROM resource \
         WHERE state = 'ACTIVE' AND language IN (?1, ?2) ORDER BY path_key",
    )?;
    let keys: Vec<String> = statement
        .query_map(
            params![
                SERVED_LANGUAGES[0].to_string(),
                SERVED_LANGUAGES[1].to_string()
            ],
            |row| row.get(0),
        )?
        .collect::<Result<_, _>>()?;
    let fields: Vec<(&str, &str)> = keys.iter().map(|key| ("module", key.as_str())).collect();
    Ok(db::fingerprint("typescript-semantic-inventory-1", &fields))
}

// ---------------------------------------------------------------------
// Change sets
// ---------------------------------------------------------------------

/// What happened to one Resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangeKind {
    Added,
    Changed,
    Deleted,
    /// Identity preserved, locator moved. The Resource is the same one;
    /// its module specifier is not.
    Moved,
}

impl ChangeKind {
    const fn watched(self) -> WatchedChangeKind {
        match self {
            Self::Added => WatchedChangeKind::Created,
            Self::Changed | Self::Moved => WatchedChangeKind::Changed,
            Self::Deleted => WatchedChangeKind::Deleted,
        }
    }

    /// Whether this change can move a module in or out of the program,
    /// which TypeScript resolves project-wide.
    ///
    /// A content edit alone is absent on purpose: editing a file does
    /// not change what any *other* file's specifiers resolve to, so it
    /// invalidates the owners whose basis names it and nobody else.
    const fn moves_inventory(self) -> bool {
        matches!(self, Self::Added | Self::Deleted | Self::Moved)
    }
}

/// One Workspace change, as the lifecycle sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceChange {
    pub resource: ResourceId,
    pub kind: ChangeKind,
    /// The current Workspace-relative path. For a move, the new one.
    pub path_rel: String,
    /// The path the Resource used to be at, for a move or a delete.
    pub previous_path_rel: Option<String>,
    pub language: Option<ResourceLanguage>,
}

impl ResourceChange {
    /// A change to a TypeScript source file. Use
    /// [`Self::with_language`] for anything else -- a `.js` module, a
    /// config file, a lockfile.
    #[must_use]
    pub fn new(resource: ResourceId, kind: ChangeKind, path_rel: impl Into<String>) -> Self {
        Self {
            resource,
            kind,
            path_rel: path_rel.into(),
            previous_path_rel: None,
            language: Some(ResourceLanguage::TypeScript),
        }
    }

    #[must_use]
    pub fn from_previous(mut self, path_rel: impl Into<String>) -> Self {
        self.previous_path_rel = Some(path_rel.into());
        self
    }

    #[must_use]
    pub const fn with_language(mut self, language: Option<ResourceLanguage>) -> Self {
        self.language = language;
        self
    }

    /// Whether this Resource is one the backend answers for.
    #[must_use]
    pub fn is_served(&self) -> bool {
        self.language
            .is_some_and(|language| SERVED_LANGUAGES.contains(&language))
    }
}

/// What one batch of Workspace changes means for one context.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangePlan {
    /// The owner contributions that must stop being current.
    pub affected: BTreeSet<SemanticOwner>,
    /// Whether the module inventory moved, which can change import
    /// resolution for owners that did not change at all.
    pub inventory_moved: bool,
    /// Whether the selected configuration, or anything it extends,
    /// moved.
    pub config_moved: bool,
    /// Whether the manifest or lockfile moved, which can change what a
    /// dependency import means with no project source changing.
    pub environment_moved: bool,
}

impl ChangePlan {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.affected.is_empty()
            && !self.inventory_moved
            && !self.config_moved
            && !self.environment_moved
    }
}

/// Which owner contributions a change batch invalidates.
///
/// Three sources, unioned, and the order they are listed in is the
/// order they were learned in:
///
/// 1. The persisted semantic basis: a Resource nobody read invalidates
///    nobody.
/// 2. The *structural* dependents of the changed Resources. The basis
///    alone does not find them -- a call site I3 already resolved
///    structurally never becomes a semantic dependency, so the callee's
///    Resource is not in the caller's basis, and the caller is still
///    re-resolved by the replacement. Because
///    `semantic_evidence.relation_id` deliberately has no cascade,
///    missing one is not a stale row, it is a failed structural
///    publication (#19 task 9 found this the hard way in Python; it is
///    carried forward here rather than rediscovered).
/// 3. The whole context, when the configuration, the environment or the
///    module inventory moved -- each of those is a property of the
///    program, not of a file.
///
/// # Errors
/// When the index cannot be read.
pub fn plan_changes(
    index: &SemanticIndex,
    context: &AnalysisContext,
    changes: &[ResourceChange],
    config: &TypeScriptProjectConfig,
) -> Result<ChangePlan, LifecycleError> {
    let context_key = context.context_key();
    let mut plan = ChangePlan::default();
    let chain: BTreeSet<ResourceId> = config.chain.iter().map(|file| file.resource.id).collect();

    for change in changes {
        for owner in index.owners_depending_on(change.resource)? {
            if owner.context_key == context_key {
                plan.affected.insert(owner);
            }
        }
        if change.is_served() && change.kind.moves_inventory() {
            plan.inventory_moved = true;
        }
        // Every file in the chain counts, not just the root: changing
        // `tsconfig.base.json` must invalidate even though the root
        // config's own bytes did not move.
        if chain.contains(&change.resource)
            || touches(change, is_config_candidate)
            || touches(change, is_extends_candidate)
        {
            plan.config_moved = true;
        }
        if touches(change, is_environment_candidate) {
            plan.environment_moved = true;
        }
    }

    if plan.inventory_moved || plan.config_moved || plan.environment_moved {
        plan.affected.extend(index.owners_of_context(&context_key)?);
        return Ok(plan);
    }

    let touched: Vec<ResourceId> = changes.iter().map(|change| change.resource).collect();
    let published: BTreeSet<SemanticOwner> =
        index.owners_of_context(&context_key)?.into_iter().collect();
    for dependent in graph_lifecycle::dependents_of(index.connection(), &touched, false)? {
        let owner = SemanticOwner::new(&context_key, dependent);
        if published.contains(&owner) {
            plan.affected.insert(owner);
        }
    }
    Ok(plan)
}

/// Whether either end of a change -- where it is now, where it was --
/// matches.
fn touches(change: &ResourceChange, predicate: impl Fn(&str) -> bool) -> bool {
    predicate(&change.path_rel) || change.previous_path_rel.as_deref().is_some_and(&predicate)
}

fn file_name(path_rel: &str) -> &str {
    path_rel.rsplit('/').next().unwrap_or(path_rel)
}

fn is_config_candidate(path_rel: &str) -> bool {
    let name = file_name(path_rel);
    CONFIG_FILE_NAMES
        .iter()
        .any(|(candidate, _)| *candidate == name)
}

/// A config file that is only ever reached through `extends`.
///
/// Named by shape rather than by a fixed list, because the base file's
/// name is the project's choice: `tsconfig.base.json`,
/// `tsconfig.strict.json`, `configs/tsconfig.lib.json`. Matching
/// `tsconfig*.json` catches them without reading anything, and a false
/// positive costs one refresh rather than a stale answer.
fn is_extends_candidate(path_rel: &str) -> bool {
    let name = file_name(path_rel);
    (name.starts_with("tsconfig") || name.starts_with("jsconfig")) && name.ends_with(".json")
}

fn is_environment_candidate(path_rel: &str) -> bool {
    let name = file_name(path_rel);
    name == MANIFEST || LOCKFILES.contains(&name)
}

// ---------------------------------------------------------------------
// Watched-file notification
// ---------------------------------------------------------------------

/// The backend notification for one change batch.
///
/// Deduped by URI and ordered, so one logical Workspace operation
/// produces one deterministic batch however many owners it touches --
/// never one notification per affected owner. A move is two events,
/// because on the filesystem it is two.
#[must_use]
pub fn watched_changes(workspace_root: &Path, changes: &[ResourceChange]) -> Vec<WatchedChange> {
    let mut by_uri: BTreeMap<String, WatchedChangeKind> = BTreeMap::new();
    for change in changes {
        if let Some(previous) = &change.previous_path_rel
            && previous != &change.path_rel
        {
            by_uri.insert(
                path_to_uri(&workspace_root.join(previous)),
                WatchedChangeKind::Deleted,
            );
        }
        let uri = path_to_uri(&workspace_root.join(&change.path_rel));
        let kind = change.kind.watched();
        // A file both created and changed in one batch is created once.
        by_uri
            .entry(uri)
            .and_modify(|held| {
                if *held != kind && *held == WatchedChangeKind::Changed {
                    *held = kind;
                }
            })
            .or_insert(kind);
    }
    by_uri
        .into_iter()
        .map(|(uri, kind)| WatchedChange { uri, kind })
        .collect()
}

/// Deliver one change batch, then nothing else until it is on the wire.
///
/// The ordering barrier in one place: after this returns, every request
/// sent on the same connection is answered against the changed
/// filesystem. There is no sleep, no poll and no settle delay, because
/// there is nothing to wait for -- JSON-RPC over one ordered stdio
/// connection delivers the notification strictly before anything sent
/// after it.
///
/// # Errors
/// When the notification could not be delivered.
pub fn synchronize(
    queries: &dyn adapter::TypeScriptQueries,
    workspace_root: &Path,
    changes: &[ResourceChange],
) -> Result<usize, LifecycleError> {
    let batch = watched_changes(workspace_root, changes);
    let count = batch.len();
    if count == 0 {
        return Ok(0);
    }
    adapter::notify_watched_files(queries, batch)
        .map_err(|failure| LifecycleError::Sync(failure.to_string()))?;
    Ok(count)
}

// ---------------------------------------------------------------------
// Lifecycle steps
// ---------------------------------------------------------------------

/// Why a lifecycle step could not complete.
#[derive(Debug)]
pub enum LifecycleError {
    Index(SemanticIndexError),
    /// The structural tier could not be asked what a replacement is
    /// about to re-resolve.
    Structural(Box<crate::scan::ScanError>),
    Merge(merge::MergeError),
    Sqlite(rusqlite::Error),
    Refresh(Box<TypeScriptSemanticError>),
    /// The watched-file batch could not be delivered, so no request
    /// after it may be treated as current.
    Sync(String),
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Index(error) => write!(formatter, "semantic index: {error}"),
            Self::Structural(error) => write!(formatter, "structural plan: {error}"),
            Self::Merge(error) => write!(formatter, "semantic merge: {error}"),
            Self::Sqlite(error) => write!(formatter, "index: {error}"),
            Self::Refresh(error) => write!(formatter, "typescript refresh: {error}"),
            Self::Sync(detail) => write!(formatter, "watched-file notification: {detail}"),
        }
    }
}

impl Error for LifecycleError {}

impl From<SemanticIndexError> for LifecycleError {
    fn from(error: SemanticIndexError) -> Self {
        Self::Index(error)
    }
}
impl From<crate::scan::ScanError> for LifecycleError {
    fn from(error: crate::scan::ScanError) -> Self {
        Self::Structural(Box::new(error))
    }
}
impl From<merge::MergeError> for LifecycleError {
    fn from(error: merge::MergeError) -> Self {
        Self::Merge(error)
    }
}
impl From<rusqlite::Error> for LifecycleError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
impl From<TypeScriptSemanticError> for LifecycleError {
    fn from(error: TypeScriptSemanticError) -> Self {
        Self::Refresh(Box::new(error))
    }
}

/// What one withdrawal pass removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WithdrawReport {
    pub owners: Vec<SemanticOwner>,
    pub gaps_restored: usize,
    pub relations_removed: usize,
}

/// Withdraw every affected owner's contribution, then mark it dirty.
///
/// Runs *before* structural replacement. Withdrawal is what restores
/// the displaced structural gaps and garbage-collects semantic-only
/// edges; doing it afterwards would either fail on the relation foreign
/// key or, with a cascade in place, lose the gaps silently.
///
/// # Errors
/// When the index cannot be written.
pub fn withdraw_affected(
    index: &SemanticIndex,
    owners: &BTreeSet<SemanticOwner>,
    error_code: &str,
) -> Result<WithdrawReport, LifecycleError> {
    let connection = index.connection();
    let mut report = WithdrawReport::default();
    for owner in owners {
        let transaction = connection.unchecked_transaction()?;
        let outcome = merge::withdraw(&transaction, &owner.context_key, Some(owner.owner))?;
        transaction.commit()?;
        report.gaps_restored += outcome.gaps_restored;
        report.relations_removed += outcome.relations_removed;
        index.mark_dirty(owner, error_code)?;
        report.owners.push(owner.clone());
    }
    Ok(report)
}

/// Mark every affected owner unavailable, keeping its last valid
/// publication readable.
///
/// # Errors
/// When the index cannot be written.
pub fn mark_unavailable(
    index: &SemanticIndex,
    owners: &BTreeSet<SemanticOwner>,
    error_code: &str,
) -> Result<(), LifecycleError> {
    for owner in owners {
        index.mark_unavailable(owner, error_code)?;
    }
    Ok(())
}

/// Re-check every owner published for one context against the inputs as
/// they are now.
///
/// What a daemon reopen runs. Nothing is launched: a publication whose
/// sources, config, environment, inventory and profile all still hold
/// proves itself from disk, and one whose inputs moved reads DIRTY
/// before any backend exists to ask.
///
/// # Errors
/// When the index cannot be read.
pub fn revalidate_context(
    index: &SemanticIndex,
    context: &AnalysisContext,
    current: &CurrentInputs,
) -> Result<Vec<(SemanticOwner, SemanticStatus)>, LifecycleError> {
    let mut answers = Vec::new();
    for owner in index.owners_of_context(&context.context_key())? {
        let status = index.revalidate(&owner, current)?;
        answers.push((owner, status));
    }
    Ok(answers)
}

/// The inputs one owner's publication is validated against.
///
/// The environment enters twice, and has to: its fingerprint says
/// *which* environment, and its assurance says whether that fingerprint
/// is worth comparing. A persisted publication can only be restored as
/// CURRENT when both hold -- which is what stops a daemon reopen
/// against an unprovable `node_modules` from reading as current.
#[must_use]
pub fn current_inputs(
    context: &AnalysisContext,
    config: &ConfigBasis,
    capabilities: &CapabilityReport,
    inventory: &str,
    environment: &EnvironmentIdentity,
) -> CurrentInputs {
    CurrentInputs::new(context, config, capabilities)
        .with_inventory(inventory)
        .with_environment_proven(environment.assurance.is_proven())
}

/// One owner's refresh outcome, or why it did not happen.
#[derive(Debug)]
pub enum OwnerOutcome {
    Refreshed(Box<RefreshOutcome>),
    /// The refresh was attempted and failed. The owner keeps its
    /// last-valid publication and is marked, never replaced with an
    /// empty success.
    Failed {
        owner: SemanticOwner,
        error: Box<TypeScriptSemanticError>,
    },
}

impl OwnerOutcome {
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        matches!(self, Self::Refreshed(_))
    }
}

/// Refresh a set of owners, one at a time, on the shared runtime.
///
/// One owner's failure marks that owner and moves on: a backend hiccup
/// on `a.ts` is not a reason to tear down a still-valid publication for
/// `b.ts`.
///
/// # Errors
/// When the index cannot be written.
pub fn refresh_owners(
    index: &SemanticIndex,
    queries: &dyn adapter::TypeScriptQueries,
    request: &mut RefreshRequest<'_>,
    owners: &BTreeSet<SemanticOwner>,
) -> Result<Vec<OwnerOutcome>, LifecycleError> {
    let mut outcomes = Vec::new();
    for owner in owners {
        request.owner = owner.owner;
        match super::refresh_resource(index, queries, request) {
            Ok(outcome) => outcomes.push(OwnerOutcome::Refreshed(Box::new(outcome))),
            Err(error) => {
                // The publication that is already there stays readable;
                // it just stops describing itself as current.
                index.mark_dirty(owner, BACKEND_UNAVAILABLE_CODE)?;
                outcomes.push(OwnerOutcome::Failed {
                    owner: owner.clone(),
                    error: Box::new(error),
                });
            }
        }
    }
    Ok(outcomes)
}

/// Mark every owner in a context not current because the configuration
/// that governs it changed.
///
/// # Errors
/// When the index cannot be written.
pub fn invalidate_for_config(
    index: &SemanticIndex,
    context: &AnalysisContext,
) -> Result<BTreeSet<SemanticOwner>, LifecycleError> {
    let owners: BTreeSet<SemanticOwner> = index
        .owners_of_context(&context.context_key())?
        .into_iter()
        .collect();
    for owner in &owners {
        index.mark_dirty(owner, CONFIG_CHANGED_CODE)?;
    }
    Ok(owners)
}

/// What a whole merge pass did, summed.
#[must_use]
pub fn total_merged(outcomes: &[OwnerOutcome]) -> MergeOutcome {
    let mut total = MergeOutcome::default();
    for outcome in outcomes {
        if let OwnerOutcome::Refreshed(refreshed) = outcome {
            total.gaps_resolved += refreshed.merged.gaps_resolved;
            total.gaps_restored += refreshed.merged.gaps_restored;
            total.corroborated += refreshed.merged.corroborated;
            total.relations_created += refreshed.merged.relations_created;
            total.relations_removed += refreshed.merged.relations_removed;
            total.conflicts += refreshed.merged.conflicts;
        }
    }
    total
}

fn active_resource_by_path(
    connection: &Connection,
    path_key: &str,
) -> Result<Option<Resource>, LifecycleError> {
    /// `(uid, path_rel, revision, content_hash, fingerprint)`.
    type ConfigRow = (Vec<u8>, String, String, Option<String>, String);
    let row: Option<ConfigRow> = connection
        .query_row(
            "SELECT uid, path_rel, resource_revision, content_hash, fingerprint \
             FROM resource WHERE path_key = ?1 AND state = 'ACTIVE'",
            params![path_key],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((uid, path_rel, revision, content_hash, fingerprint)) = row else {
        return Ok(None);
    };
    let bytes: [u8; 16] = uid
        .as_slice()
        .try_into()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    Ok(Some(Resource {
        id: ResourceId::from_bytes(bytes),
        path_rel,
        path_key: path_key.to_owned(),
        kind: crate::resource::ResourceKind::File,
        role: crate::resource::ResourceRole::Config,
        language: None,
        size_bytes: 0,
        mtime_ns: 0,
        fingerprint,
        content_hash,
        state: crate::resource::ResourceState::Active,
        resource_revision: revision,
        generated_kind: None,
        container_resource_id: None,
    }))
}
