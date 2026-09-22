//! The backend-independent semantic contract (#19 task 1).
//!
//! I3 leaves honest gaps: an import whose module resolution needs a
//! config, a `obj.method()` whose receiver type is not written down, an
//! `override` marker with no stated base. I4 closes those with language
//! semantic engines. This module is the vocabulary every one of those
//! engines speaks, and nothing here starts, talks to, or names a
//! concrete backend process -- that is task 2 and task 5.
//!
//! ## What the contract is for
//!
//! Semantic backends **enrich** the existing model. There is one
//! canonical graph, and it is the Resource/Symbol/Occurrence/Relation
//! graph I2 and I3 already publish. So a backend result arrives here as
//! [`SemanticEvidence`]: an [`EvidenceBasis`] naming the Resource and
//! revision it was read from, an [`OccurrenceRef`] naming the exact
//! source span, and [`GraphEndpoint`]s naming what it resolved to. There
//! is deliberately no field anywhere in this module that could hold a
//! Pyright symbol id, an LSP document handle, a tsserver project handle,
//! a Roslyn `SymbolKey`, or a rust-analyzer id. A backend's own handles
//! stay inside the adapter that owns them; what crosses this boundary is
//! Brainprint identity or nothing.
//!
//! ## Capability, not "supported language"
//!
//! `Python = supported` is the claim that produces false zeros. A
//! backend covers [`SemanticCapability::ImportBinding`] and not
//! [`SemanticCapability::OverloadResolution`]; it covers both for a
//! project with a config it understands and neither for one without. So
//! support is reported per capability ([`CapabilityReport`]), per
//! [`AnalysisContext`], in the same three-state [`Support`] vocabulary
//! the rest of the crate already uses -- never a confidence float, and
//! never a language-wide boolean.
//!
//! A capability nobody declared reads [`Support::Unsupported`]. Silence
//! is not a claim of coverage.
//!
//! ## Level A / B / C
//!
//! The grading itself belongs to a later acceptance layer, and there is
//! no `Python = Level A` constant here or anywhere. What this module
//! provides is the input that grading needs: capabilities carry a
//! [`CapabilityGroup`], and [`CapabilityReport::group_support`] answers
//! how well a whole group is covered *for one context*.
//!
//! - Level A (Deep Semantic) is a claim about [`CapabilityGroup::Type`].
//! - Level B (Structural) is a claim about [`CapabilityGroup::Structure`].
//! - Level C (Resource/Text) is a claim about
//!   [`SemanticCapability::ResourceDiscovery`] alone.
//!
//! Each is derived from what a backend actually reports, per context.

use std::{collections::BTreeMap, fmt};

use brainprint_core::{ResourceId, WorkspaceId};

use crate::{
    db,
    evidence::OccurrenceRef,
    gaps::UnresolvedReason,
    graph::{GraphEndpoint, RelationKind},
    resolution::{
        Dispatch, EvidenceBasis, Resolution, Support, UnknownAxisValue, closed_vocabulary,
        weaker_support,
    },
    resource::ResourceLanguage,
};

/// One semantic question a backend may or may not be able to answer.
///
/// Closed on purpose. A free-form capability string would let an adapter
/// invent a capability no consumer knows how to grade, which is how
/// "supported" quietly stops meaning anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SemanticCapability {
    // Resource / Structure.
    /// Which files the backend considers part of the analysis at all.
    ResourceDiscovery,
    SyntaxStructure,
    SymbolDefinition,
    SymbolSpan,
    ContainingScope,
    ImportDeclaration,
    ExportDeclaration,
    /// Where an embedded script/template region begins and ends inside a
    /// container file (a Svelte component, JSX markup).
    EmbeddedRegionMapping,
    /// A position in generated output mapped back to the original source
    /// it came from. Without it, a generated span must never be reported
    /// as an original one.
    OriginalSourceMapping,

    // Binding / Relation.
    ImportBinding,
    AliasResolution,
    ReexportResolution,
    References,
    CallsIntraFile,
    CallsCrossFile,
    /// A target outside the Workspace, resolved to a stable external
    /// identity rather than to indexed dependency source.
    ExternalSymbolResolution,

    // Type / Semantic.
    TypeResolution,
    Inheritance,
    Implements,
    Overrides,
    /// Which target a call site statically binds to, when it does.
    StaticDispatchTarget,
    OverloadResolution,
    /// Which concrete implementation satisfies an abstract member.
    ImplementationTarget,
}

