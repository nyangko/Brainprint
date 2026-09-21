//! Workspace root discovery + ignore/exclusion + supported resource
//! enumeration (#16 task 2).
//!
//! Scope: this module walks a Workspace root and turns it into a flat list
//! of [`DiscoveredResource`] candidates, classified by task 1's canonical
//! [`ResourceKind`]/[`ResourceRole`]/[`ResourceLanguage`] vocabulary
//! ([`crate::resource`]) -- it adds no new vocabulary and redesigns
//! nothing there. It has no knowledge of:
//! - fingerprint/revision computation, stable `ResourceId` reuse, or
//!   create/modify/delete/move identity decisions (#16 task 3)
//! - `index.db` persistence/orchestration (#16 task 3-4)
//! - watcher/reconcile, Tree-sitter, Symbol/Occurrence, Relation (#16 task
//!   5+)
//!
//! "Workspace root discovery" is [`crate::paths::WorkspacePaths::workspace_root`]
//! (already established by #15 task 6-7's init) -- this module's own
//! contribution is walking it correctly: `.brainprint` (Brainprint's own
//! runtime/data directory, which lives inside the Workspace root) is
//! always excluded, alongside the rest of the deterministic default list.
//!
//! Exclusion is deterministic and name-based only (#16 task 2 "Discovery /
//! Ignore" §1-2): no `.gitignore` parsing or glob engine is implemented
//! here. Two tiers (#16 task 2 acceptance correction -- "이름만 보고 모든
//! 동명 디렉터리를 무조건 제외하는 과잉 규칙은 피하고, project/config
//! evidence를 이용할 수 있게 구조를 둔다"):
//! - [`UNCONDITIONAL_EXCLUDED_DIR_NAMES`] (plus
//!   [`crate::config::WorkspaceConfig::extra_excluded_directory_names`]):
//!   the name alone is strong evidence -- pruned at every depth.
//! - [`CONTEXTUAL_EXCLUDED_DIR_NAMES`][]: `build`/`dist`/`target`/`bin`/`obj`
//!   are common derived-output names but also plausible ordinary source
//!   directory names, so they are pruned only where their parent
//!   directory also holds a matching ecosystem project marker (e.g.
//!   `target` next to `Cargo.toml`, `bin`/`obj` next to a `.csproj`/`.sln`
//!   file). A `src/target/` or `domain/build/` with no such sibling marker
//!   is left in the tree: false exclusion is worse than false inclusion.
//!
//! Classification never guesses a language for a non-code Resource:
//! `language: None` is preserved for anything outside the closed
//! [`ResourceLanguage`] set (the schema column is nullable, not
//! "unknown").

use std::{collections::HashSet, error::Error, fmt, fs, path::Path, path::PathBuf};

use crate::{
    config::WorkspaceConfig,
    resource::{ResourceKind, ResourceLanguage, ResourceRole},
};

/// Directory names pruned at every depth, regardless of location -- the
/// name itself is strong, unambiguous evidence (#16 task 2 "기본 제외
/// 후보"). `.brainprint` is Brainprint's own runtime/data directory, never
/// project-owned source (#16 "Locked 입력").
const UNCONDITIONAL_EXCLUDED_DIR_NAMES: &[&str] = &[
    ".git",
    ".brainprint",
    "node_modules",
    "venv",
    ".venv",
    "virtualenv",
];

