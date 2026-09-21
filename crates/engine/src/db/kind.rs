//! Canonical DB kind discriminant shared by all Brainprint SQLite databases.

use std::fmt;

/// The four physically separate SQLite databases Brainprint owns.
///
/// Per design issue #13 task 5, `global`/`project`/`workspace`/`index` are
/// never merged into one file, and each carries its own schema version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DbKind {
    Global,
    Project,
    Workspace,
    Index,
}

impl DbKind {
    /// Canonical lowercase identifier stored in `db_meta.db_kind`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::Workspace => "workspace",
            Self::Index => "index",
        }
    }
}

impl fmt::Display for DbKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_have_distinct_canonical_names() {
        let kinds = [
            DbKind::Global,
            DbKind::Project,
            DbKind::Workspace,
            DbKind::Index,
        ];
        let names: Vec<&str> = kinds.iter().map(|kind| kind.as_str()).collect();

        for (index, name) in names.iter().enumerate() {
            assert!(
                names[index + 1..].iter().all(|other| other != name),
                "duplicate DB kind name: {name}"
            );
        }
    }
}