closed_vocabulary!(SemanticCapability {
    ResourceDiscovery => "RESOURCE_DISCOVERY",
    SyntaxStructure => "SYNTAX_STRUCTURE",
    SymbolDefinition => "SYMBOL_DEFINITION",
    SymbolSpan => "SYMBOL_SPAN",
    ContainingScope => "CONTAINING_SCOPE",
    ImportDeclaration => "IMPORT_DECLARATION",
    ExportDeclaration => "EXPORT_DECLARATION",
    EmbeddedRegionMapping => "EMBEDDED_REGION_MAPPING",
    OriginalSourceMapping => "ORIGINAL_SOURCE_MAPPING",
    ImportBinding => "IMPORT_BINDING",
    AliasResolution => "ALIAS_RESOLUTION",
    ReexportResolution => "REEXPORT_RESOLUTION",
    References => "REFERENCES",
    CallsIntraFile => "CALLS_INTRA_FILE",
    CallsCrossFile => "CALLS_CROSS_FILE",
    ExternalSymbolResolution => "EXTERNAL_SYMBOL_RESOLUTION",
    TypeResolution => "TYPE_RESOLUTION",
    Inheritance => "INHERITANCE",
    Implements => "IMPLEMENTS",
    Overrides => "OVERRIDES",
    StaticDispatchTarget => "STATIC_DISPATCH_TARGET",
    OverloadResolution => "OVERLOAD_RESOLUTION",
    ImplementationTarget => "IMPLEMENTATION_TARGET",
});

impl SemanticCapability {
    /// Every capability, in a fixed order. The order is part of the
    /// contract: [`CapabilityReport::capability_fingerprint`] depends on
    /// it being the same everywhere.
    pub const ALL: [Self; 23] = [
        Self::ResourceDiscovery,
        Self::SyntaxStructure,
        Self::SymbolDefinition,
        Self::SymbolSpan,
        Self::ContainingScope,
        Self::ImportDeclaration,
        Self::ExportDeclaration,
        Self::EmbeddedRegionMapping,
        Self::OriginalSourceMapping,
        Self::ImportBinding,
        Self::AliasResolution,
        Self::ReexportResolution,
        Self::References,
        Self::CallsIntraFile,
        Self::CallsCrossFile,
        Self::ExternalSymbolResolution,
        Self::TypeResolution,
        Self::Inheritance,
        Self::Implements,
        Self::Overrides,
        Self::StaticDispatchTarget,
        Self::OverloadResolution,
        Self::ImplementationTarget,
    ];

    /// Which axis of the model this capability answers for.
    #[must_use]
    pub const fn group(self) -> CapabilityGroup {
        match self {
            Self::ResourceDiscovery
            | Self::SyntaxStructure
            | Self::SymbolDefinition
            | Self::SymbolSpan
            | Self::ContainingScope
            | Self::ImportDeclaration
            | Self::ExportDeclaration
            | Self::EmbeddedRegionMapping
            | Self::OriginalSourceMapping => CapabilityGroup::Structure,
            Self::ImportBinding
            | Self::AliasResolution
            | Self::ReexportResolution
            | Self::References
            | Self::CallsIntraFile
            | Self::CallsCrossFile
            | Self::ExternalSymbolResolution => CapabilityGroup::Binding,
            Self::TypeResolution
            | Self::Inheritance
            | Self::Implements
            | Self::Overrides
            | Self::StaticDispatchTarget
            | Self::OverloadResolution
            | Self::ImplementationTarget => CapabilityGroup::Type,
        }
    }
}

/// The three capability axes a later acceptance layer grades against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityGroup {
    /// What exists and where it is written.
    Structure,
    /// What a written name binds to.
    Binding,
    /// What a type or dispatch position means.
    Type,
}

closed_vocabulary!(CapabilityGroup {
    Structure => "STRUCTURE",
    Binding => "BINDING",
    Type => "TYPE",
});

/// Which family of semantic backend serves a context.
///
/// Named after the language family it answers for, not after the tool
/// behind it: the Python backend stays the Python backend whether task 5
/// settles on Pyright, something else, or a different transport to the
/// same thing. No process command, protocol, or port appears here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SemanticBackendKind {
    Python,
    /// One backend for TypeScript and JavaScript, JSX and TSX included.
    /// React is not a language and has no backend of its own.
    TypeScriptJavaScript,
    /// Svelte components: a container whose script blocks carry TS/JS
    /// semantics, so this backend serves the container and leans on
    /// TS/JS semantics inside it.
    Svelte,
    CSharp,
    Rust,
}