/// A sibling file that counts as project/config evidence for a
/// [`CONTEXTUAL_EXCLUDED_DIR_NAMES`] candidate.
enum ProjectMarker {
    /// Exact, case-sensitive file name.
    FileName(&'static str),
    /// File extension, compared case-insensitively.
    Extension(&'static str),
}

const RUST_MARKERS: &[ProjectMarker] = &[ProjectMarker::FileName("Cargo.toml")];
const DOTNET_MARKERS: &[ProjectMarker] = &[
    ProjectMarker::Extension("csproj"),
    ProjectMarker::Extension("sln"),
];
const BUILD_OUTPUT_MARKERS: &[ProjectMarker] = &[
    ProjectMarker::FileName("package.json"),
    ProjectMarker::FileName("pyproject.toml"),
    ProjectMarker::FileName("setup.py"),
    ProjectMarker::FileName("setup.cfg"),
    ProjectMarker::FileName("Cargo.toml"),
];

/// Directory names that are common derived-output locations but are also
/// plausible ordinary source directory names. Pruned only when the
/// candidate's *parent* directory also contains one of the listed sibling
/// markers for that ecosystem (#16 task 2 acceptance correction).
const CONTEXTUAL_EXCLUDED_DIR_NAMES: &[(&str, &[ProjectMarker])] = &[
    ("target", RUST_MARKERS),
    ("bin", DOTNET_MARKERS),
    ("obj", DOTNET_MARKERS),
    ("build", BUILD_OUTPUT_MARKERS),
    ("dist", BUILD_OUTPUT_MARKERS),
];

const TEST_DIR_NAMES: &[&str] = &["test", "tests", "__tests__", "spec", "specs"];

const CONFIG_FILE_NAMES: &[&str] = &[
    "cargo.toml",
    "cargo.lock",
    "package.json",
    "package-lock.json",
    "pyproject.toml",
    "tsconfig.json",
    "jsconfig.json",
    ".gitignore",
    ".gitattributes",
    ".editorconfig",
    ".brainprintignore",
];
const CONFIG_EXTENSIONS: &[&str] = &["toml", "yaml", "yml", "ini", "cfg", "conf", "csproj"];

const DOCS_FILE_STEMS: &[&str] = &["readme", "changelog", "license", "contributing", "authors"];
const DOCS_EXTENSIONS: &[&str] = &["md", "mdx", "rst", "adoc"];

const ASSET_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "svg", "ico", "bmp", "webp", "woff", "woff2", "ttf", "otf", "pdf",
    "mp4", "mp3", "wav", "wasm", "zip",
];

/// One filesystem entry classified into task 1's Resource vocabulary, not
/// yet persisted (no `ResourceId`, fingerprint, or revision -- #16 task
/// 3's job).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredResource {
    pub path_rel: String,
    pub path_key: String,
    pub kind: ResourceKind,
    pub role: ResourceRole,
    pub language: Option<ResourceLanguage>,
}

/// Failure walking the Workspace root.
#[derive(Debug)]
pub enum DiscoveryError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "discovery I/O failed at {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl Error for DiscoveryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
        }
    }
}

/// Walk `workspace_root`, applying the unconditional exclusion list plus
/// `config.extra_excluded_directory_names` (both location-independent, per
/// explicit user/deterministic-name evidence), the marker-gated contextual
/// exclusion list, and classify every retained entry. Results are ordered
/// by `path_key` for determinism.
pub fn enumerate_resources(
    workspace_root: &Path,
    config: &WorkspaceConfig,
) -> Result<Vec<DiscoveredResource>, DiscoveryError> {
    let excluded_names = excluded_name_set(config);

    let mut resources = Vec::new();
    walk(
        workspace_root,
        workspace_root,
        &excluded_names,
        &mut resources,
    )?;
    resources.sort_by(|a, b| a.path_key.cmp(&b.path_key));
    Ok(resources)
}

/// Whether `path_rel` lies under any excluded directory, for a caller that
/// has one path rather than a whole walk -- a watcher event (#16 task 5).
///
/// Applies the same two-tier rule as [`enumerate_resources`], component by
/// component, so watcher noise from `.git`, `node_modules`, or an
/// evidenced build output directory never reaches the index. The final
/// component is checked too: its kind may be unknowable (the entry may
/// already be gone), and a *file* named `node_modules` or `target` is not
/// worth a stat call to rescue. A parent directory that cannot be read
/// yields no marker evidence, which -- as in [`enumerate_resources`] --
/// means "not excluded": false exclusion is worse than false inclusion.
pub fn is_ignored_path(workspace_root: &Path, path_rel: &str, config: &WorkspaceConfig) -> bool {
    let excluded_names = excluded_name_set(config);

    let mut parent = workspace_root.to_path_buf();
    for name in path_rel.split('/').filter(|name| !name.is_empty()) {
        let siblings: Vec<fs::DirEntry> = fs::read_dir(&parent)
            .map(|entries| entries.filter_map(Result::ok).collect())
            .unwrap_or_default();
        if is_excluded_dir(name, &siblings, &excluded_names) {
            return true;
        }
        parent.push(name);
    }
    false
}

