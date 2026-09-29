//! Which absolute Rust import names could be a crate from this
//! Workspace (#48).
//!
//! `use brainprint_engine::config::load;` looks like a dependency on a
//! package, and in a Cargo workspace it is often source from a sibling
//! package that is indexed right here. Whether it is is Cargo's decision,
//! not the syntax's, so the structural tier must not call it a proven
//! external package. It also must not call it a local one: a name that
//! *could* be local is a candidate, and rust-analyzer decides what it
//! actually reaches.
//!
//! The candidates are read from facts the manifests state and nothing
//! else -- no Cargo command, no directory name, no product name:
//!
//! - `[package].name` and an explicit `[lib].name`, in Rust crate
//!   spelling (`-` is `_`);
//! - a dependency key (an alias) whose entry names a local `path`, names
//!   a local package through `package = "..."`, or inherits one from a
//!   `[workspace.dependencies]` table that is itself an indexed manifest.
//!
//! A manifest that cannot be read or parsed contributes no name -- it is
//! never guessed at -- and the Rust files it owns are remembered as
//! incomplete, because whether one of their imports names a local crate
//! is then not known.

use std::collections::{BTreeMap, BTreeSet};

use toml::{Table, Value};

/// The crates the language ships that no manifest names.
const SYSROOT_CRATES: [&str; 3] = ["std", "core", "alloc"];

/// One indexed `Cargo.toml`, as far as it could be read.
#[derive(Debug, Clone, Copy)]
pub struct ManifestSource<'a> {
    /// The manifest's Workspace-relative path.
    pub path_key: &'a str,
    /// Its bytes as the index describes them; `None` when they could not
    /// be read or are not the indexed ones.
    pub text: Option<&'a str>,
}

/// Whether `path_key` is a Cargo manifest.
#[must_use]
pub fn is_manifest(path_key: &str) -> bool {
    path_key == "Cargo.toml" || path_key.ends_with("/Cargo.toml")
}

/// The import names a Workspace's indexed manifests could give a local
/// crate, and where the manifests could not say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalCrateNames {
    names: BTreeSet<String>,
    /// Directories of every manifest, and whether it was readable.
    manifests: BTreeMap<String, bool>,
}