closed_vocabulary!(SemanticBackendKind {
    Python => "PYTHON",
    TypeScriptJavaScript => "TYPESCRIPT_JAVASCRIPT",
    Svelte => "SVELTE",
    CSharp => "CSHARP",
    Rust => "RUST",
});

impl SemanticBackendKind {
    /// The languages this backend can be asked about.
    ///
    /// More than one for the TS/JS and Svelte backends, which is exactly
    /// why the backend kind is not the language.
    #[must_use]
    pub const fn languages(self) -> &'static [ResourceLanguage] {
        match self {
            Self::Python => &[ResourceLanguage::Python],
            Self::TypeScriptJavaScript => {
                &[ResourceLanguage::TypeScript, ResourceLanguage::JavaScript]
            }
            // The component file itself, plus the embedded script
            // languages its blocks are written in.
            Self::Svelte => &[
                ResourceLanguage::Svelte,
                ResourceLanguage::TypeScript,
                ResourceLanguage::JavaScript,
            ],
            Self::CSharp => &[ResourceLanguage::CSharp],
            Self::Rust => &[ResourceLanguage::Rust],
        }
    }

    /// Whether this backend answers for `language` at all.
    #[must_use]
    pub fn serves(self, language: ResourceLanguage) -> bool {
        self.languages().contains(&language)
    }
}

/// The project/config root an analysis is scoped to, as identity.
///
/// A path is not identity: the same project reached through a symlink,
/// or after the Workspace is moved, is the same project. So the root is
/// a Resource when the Workspace inventory covers it (a `pyproject.toml`,
/// a `tsconfig.json`, a `.csproj`, a `Cargo.toml`), and a normalized key
/// only when nothing in the inventory does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectRootIdentity {
    /// The config file that defines the project, as a Resource.
    Config(ResourceId),
    /// No config Resource defines it -- a language whose project is the
    /// Workspace itself, or a config outside the inventory. The key is
    /// the caller's normalized vocabulary, never an absolute path.
    Key(String),
}

impl ProjectRootIdentity {
    fn fingerprint_field(&self) -> (&'static str, String) {
        match self {
            Self::Config(resource) => ("project_root_config", resource.to_string()),
            Self::Key(key) => ("project_root_key", key.clone()),
        }
    }
}

/// The toolchain and environment a semantic result depends on.
///
/// Two Workspaces on the same source can resolve the same import to
/// different things because their interpreters, SDKs, or dependency sets
/// differ. All three fields are identity, and all three are
/// fingerprints or version strings -- never raw config or source text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolchainIdentity {
    /// The semantic backend's own version.
    pub backend_version: String,
    /// Which backend versions results are comparable across, so a patch
    /// bump inside one class does not invalidate everything.
    pub backend_compatibility_class: String,
    /// The interpreter/SDK/runtime and its resolved dependency set: the
    /// virtualenv, the node_modules tree, the target framework.
    pub environment_fingerprint: String,
}

impl ToolchainIdentity {
    /// This toolchain's deterministic fingerprint, for a publication
    /// basis that must detect an environment moving underneath it (#19
    /// task 3).
    #[must_use]
    pub fn fingerprint(&self) -> String {
        db::fingerprint(
            "semantic-toolchain-1",
            &[
                ("backend_version", &self.backend_version),
                (
                    "backend_compatibility_class",
                    &self.backend_compatibility_class,
                ),
                ("environment", &self.environment_fingerprint),
            ],
        )
    }
}

/// What a semantic analysis is *of*, independently of who asked.
///
/// The owner of a semantic backend is this context, never an Agent or a
/// session. Claude, Codex, Gemini and three more clients looking at the
/// same Workspace, language, project root and toolchain resolve to one
/// AnalysisContext and share one backend and one set of results -- which
/// is only true because no client, session, or request identity appears
/// in this struct or in [`Self::context_key`].
///
/// A different worktree has a different [`WorkspaceId`], so it is a
/// different context and never shares current semantic state, even
/// against the same Git repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisContext {
    pub workspace: WorkspaceId,
    pub backend: SemanticBackendKind,
    pub language: ResourceLanguage,
    pub project_root: ProjectRootIdentity,
    pub toolchain: ToolchainIdentity,
}