/// Classify one existing path the way [`enumerate_resources`] would, for a
/// caller holding a single path (#16 task 5). `Ok(None)` means the path is
/// not currently a file or directory -- typically already deleted -- which
/// is a fact, not an error.
pub fn describe_path(
    workspace_root: &Path,
    path_rel: &str,
) -> Result<Option<DiscoveredResource>, DiscoveryError> {
    let full = workspace_root.join(path_rel);
    let metadata = match fs::symlink_metadata(&full) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error(&full, source)),
    };

    // Symlinks are skipped by the walk, so they are not Resources here
    // either.
    let kind = if metadata.is_dir() {
        ResourceKind::Directory
    } else if metadata.is_file() {
        ResourceKind::File
    } else {
        return Ok(None);
    };

    let (role, language) = match kind {
        ResourceKind::Directory => (ResourceRole::Unknown, None),
        ResourceKind::File => classify(path_rel),
    };
    Ok(Some(DiscoveredResource {
        path_rel: path_rel.to_owned(),
        path_key: path_rel.to_owned(),
        kind,
        role,
        language,
    }))
}

fn excluded_name_set(config: &WorkspaceConfig) -> HashSet<&str> {
    UNCONDITIONAL_EXCLUDED_DIR_NAMES
        .iter()
        .copied()
        .chain(
            config
                .extra_excluded_directory_names
                .iter()
                .map(String::as_str),
        )
        .collect()
}

fn walk(
    root: &Path,
    current: &Path,
    excluded_names: &HashSet<&str>,
    out: &mut Vec<DiscoveredResource>,
) -> Result<(), DiscoveryError> {
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(current)
        .map_err(|source| io_error(current, source))?
        .collect::<Result<_, _>>()
        .map_err(|source| io_error(current, source))?;
    entries.sort_by_key(fs::DirEntry::file_name);

    for entry in &entries {
        let file_type = entry
            .file_type()
            .map_err(|source| io_error(&entry.path(), source))?;
        // Symlinks are skipped outright: neither indexed as their target's
        // kind nor followed (avoids cycles); not requested by this task.
        if file_type.is_symlink() {
            continue;
        }

        let path = entry.path();
        let path_rel = relative_slash_path(root, &path);

        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if is_excluded_dir(&name, &entries, excluded_names) {
                continue;
            }
            out.push(DiscoveredResource {
                path_rel: path_rel.clone(),
                path_key: path_rel,
                kind: ResourceKind::Directory,
                role: ResourceRole::Unknown,
                language: None,
            });
            walk(root, &path, excluded_names, out)?;
        } else if file_type.is_file() {
            let (role, language) = classify(&path_rel);
            out.push(DiscoveredResource {
                path_rel: path_rel.clone(),
                path_key: path_rel,
                kind: ResourceKind::File,
                role,
                language,
            });
        }
        // Anything else (device nodes, FIFOs, ...) is neither a file nor a
        // directory Resource and is skipped.
    }
    Ok(())
}

/// Whether directory `name` should be pruned: unconditionally (strong
/// name evidence, incl. user config), or -- for a
/// [`CONTEXTUAL_EXCLUDED_DIR_NAMES`] candidate -- only when `siblings`
/// (its parent directory's own entries) contain a matching project marker.
fn is_excluded_dir(name: &str, siblings: &[fs::DirEntry], excluded_names: &HashSet<&str>) -> bool {
    if excluded_names.contains(name) {
        return true;
    }
    CONTEXTUAL_EXCLUDED_DIR_NAMES
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .is_some_and(|(_, markers)| sibling_has_marker(siblings, markers))
}

fn sibling_has_marker(siblings: &[fs::DirEntry], markers: &[ProjectMarker]) -> bool {
    siblings.iter().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        markers.iter().any(|marker| match marker {
            ProjectMarker::FileName(expected) => name.as_ref() == *expected,
            ProjectMarker::Extension(extension) => Path::new(name.as_ref())
                .extension()
                .and_then(|found| found.to_str())
                .is_some_and(|found| found.eq_ignore_ascii_case(extension)),
        })
    })
}