impl LocalCrateNames {
    /// Read the candidates from every indexed manifest.
    #[must_use]
    pub fn from_manifests(sources: &[ManifestSource<'_>]) -> Self {
        let mut found = Self::default();
        let mut parsed: Vec<(String, Table)> = Vec::new();
        for source in sources {
            let directory = directory_of(source.path_key);
            match source.text.and_then(|text| text.parse::<Table>().ok()) {
                Some(table) => parsed.push((directory, table)),
                None => {
                    found.manifests.insert(directory, false);
                }
            }
        }

        // Local packages first: an alias can name one through `package`.
        let mut packages: BTreeSet<String> = BTreeSet::new();
        let mut readable: Vec<(String, Table)> = Vec::new();
        for (directory, table) in parsed {
            match package_and_lib_names(&table) {
                Some((package, lib)) => {
                    packages.extend(package);
                    found.names.extend(lib);
                    found.manifests.insert(directory.clone(), true);
                    readable.push((directory, table));
                }
                // A `[package]` or `[lib]` in a form this reader does not
                // understand: nothing is guessed from it.
                None => {
                    found.manifests.insert(directory, false);
                }
            }
        }
        found.names.extend(packages.iter().cloned());

        let workspace_path_dependencies = workspace_path_dependencies(&readable, &packages);
        for (_, table) in &readable {
            for section in dependency_sections(table) {
                for (alias, entry) in section {
                    if names_a_local_crate(alias, entry, &packages, &workspace_path_dependencies) {
                        found.names.insert(crate_spelling(alias));
                    }
                }
            }
        }
        found
    }

    /// Whether an import whose first segment is `name` could be a crate
    /// from this Workspace.
    #[must_use]
    pub fn could_name(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// Whether the manifest that owns `owner_path_key` could not be read,
    /// so its imports cannot be ruled in or out.
    ///
    /// The owning manifest is the nearest `Cargo.toml` above the file --
    /// Cargo's own package membership, used only to say *where* the
    /// answer is incomplete, never to say what a name is.
    #[must_use]
    pub fn is_incomplete_for(&self, owner_path_key: &str, first_segment: &str) -> bool {
        if SYSROOT_CRATES.contains(&first_segment) {
            return false;
        }
        self.manifests
            .iter()
            .filter(|(directory, _)| {
                directory.is_empty()
                    || owner_path_key
                        .strip_prefix(directory.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|(directory, _)| directory.len())
            .is_some_and(|(_, readable)| !readable)
    }
}

/// `crates/core/Cargo.toml` -> `crates/core`; the root manifest -> ``.
fn directory_of(manifest_path_key: &str) -> String {
    manifest_path_key
        .strip_suffix("Cargo.toml")
        .unwrap_or(manifest_path_key)
        .trim_end_matches('/')
        .to_owned()
}

/// A Cargo package or dependency name as `use` spells the crate.
fn crate_spelling(name: &str) -> String {
    name.replace('-', "_")
}

/// `(package name, lib name)`, each in crate spelling. `None` when either
/// section is present in a form this reader does not understand.
fn package_and_lib_names(table: &Table) -> Option<(Option<String>, Option<String>)> {
    let package = match table.get("package") {
        None => None,
        Some(Value::Table(package)) => match package.get("name") {
            Some(Value::String(name)) => Some(crate_spelling(name)),
            _ => return None,
        },
        Some(_) => return None,
    };
    let lib = match table.get("lib") {
        None => None,
        Some(Value::Table(lib)) => match lib.get("name") {
            None => None,
            Some(Value::String(name)) => Some(crate_spelling(name)),
            Some(_) => return None,
        },
        Some(_) => return None,
    };
    Some((package, lib))
}

/// Every table of dependencies one manifest declares.
fn dependency_sections(table: &Table) -> Vec<&Table> {
    fn declared(parent: &Table) -> impl Iterator<Item = &Table> {
        ["dependencies", "dev-dependencies", "build-dependencies"]
            .into_iter()
            .filter_map(|key| match parent.get(key) {
                Some(Value::Table(section)) => Some(section),
                _ => None,
            })
    }
    let mut sections: Vec<&Table> = declared(table).collect();
    if let Some(Value::Table(targets)) = table.get("target") {
        for target in targets.values() {
            if let Value::Table(target) = target {
                sections.extend(declared(target));
            }
        }
    }
    if let Some(Value::Table(workspace)) = table.get("workspace")
        && let Some(Value::Table(section)) = workspace.get("dependencies")
    {
        sections.push(section);
    }
    sections
}

/// Whether one dependency entry names a crate from this Workspace.
fn names_a_local_crate(
    alias: &str,
    entry: &Value,
    packages: &BTreeSet<String>,
    workspace_path_dependencies: &BTreeSet<String>,
) -> bool {
    let Value::Table(entry) = entry else {
        return false;
    };
    if matches!(entry.get("path"), Some(Value::String(_))) {
        return true;
    }
    if let Some(Value::String(package)) = entry.get("package") {
        return packages.contains(&crate_spelling(package));
    }
    matches!(entry.get("workspace"), Some(Value::Boolean(true)))
        && workspace_path_dependencies.contains(alias)
}

/// The aliases a `[workspace.dependencies]` table declares as local, over
/// every indexed manifest. Inheritance is followed only through these.
fn workspace_path_dependencies(
    manifests: &[(String, Table)],
    packages: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for (_, table) in manifests {
        let Some(Value::Table(workspace)) = table.get("workspace") else {
            continue;
        };
        let Some(Value::Table(dependencies)) = workspace.get("dependencies") else {
            continue;
        };
        for (alias, entry) in dependencies {
            if names_a_local_crate(alias, entry, packages, &BTreeSet::new()) {
                found.insert(alias.clone());
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(manifests: &[(&str, &str)]) -> LocalCrateNames {
        let sources: Vec<ManifestSource<'_>> = manifests
            .iter()
            .map(|(path_key, text)| ManifestSource {
                path_key,
                text: Some(text),
            })
            .collect();
        LocalCrateNames::from_manifests(&sources)
    }

    #[test]
    fn a_package_name_is_a_candidate_in_crate_spelling() {
        let found = names(&[(
            "crates/engine/Cargo.toml",
            "[package]\nname = \"my-engine\"\n",
        )]);
        assert!(found.could_name("my_engine"));
        assert!(
            !found.could_name("my-engine"),
            "`-` is never how `use` spells it"
        );
        assert!(!found.could_name("serde"));
    }

    #[test]
    fn an_explicit_lib_name_is_a_candidate_beside_the_package_name() {
        let found = names(&[(
            "crates/x/Cargo.toml",
            "[package]\nname = \"x-pkg\"\n[lib]\nname = \"x_lib\"\n",
        )]);
        assert!(found.could_name("x_lib"));
        assert!(found.could_name("x_pkg"));
    }

    #[test]
    fn a_path_dependency_alias_is_a_candidate_and_a_registry_one_is_not() {
        let found = names(&[
            ("crates/engine/Cargo.toml", "[package]\nname = \"engine\"\n"),
            (
                "crates/app/Cargo.toml",
                "[package]\nname = \"app\"\n\
                 [dependencies]\n\
                 eng = { package = \"engine\", path = \"../engine\" }\n\
                 helper = { path = \"../helper\" }\n\
                 serde = \"1\"\n\
                 rand = { version = \"0.9\", features = [\"std\"] }\n\
                 [dev-dependencies.dev-helper]\npath = \"../dev\"\n\
                 [target.'cfg(unix)'.dependencies]\nunix-helper = { path = \"../unix\" }\n",
            ),
        ]);
        for local in [
            "eng",
            "helper",
            "dev_helper",
            "unix_helper",
            "engine",
            "app",
        ] {
            assert!(found.could_name(local), "{local}");
        }
        for external in ["serde", "rand"] {
            assert!(
                !found.could_name(external),
                "{external} is an ordinary crate"
            );
        }
    }

    #[test]
    fn inheritance_is_followed_only_through_an_indexed_workspace_table() {
        let root = "[workspace.dependencies]\n\
                    shared = { path = \"crates/shared\" }\n\
                    serde = \"1\"\n";
        let member = "[package]\nname = \"m\"\n[dependencies]\n\
                      shared.workspace = true\nserde.workspace = true\n";
        let with_root = names(&[("Cargo.toml", root), ("crates/m/Cargo.toml", member)]);
        assert!(with_root.could_name("shared"));
        assert!(
            !with_root.could_name("serde"),
            "an inherited registry crate"
        );

        // The workspace table is not indexed: nothing is followed, so
        // nothing is guessed.
        let without_root = names(&[("crates/m/Cargo.toml", member)]);
        assert!(!without_root.could_name("shared"));
    }

    #[test]
    fn a_name_collision_only_makes_a_candidate() {
        // A local package named like a registry crate is one name; the
        // inventory cannot say which Cargo picked, and does not try.
        let found = names(&[
            ("crates/a/Cargo.toml", "[package]\nname = \"serde\"\n"),
            (
                "crates/b/Cargo.toml",
                "[package]\nname = \"b\"\n[dependencies]\nserde = \"1\"\n",
            ),
        ]);
        assert!(found.could_name("serde"));
    }

    #[test]
    fn a_manifest_that_cannot_be_read_guesses_nothing_and_marks_its_files() {
        let sources = [
            ManifestSource {
                path_key: "crates/ok/Cargo.toml",
                text: Some("[package]\nname = \"ok\"\n"),
            },
            ManifestSource {
                path_key: "crates/broken/Cargo.toml",
                text: Some("[package\nname = "),
            },
            ManifestSource {
                path_key: "crates/gone/Cargo.toml",
                text: None,
            },
            ManifestSource {
                path_key: "crates/odd/Cargo.toml",
                text: Some("[package]\nname.workspace = true\n"),
            },
        ];
        let found = LocalCrateNames::from_manifests(&sources);
        assert!(found.could_name("ok"));
        for guessed in ["broken", "gone", "odd"] {
            assert!(!found.could_name(guessed), "{guessed} is never guessed");
        }
        assert!(!found.is_incomplete_for("crates/ok/src/lib.rs", "serde"));
        for owned in [
            "crates/broken/src/lib.rs",
            "crates/gone/src/a/b.rs",
            "crates/odd/src/main.rs",
        ] {
            assert!(found.is_incomplete_for(owned, "serde"), "{owned}");
            assert!(
                !found.is_incomplete_for(owned, "std"),
                "the language's own crates are never in question"
            );
        }
        // A file outside every manifest's directory is not affected.
        assert!(!found.is_incomplete_for("scripts/tool.rs", "serde"));
    }

    #[test]
    fn the_nearest_manifest_owns_a_file() {
        let sources = [
            ManifestSource {
                path_key: "Cargo.toml",
                text: Some("[package\n"),
            },
            ManifestSource {
                path_key: "crates/ok/Cargo.toml",
                text: Some("[package]\nname = \"ok\"\n"),
            },
        ];
        let found = LocalCrateNames::from_manifests(&sources);
        assert!(
            found.is_incomplete_for("src/main.rs", "serde"),
            "the root package's own file"
        );
        assert!(
            !found.is_incomplete_for("crates/ok/src/lib.rs", "serde"),
            "a readable manifest nearer the file owns it"
        );
    }

    #[test]
    fn a_virtual_workspace_manifest_is_readable_and_names_nothing_itself() {
        let found = names(&[("Cargo.toml", "[workspace]\nmembers = [\"crates/a\"]\n")]);
        assert!(!found.could_name("crates"));
        assert!(!found.is_incomplete_for("crates/a/src/lib.rs", "serde"));
    }
}