/// Tag on every [`AnalysisContext::context_key`], so a stored key stays
/// self-describing if the derivation ever changes.
pub const CONTEXT_KEY_ALGORITHM: &str = "sha256-ac1";

impl AnalysisContext {
    /// This context's deterministic identity.
    ///
    /// Same inputs, same key, on any machine, in any order of discovery
    /// and from any client -- which is what makes two Agents reuse one
    /// backend instead of starting two.
    #[must_use]
    pub fn context_key(&self) -> String {
        let (root_field, root_value) = self.project_root.fingerprint_field();
        db::fingerprint(
            CONTEXT_KEY_ALGORITHM,
            &[
                ("workspace", &self.workspace.to_string()),
                ("backend", self.backend.as_str()),
                ("language", &self.language.to_string()),
                (root_field, &root_value),
                ("backend_version", &self.toolchain.backend_version),
                (
                    "backend_compatibility_class",
                    &self.toolchain.backend_compatibility_class,
                ),
                (
                    "environment_fingerprint",
                    &self.toolchain.environment_fingerprint,
                ),
            ],
        )
    }
}

/// Where a context's project root currently lives on disk.
///
/// Deliberately *not* part of [`AnalysisContext`]: a locator is current
/// metadata, an identity is not. A backend process needs a working
/// directory and a config file to start (task 2), and collapsing that
/// into the identity would make the same project a different context
/// every time the Workspace moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisContextBinding {
    pub context: AnalysisContext,
    /// The project root, relative to the Workspace root.
    pub project_root_rel: String,
    /// The config file that defines it, relative to the Workspace root,
    /// when there is one.
    pub config_file_rel: Option<String>,
}

/// What one backend declares it can answer for one [`AnalysisContext`].
///
/// Capabilities are independent: a backend may fully support
/// [`SemanticCapability::ImportBinding`], partially support
/// [`SemanticCapability::References`], and not support
/// [`SemanticCapability::OverloadResolution`] at all, in the same
/// context. Nothing here collapses that into one verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityReport {
    context_key: String,
    declared: BTreeMap<SemanticCapability, Support>,
}

impl CapabilityReport {
    /// A report that claims nothing yet: every capability reads
    /// [`Support::Unsupported`] until a backend declares otherwise.
    #[must_use]
    pub fn new(context: &AnalysisContext) -> Self {
        Self {
            context_key: context.context_key(),
            declared: BTreeMap::new(),
        }
    }

    /// The context this report is about. A report is never read against
    /// a different one.
    #[must_use]
    pub fn context_key(&self) -> &str {
        &self.context_key
    }

    /// Declare what `capability` is covered by. Re-declaring replaces
    /// the previous value: a backend that loses a capability after a
    /// config change must be able to say so.
    pub fn declare(&mut self, capability: SemanticCapability, support: Support) -> &mut Self {
        self.declared.insert(capability, support);
        self
    }

    /// What `capability` is covered by.
    ///
    /// Undeclared reads [`Support::Unsupported`] -- an empty result for
    /// something nobody claimed to analyze is a statement about
    /// capability, not about the code.
    #[must_use]
    pub fn support(&self, capability: SemanticCapability) -> Support {
        self.declared
            .get(&capability)
            .copied()
            .unwrap_or(Support::Unsupported)
    }

    /// Whether `capability` was given a verdict at all.
    ///
    /// [`Self::support`] deliberately reads an undeclared capability as
    /// UNSUPPORTED, which is the right default for a *caller* -- but it
    /// makes "we considered this and it does not apply" indistinguishable
    /// from "nobody thought about it". A backend asserting that its
    /// matrix is complete needs to tell those apart.
    #[must_use]
    pub fn is_declared(&self, capability: SemanticCapability) -> bool {
        self.declared.contains_key(&capability)
    }

    /// How well a whole group is covered: the weakest member decides,
    /// because a group is not better covered than its worst capability.
    ///
    /// This is the input a later Level A/B/C acceptance layer grades
    /// from. It is not itself a grade, and there is no language-wide
    /// level constant anywhere in this crate.
    #[must_use]
    pub fn group_support(&self, group: CapabilityGroup) -> Support {
        SemanticCapability::ALL
            .into_iter()
            .filter(|capability| capability.group() == group)
            .fold(Support::Supported, |weakest, capability| {
                weaker_support(weakest, self.support(capability))
            })
    }