fn io_error(path: &Path, source: std::io::Error) -> DiscoveryError {
    DiscoveryError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn relative_slash_path(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn classify(path_rel: &str) -> (ResourceRole, Option<ResourceLanguage>) {
    let file_name = path_rel
        .rsplit('/')
        .next()
        .unwrap_or(path_rel)
        .to_lowercase();
    let extension = Path::new(&file_name)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_owned);
    let stem = Path::new(&file_name)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(&file_name)
        .to_owned();
    let language = extension.as_deref().and_then(language_for_extension);

    let in_test_dir = path_rel
        .split('/')
        .any(|component| TEST_DIR_NAMES.contains(&component.to_lowercase().as_str()));
    if in_test_dir || is_test_file_name(&file_name) {
        return (ResourceRole::Test, language);
    }

    if CONFIG_FILE_NAMES.contains(&file_name.as_str())
        || extension
            .as_deref()
            .is_some_and(|value| CONFIG_EXTENSIONS.contains(&value))
    {
        return (ResourceRole::Config, None);
    }

    if DOCS_FILE_STEMS.contains(&stem.as_str())
        || extension
            .as_deref()
            .is_some_and(|value| DOCS_EXTENSIONS.contains(&value))
    {
        return (ResourceRole::Docs, None);
    }

    if let Some(language) = language {
        return (ResourceRole::Source, Some(language));
    }

    if extension
        .as_deref()
        .is_some_and(|value| ASSET_EXTENSIONS.contains(&value))
    {
        return (ResourceRole::Asset, None);
    }

    (ResourceRole::Unknown, None)
}

fn is_test_file_name(lower_name: &str) -> bool {
    let stem = Path::new(lower_name)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(lower_name);
    stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with(".test")
        || stem.ends_with("_spec")
        || stem.ends_with(".spec")
}

fn language_for_extension(extension: &str) -> Option<ResourceLanguage> {
    match extension {
        "py" | "pyi" => Some(ResourceLanguage::Python),
        "js" | "mjs" | "cjs" | "jsx" => Some(ResourceLanguage::JavaScript),
        "ts" | "mts" | "cts" | "tsx" => Some(ResourceLanguage::TypeScript),
        "svelte" => Some(ResourceLanguage::Svelte),
        "cs" => Some(ResourceLanguage::CSharp),
        "rs" => Some(ResourceLanguage::Rust),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env, process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "brainprint-discovery-{label}-{}-{sequence}",
                process::id()
            ));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, rel: &str, contents: &str) {
            let full = self.0.join(rel);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).expect("parent dirs should be creatable");
            }
            fs::write(full, contents).expect("fixture file should be writable");
        }

        fn mkdir(&self, rel: &str) {
            fs::create_dir_all(self.0.join(rel)).expect("fixture dir should be creatable");
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn find<'a>(resources: &'a [DiscoveredResource], path_rel: &str) -> &'a DiscoveredResource {
        resources
            .iter()
            .find(|resource| resource.path_rel == path_rel)
            .unwrap_or_else(|| panic!("expected discovered resource for {path_rel}"))
    }

    fn contains(resources: &[DiscoveredResource], path_rel: &str) -> bool {
        resources
            .iter()
            .any(|resource| resource.path_rel == path_rel)
    }

    #[test]
    fn unconditional_excluded_directories_are_pruned_entirely() {
        let dir = TestDir::create("unconditional-exclusion");
        dir.write("src/lib.rs", "");
        dir.write("node_modules/pkg/index.js", "");
        dir.write(".git/HEAD", "");
        dir.write(".brainprint/data/index.db", "");
        dir.write("venv/lib/site.py", "");

        let resources = enumerate_resources(dir.path(), &WorkspaceConfig::default())
            .expect("enumeration should succeed");

        assert!(contains(&resources, "src/lib.rs"));
        for excluded in ["node_modules", ".git", ".brainprint", "venv"] {
            assert!(
                !resources
                    .iter()
                    .any(|resource| resource.path_rel.starts_with(excluded)),
                "{excluded} must be fully pruned"
            );
        }
    }

    #[test]
    fn contextual_derived_output_dirs_are_pruned_only_with_matching_project_marker() {
        let dir = TestDir::create("contextual-exclusion-positive");
        // Rust workspace root: target/ next to Cargo.toml.
        dir.write("Cargo.toml", "");
        dir.write("target/debug/build.log", "");
        // .NET project: bin/, obj/ next to a .csproj.
        dir.write("App.csproj", "");
        dir.write("bin/Debug/app.dll", "");
        dir.write("obj/Debug/app.cache", "");
        // JS project: build/, dist/ next to package.json.
        dir.write("web/package.json", "");
        dir.write("web/build/bundle.js", "");
        dir.write("web/dist/bundle.min.js", "");

        let resources = enumerate_resources(dir.path(), &WorkspaceConfig::default())
            .expect("enumeration should succeed");

        for excluded in ["target", "bin", "obj", "web/build", "web/dist"] {
            assert!(
                !resources
                    .iter()
                    .any(|resource| resource.path_rel.starts_with(excluded)),
                "{excluded} with a matching project marker must be pruned"
            );
        }
        assert!(contains(&resources, "Cargo.toml"));
        assert!(contains(&resources, "App.csproj"));
        assert!(contains(&resources, "web/package.json"));
    }

    #[test]
    fn contextual_derived_output_dirs_without_a_project_marker_are_not_excluded() {
        let dir = TestDir::create("contextual-exclusion-negative");
        // No Cargo.toml/.csproj/.sln/package.json anywhere: these are
        // plausible ordinary source subtrees, not derived output.
        dir.write("src/target/generated.rs", "");
        dir.write("domain/build/model.rs", "");
        dir.write("app/bin/launcher.py", "");
        dir.write("lib/dist/index.py", "");
        dir.write("service/obj/mapper.py", "");

        let resources = enumerate_resources(dir.path(), &WorkspaceConfig::default())
            .expect("enumeration should succeed");

        for retained in [
            "src/target/generated.rs",
            "domain/build/model.rs",
            "app/bin/launcher.py",
            "lib/dist/index.py",
            "service/obj/mapper.py",
        ] {
            assert!(
                contains(&resources, retained),
                "{retained} has no sibling project marker and must not be excluded \
                 by name alone -- false exclusion must be avoided"
            );
        }
    }

    #[test]
    fn user_extra_excluded_names_prune_regardless_of_location_or_markers() {
        let dir = TestDir::create("user-exclusion-any-location");
        dir.write("nested/deep/vendor/thirdparty.rs", "");
        dir.write("nested/deep/kept.rs", "");

        let mut config = WorkspaceConfig::default();
        config
            .extra_excluded_directory_names
            .push("vendor".to_owned());

        let resources =
            enumerate_resources(dir.path(), &config).expect("enumeration should succeed");

        assert!(contains(&resources, "nested/deep/kept.rs"));
        assert!(
            !resources
                .iter()
                .any(|resource| resource.path_rel.contains("vendor"))
        );
    }

    #[test]
    fn workspace_config_extends_exclusion() {
        let dir = TestDir::create("config-exclusion");
        dir.write("src/lib.rs", "");
        dir.write("vendor/thirdparty.rs", "");

        let mut config = WorkspaceConfig::default();
        config
            .extra_excluded_directory_names
            .push("vendor".to_owned());

        let resources =
            enumerate_resources(dir.path(), &config).expect("enumeration should succeed");

        assert!(contains(&resources, "src/lib.rs"));
        assert!(!resources.iter().any(|r| r.path_rel.starts_with("vendor")));
    }

    #[test]
    fn source_files_are_classified_by_extension() {
        let dir = TestDir::create("source-classification");
        dir.write("a.rs", "");
        dir.write("b.py", "");
        dir.write("c.ts", "");
        dir.write("d.tsx", "");
        dir.write("e.js", "");
        dir.write("f.jsx", "");
        dir.write("g.svelte", "");
        dir.write("h.cs", "");

        let resources = enumerate_resources(dir.path(), &WorkspaceConfig::default())
            .expect("enumeration should succeed");

        let expectations: &[(&str, ResourceLanguage)] = &[
            ("a.rs", ResourceLanguage::Rust),
            ("b.py", ResourceLanguage::Python),
            ("c.ts", ResourceLanguage::TypeScript),
            ("d.tsx", ResourceLanguage::TypeScript),
            ("e.js", ResourceLanguage::JavaScript),
            ("f.jsx", ResourceLanguage::JavaScript),
            ("g.svelte", ResourceLanguage::Svelte),
            ("h.cs", ResourceLanguage::CSharp),
        ];
        for (path_rel, language) in expectations {
            let resource = find(&resources, path_rel);
            assert_eq!(resource.role, ResourceRole::Source);
            assert_eq!(resource.language, Some(*language));
        }
    }

    #[test]
    fn test_directory_and_test_file_names_are_classified_as_test() {
        let dir = TestDir::create("test-classification");
        dir.write("tests/integration.rs", "");
        dir.write("src/test_utils.py", "");
        dir.write("src/app.test.ts", "");
        dir.write("src/app.spec.ts", "");

        let resources = enumerate_resources(dir.path(), &WorkspaceConfig::default())
            .expect("enumeration should succeed");

        for path_rel in [
            "tests/integration.rs",
            "src/test_utils.py",
            "src/app.test.ts",
            "src/app.spec.ts",
        ] {
            assert_eq!(find(&resources, path_rel).role, ResourceRole::Test);
        }
    }

    #[test]
    fn known_config_and_docs_files_are_classified_without_a_language() {
        let dir = TestDir::create("config-docs-classification");
        dir.write("Cargo.toml", "");
        dir.write(".gitignore", "");
        dir.write("README.md", "");
        dir.write("LICENSE", "");

        let resources = enumerate_resources(dir.path(), &WorkspaceConfig::default())
            .expect("enumeration should succeed");

        assert_eq!(find(&resources, "Cargo.toml").role, ResourceRole::Config);
        assert_eq!(find(&resources, ".gitignore").role, ResourceRole::Config);
        assert_eq!(find(&resources, "README.md").role, ResourceRole::Docs);
        assert_eq!(find(&resources, "LICENSE").role, ResourceRole::Docs);

        for path_rel in ["Cargo.toml", ".gitignore", "README.md", "LICENSE"] {
            assert_eq!(
                find(&resources, path_rel).language,
                None,
                "{path_rel} must not be assigned a fake language"
            );
        }
    }

    #[test]
    fn asset_files_are_classified_without_a_language() {
        let dir = TestDir::create("asset-classification");
        dir.write("logo.png", "");

        let resources = enumerate_resources(dir.path(), &WorkspaceConfig::default())
            .expect("enumeration should succeed");

        let resource = find(&resources, "logo.png");
        assert_eq!(resource.role, ResourceRole::Asset);
        assert_eq!(resource.language, None);
    }

    #[test]
    fn unrecognized_files_fall_back_to_unknown_without_guessing() {
        let dir = TestDir::create("unknown-classification");
        dir.write("mystery.xyz", "");
        dir.write("no_extension_at_all", "");

        let resources = enumerate_resources(dir.path(), &WorkspaceConfig::default())
            .expect("enumeration should succeed");

        for path_rel in ["mystery.xyz", "no_extension_at_all"] {
            let resource = find(&resources, path_rel);
            assert_eq!(resource.role, ResourceRole::Unknown);
            assert_eq!(resource.language, None);
        }
    }

    #[test]
    fn retained_directories_are_emitted_as_directory_kind_resources() {
        let dir = TestDir::create("directory-kind");
        dir.write("src/lib.rs", "");

        let resources = enumerate_resources(dir.path(), &WorkspaceConfig::default())
            .expect("enumeration should succeed");

        let src_dir = find(&resources, "src");
        assert_eq!(src_dir.kind, ResourceKind::Directory);
        assert_eq!(src_dir.language, None);
    }

    #[test]
    fn empty_directories_are_still_discovered() {
        let dir = TestDir::create("empty-directory");
        dir.mkdir("docs");

        let resources = enumerate_resources(dir.path(), &WorkspaceConfig::default())
            .expect("enumeration should succeed");

        assert_eq!(find(&resources, "docs").kind, ResourceKind::Directory);
    }

    #[test]
    fn results_are_ordered_by_path_key() {
        let dir = TestDir::create("ordering");
        dir.write("b.rs", "");
        dir.write("a.rs", "");
        dir.write("c/d.rs", "");

        let resources = enumerate_resources(dir.path(), &WorkspaceConfig::default())
            .expect("enumeration should succeed");

        let mut sorted = resources.clone();
        sorted.sort_by(|a, b| a.path_key.cmp(&b.path_key));
        assert_eq!(resources, sorted);
    }

    #[test]
    fn path_rel_and_path_key_use_forward_slashes_and_no_workspace_root_prefix() {
        let dir = TestDir::create("path-shape");
        dir.write("nested/dir/file.rs", "");

        let resources = enumerate_resources(dir.path(), &WorkspaceConfig::default())
            .expect("enumeration should succeed");

        let resource = find(&resources, "nested/dir/file.rs");
        assert_eq!(resource.path_key, resource.path_rel);
        assert!(!resource.path_rel.contains('\\'));
        assert!(!resource.path_rel.starts_with('/'));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_skipped() {
        use std::os::unix::fs::symlink;

        let dir = TestDir::create("symlink-skip");
        dir.write("real.rs", "");
        symlink(dir.path().join("real.rs"), dir.path().join("link.rs"))
            .expect("symlink should be creatable");

        let resources = enumerate_resources(dir.path(), &WorkspaceConfig::default())
            .expect("enumeration should succeed");

        assert!(contains(&resources, "real.rs"));
        assert!(!contains(&resources, "link.rs"));
    }
}