    /// Every capability and its declared support, in
    /// [`SemanticCapability::ALL`] order.
    pub fn entries(&self) -> impl Iterator<Item = (SemanticCapability, Support)> + '_ {
        SemanticCapability::ALL
            .into_iter()
            .map(|capability| (capability, self.support(capability)))
    }

    /// A deterministic fingerprint of the whole report, in the shape
    /// `analysis_profile.capability_fingerprint` wants.
    ///
    /// Covers every capability rather than only the declared ones, so
    /// losing a declaration changes the fingerprint instead of being
    /// invisible.
    #[must_use]
    pub fn capability_fingerprint(&self) -> String {
        let fields: Vec<(&str, &str)> = SemanticCapability::ALL
            .iter()
            .map(|capability| (capability.as_str(), self.support(*capability).as_str()))
            .collect();
        db::fingerprint("semantic-capability-1", &fields)
    }
}

/// What a backend established about one source position.
///
/// Every variant carries Brainprint identity or a reason, and none of
/// them carries a backend handle. A [`Self::Candidates`] set is never
/// promoted to [`Self::Resolved`] because it happens to hold one entry:
/// that is the guess this tier exists to avoid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticOutcome {
    /// Exactly one target, established deterministically.
    Resolved { target: GraphEndpoint },
    /// Possible targets, none confirmed.
    Candidates { targets: Vec<GraphEndpoint> },
    /// The backend read the position and could not establish a target.
    /// Kept, because dropping it would be a false zero.
    Unresolved { reason: UnresolvedReason },
}

impl SemanticOutcome {
    /// Which resolution axis value this outcome is.
    #[must_use]
    pub const fn resolution(&self) -> Resolution {
        match self {
            Self::Resolved { .. } => Resolution::Resolved,
            Self::Candidates { .. } => Resolution::Candidate,
            Self::Unresolved { .. } => Resolution::Unresolved,
        }
    }
}

/// One normalized semantic fact, ready to be merged with structural
/// truth (task 4).
///
/// The whole point of the type is what it *cannot* express: there is no
/// field for a backend symbol id, document handle, or project handle.
/// An adapter that wants to pass one has to resolve it to a
/// [`GraphEndpoint`] first, which is the boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticEvidence {
    /// The context that produced it, by [`AnalysisContext::context_key`].
    pub context_key: String,
    /// Which capability answered. A consumer that does not trust a
    /// capability in this context can drop the evidence without
    /// inspecting it.
    pub capability: SemanticCapability,
    /// The canonical relation this evidence states, when the backend
    /// identified one. `None` for evidence that resolves a target
    /// without stating what relation it is -- a type lookup used only to
    /// bind a name. The merge tier (#19 task 4) refuses to turn `None`
    /// into an edge rather than inferring a kind from the capability.
    pub relation_kind: Option<RelationKind>,
    /// The Resource, revision, generation and profile this was read
    /// against -- the same basis structural evidence is published with,
    /// so freshness is one question and not two.
    pub basis: EvidenceBasis,
    /// The exact source position, when the capability is about one.
    /// `None` for a whole-Resource fact.
    pub occurrence: Option<OccurrenceRef>,
    /// The Brainprint endpoint the evidence is *from*, when it is
    /// already known -- the calling Symbol, the importing Resource.
    pub source: Option<GraphEndpoint>,
    pub outcome: SemanticOutcome,
    /// How much of the owning Resource the backend actually covered. A
    /// resolved target in a partially covered file is still a resolved
    /// target; absence of other targets in it proves nothing.
    pub support: Support,
    /// Whether the position binds statically. [`Dispatch::Unknown`] when
    /// the backend has not established it -- never guessed.
    pub dispatch: Dispatch,
}

impl SemanticEvidence {
    /// Which resolution axis value this evidence carries.
    #[must_use]
    pub const fn resolution(&self) -> Resolution {
        self.outcome.resolution()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{graph::ExternalEntity, symbol::OccurrenceKind};
    use brainprint_core::SymbolId;

    fn toolchain() -> ToolchainIdentity {
        ToolchainIdentity {
            backend_version: "1.2.3".to_owned(),
            backend_compatibility_class: "python-semantic:1".to_owned(),
            environment_fingerprint: "sha256:venv".to_owned(),
        }
    }

    fn context() -> AnalysisContext {
        AnalysisContext {
            workspace: WorkspaceId::from_bytes([7; 16]),
            backend: SemanticBackendKind::Python,
            language: ResourceLanguage::Python,
            project_root: ProjectRootIdentity::Config(ResourceId::from_bytes([9; 16])),
            toolchain: toolchain(),
        }
    }

    #[test]
    fn every_vocabulary_round_trips_and_refuses_anything_else() {
        for capability in SemanticCapability::ALL {
            assert_eq!(
                SemanticCapability::parse(capability.as_str()),
                Ok(capability)
            );
        }
        for group in [
            CapabilityGroup::Structure,
            CapabilityGroup::Binding,
            CapabilityGroup::Type,
        ] {
            assert_eq!(CapabilityGroup::parse(group.as_str()), Ok(group));
        }
        for backend in [
            SemanticBackendKind::Python,
            SemanticBackendKind::TypeScriptJavaScript,
            SemanticBackendKind::Svelte,
            SemanticBackendKind::CSharp,
            SemanticBackendKind::Rust,
        ] {
            assert_eq!(SemanticBackendKind::parse(backend.as_str()), Ok(backend));
        }

        assert!(SemanticCapability::parse("references").is_err());
        assert!(SemanticCapability::parse("PYRIGHT_HOVER").is_err());
        assert!(SemanticBackendKind::parse("REACT").is_err());
        assert!(CapabilityGroup::parse("").is_err());
    }

    #[test]
    fn capability_names_and_groups_are_complete_and_unique() {
        let mut names: Vec<&str> = SemanticCapability::ALL
            .iter()
            .map(|capability| capability.as_str())
            .collect();
        names.sort_unstable();
        let total = names.len();
        names.dedup();
        assert_eq!(names.len(), total, "no two capabilities share a name");

        let count = |group| {
            SemanticCapability::ALL
                .iter()
                .filter(|capability| capability.group() == group)
                .count()
        };
        assert_eq!(count(CapabilityGroup::Structure), 9);
        assert_eq!(count(CapabilityGroup::Binding), 7);
        assert_eq!(count(CapabilityGroup::Type), 7);
    }

    #[test]
    fn a_backend_serves_its_language_family_and_react_is_not_one() {
        assert!(SemanticBackendKind::Python.serves(ResourceLanguage::Python));
        assert!(!SemanticBackendKind::Python.serves(ResourceLanguage::Rust));

        // One backend, both languages: JSX/TSX are dialects, not
        // languages, so nothing else is needed for React.
        let ts_js = SemanticBackendKind::TypeScriptJavaScript;
        assert!(ts_js.serves(ResourceLanguage::TypeScript));
        assert!(ts_js.serves(ResourceLanguage::JavaScript));

        // Svelte is a container over embedded TS/JS semantics.
        let svelte = SemanticBackendKind::Svelte;
        assert!(svelte.serves(ResourceLanguage::Svelte));
        assert!(svelte.serves(ResourceLanguage::TypeScript));
    }

    #[test]
    fn a_context_key_is_deterministic_and_ignores_who_is_asking() {
        assert_eq!(context().context_key(), context().context_key());
        assert!(context().context_key().starts_with("sha256-ac1:"));

        // Nothing about an Agent, session, or client can be expressed in
        // the context at all, so two clients on the same Workspace,
        // language, project and toolchain reach the same backend.
        let one_agent = context();
        let another_agent = context();
        assert_eq!(one_agent.context_key(), another_agent.context_key());
    }

    #[test]
    fn every_identity_field_moves_the_context_key() {
        let base = context().context_key();

        let mut other_workspace = context();
        other_workspace.workspace = WorkspaceId::from_bytes([8; 16]);
        assert_ne!(
            other_workspace.context_key(),
            base,
            "another worktree is another context and shares no current state"
        );

        let mut other_backend = context();
        other_backend.backend = SemanticBackendKind::Rust;
        assert_ne!(other_backend.context_key(), base);

        let mut other_language = context();
        other_language.language = ResourceLanguage::TypeScript;
        assert_ne!(other_language.context_key(), base);

        let mut other_root = context();
        other_root.project_root = ProjectRootIdentity::Config(ResourceId::from_bytes([10; 16]));
        assert_ne!(other_root.context_key(), base);

        for mutate in [
            |context: &mut AnalysisContext| context.toolchain.backend_version = "9".to_owned(),
            |context: &mut AnalysisContext| {
                context.toolchain.backend_compatibility_class = "9".to_owned();
            },
            |context: &mut AnalysisContext| {
                context.toolchain.environment_fingerprint = "9".to_owned();
            },
        ] {
            let mut changed = context();
            mutate(&mut changed);
            assert_ne!(changed.context_key(), base);
        }
    }

    #[test]
    fn a_config_root_and_a_key_root_are_different_identities() {
        let mut keyed = context();
        keyed.project_root = ProjectRootIdentity::Key("workspace".to_owned());
        assert_ne!(keyed.context_key(), context().context_key());

        // The field name is part of the fingerprint, so a key that spells
        // out a Resource's id is still not that Resource.
        let mut spelled = context();
        spelled.project_root =
            ProjectRootIdentity::Key(ResourceId::from_bytes([9; 16]).to_string());
        assert_ne!(spelled.context_key(), context().context_key());
    }

    #[test]
    fn a_locator_is_current_metadata_and_not_identity() {
        let moved = AnalysisContextBinding {
            context: context(),
            project_root_rel: "services/api".to_owned(),
            config_file_rel: Some("services/api/pyproject.toml".to_owned()),
        };
        let elsewhere = AnalysisContextBinding {
            context: context(),
            project_root_rel: "vendored/services/api".to_owned(),
            config_file_rel: None,
        };

        assert_eq!(
            moved.context.context_key(),
            elsewhere.context.context_key(),
            "where the root currently sits does not change what it is"
        );
    }

    #[test]
    fn an_undeclared_capability_is_unsupported_and_never_a_silent_yes() {
        let report = CapabilityReport::new(&context());

        for capability in SemanticCapability::ALL {
            assert_eq!(report.support(capability), Support::Unsupported);
        }
        assert_eq!(report.context_key(), context().context_key());
    }

    #[test]
    fn capabilities_are_declared_independently() {
        let mut report = CapabilityReport::new(&context());
        report
            .declare(SemanticCapability::ImportBinding, Support::Supported)
            .declare(SemanticCapability::References, Support::Partial);

        assert_eq!(
            report.support(SemanticCapability::ImportBinding),
            Support::Supported
        );
        assert_eq!(
            report.support(SemanticCapability::References),
            Support::Partial
        );
        assert_eq!(
            report.support(SemanticCapability::OverloadResolution),
            Support::Unsupported,
            "supporting one capability claims nothing about another"
        );

        // A capability can be lost again -- a config change may take one
        // away, and the report has to be able to say so.
        report.declare(SemanticCapability::ImportBinding, Support::Unsupported);
        assert_eq!(
            report.support(SemanticCapability::ImportBinding),
            Support::Unsupported
        );
    }

    /// A report where every capability in `groups` is SUPPORTED.
    fn report_supporting(groups: &[CapabilityGroup]) -> CapabilityReport {
        let mut report = CapabilityReport::new(&context());
        for capability in SemanticCapability::ALL {
            if groups.contains(&capability.group()) {
                report.declare(capability, Support::Supported);
            }
        }
        report
    }

    #[test]
    fn group_support_is_decided_by_the_weakest_member() {
        let mut report = report_supporting(&[CapabilityGroup::Type]);
        assert_eq!(
            report.group_support(CapabilityGroup::Type),
            Support::Supported
        );
        assert_eq!(
            report.group_support(CapabilityGroup::Structure),
            Support::Unsupported
        );

        report.declare(SemanticCapability::OverloadResolution, Support::Partial);
        assert_eq!(
            report.group_support(CapabilityGroup::Type),
            Support::Partial,
            "one partial capability makes the whole group partial"
        );

        report.declare(SemanticCapability::Inheritance, Support::Unsupported);
        assert_eq!(
            report.group_support(CapabilityGroup::Type),
            Support::Unsupported
        );
    }

    #[test]
    fn a_later_level_grading_can_be_derived_per_context() {
        // Level A is a claim about the Type group, Level B about
        // Structure, Level C about resource discovery alone -- each read
        // off the report rather than off the language's name.
        let deep = report_supporting(&[
            CapabilityGroup::Structure,
            CapabilityGroup::Binding,
            CapabilityGroup::Type,
        ]);
        assert_eq!(
            deep.group_support(CapabilityGroup::Type),
            Support::Supported
        );

        let structural = report_supporting(&[CapabilityGroup::Structure]);
        assert_eq!(
            structural.group_support(CapabilityGroup::Structure),
            Support::Supported
        );
        assert_eq!(
            structural.group_support(CapabilityGroup::Type),
            Support::Unsupported,
            "a structural-only backend must not read as deep semantic"
        );

        let mut discovery = CapabilityReport::new(&context());
        discovery.declare(SemanticCapability::ResourceDiscovery, Support::Supported);
        assert_eq!(
            discovery.support(SemanticCapability::ResourceDiscovery),
            Support::Supported
        );
        assert_eq!(
            discovery.group_support(CapabilityGroup::Structure),
            Support::Unsupported
        );
    }

    #[test]
    fn a_capability_fingerprint_covers_every_capability() {
        let base = report_supporting(&[CapabilityGroup::Binding]);
        assert_eq!(
            base.capability_fingerprint(),
            report_supporting(&[CapabilityGroup::Binding]).capability_fingerprint()
        );

        // A capability that is *lost* changes the fingerprint, which it
        // could not do if only declared capabilities were covered.
        let mut lost = report_supporting(&[CapabilityGroup::Binding]);
        lost.declare(SemanticCapability::References, Support::Unsupported);
        assert_ne!(lost.capability_fingerprint(), base.capability_fingerprint());

        let mut weakened = report_supporting(&[CapabilityGroup::Binding]);
        weakened.declare(SemanticCapability::References, Support::Partial);
        assert_ne!(
            weakened.capability_fingerprint(),
            base.capability_fingerprint()
        );
    }

    #[test]
    fn entries_cover_every_capability_in_a_fixed_order() {
        let report = report_supporting(&[CapabilityGroup::Structure]);
        let listed: Vec<SemanticCapability> =
            report.entries().map(|(capability, _)| capability).collect();

        assert_eq!(listed, SemanticCapability::ALL.to_vec());
    }

    fn basis() -> EvidenceBasis {
        EvidenceBasis {
            owner_resource: ResourceId::from_bytes([1; 16]),
            owner_resource_revision: "4".to_owned(),
            generation_id: 12,
            analysis_profile_id: 3,
            resolution_context_key: None,
        }
    }

    fn evidence(outcome: SemanticOutcome) -> SemanticEvidence {
        SemanticEvidence {
            context_key: context().context_key(),
            capability: SemanticCapability::CallsCrossFile,
            relation_kind: Some(RelationKind::Calls),
            basis: basis(),
            occurrence: Some(OccurrenceRef {
                kind: OccurrenceKind::CallSite,
                start_byte: 40,
                end_byte: 52,
            }),
            source: Some(GraphEndpoint::Symbol(SymbolId::from_bytes([2; 16]))),
            outcome,
            support: Support::Supported,
            dispatch: Dispatch::Static,
        }
    }

    #[test]
    fn evidence_carries_brainprint_identity_and_a_resolution_axis() {
        let resolved = evidence(SemanticOutcome::Resolved {
            target: GraphEndpoint::Symbol(SymbolId::from_bytes([3; 16])),
        });
        assert_eq!(resolved.resolution(), Resolution::Resolved);
        assert_eq!(resolved.context_key, context().context_key());

        // An external target is an ExternalEntity, not indexed
        // dependency source and not a backend handle.
        let external = evidence(SemanticOutcome::Resolved {
            target: GraphEndpoint::External(ExternalEntity {
                package_identity: "requests".to_owned(),
                module_path: Some("requests.api".to_owned()),
                symbol_name: Some("get".to_owned()),
                qualified_name: Some("requests.api.get".to_owned()),
                kind: "FUNCTION".to_owned(),
                resolved_version: Some("2.31.0".to_owned()),
                declaration_locator: None,
            }),
        });
        assert_eq!(external.resolution(), Resolution::Resolved);
    }

    #[test]
    fn a_single_candidate_is_still_a_candidate() {
        let single = evidence(SemanticOutcome::Candidates {
            targets: vec![GraphEndpoint::Symbol(SymbolId::from_bytes([4; 16]))],
        });

        assert_eq!(
            single.resolution(),
            Resolution::Candidate,
            "one possible target is not a confirmed one"
        );
    }

    #[test]
    fn an_unresolved_position_keeps_its_reason_instead_of_disappearing() {
        let unresolved = evidence(SemanticOutcome::Unresolved {
            reason: UnresolvedReason::ReceiverTypeRequired,
        });

        assert_eq!(unresolved.resolution(), Resolution::Unresolved);
        assert!(
            matches!(
                unresolved.outcome,
                SemanticOutcome::Unresolved {
                    reason: UnresolvedReason::ReceiverTypeRequired
                }
            ),
            "the reason is the evidence: dropping it would be a false zero"
        );
    }
}
