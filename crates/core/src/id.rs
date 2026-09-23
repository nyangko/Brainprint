//! Stable identities shared across Brainprint storage and protocol boundaries.

use std::{error::Error, fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

/// Error returned when a stable Brainprint identity cannot be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseStableIdError;

impl fmt::Display for ParseStableIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid Brainprint stable identity")
    }
}

impl Error for ParseStableIdError {}

macro_rules! define_stable_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[repr(transparent)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Uuid);

        impl $name {
            /// Generate a new stable identity.
            #[must_use]
            pub fn generate() -> Self {
                Self(Uuid::new_v4())
            }

            /// Construct an identity from its canonical 16-byte storage form.
            #[must_use]
            pub fn from_bytes(bytes: [u8; 16]) -> Self {
                Self(Uuid::from_bytes(bytes))
            }

            /// Return the canonical 16-byte storage form.
            #[must_use]
            pub fn to_bytes(self) -> [u8; 16] {
                *self.0.as_bytes()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.hyphenated().fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = ParseStableIdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value)
                    .map(Self)
                    .map_err(|_| ParseStableIdError)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

define_stable_id!(
    ProjectId,
    "Stable identity for one Brainprint project lineage."
);
define_stable_id!(
    WorkspaceId,
    "Stable identity for one mutable workspace or worktree."
);
define_stable_id!(
    ResourceId,
    "Stable identity for one project-owned resource across supported moves."
);
define_stable_id!(
    SymbolId,
    "Stable identity for one declared symbol across edits that do not redeclare it."
);
define_stable_id!(
    LogicalSymbolId,
    "Stable identity for one *semantic* symbol that a language may declare more \
     than once -- a C# partial type, and whatever later languages merge the same \
     way. It groups existing `SymbolId` declarations; it never replaces them."
);

define_stable_id!(
    PolicyId,
    "Stable identity for one project Policy or global user Policy."
);
define_stable_id!(DecisionId, "Stable identity for one project Decision.");
define_stable_id!(
    BlueprintId,
    "Stable identity for one Blueprint definition, global or project-local."
);
define_stable_id!(
    BlueprintApplicationId,
    "Stable identity for one Blueprint application inside a Project."
);
define_stable_id!(
    ProjectStateId,
    "Stable identity for one project- or workspace-scoped Project State entry."
);
define_stable_id!(
    UserPreferenceId,
    "Stable identity for one global user Preference."
);
define_stable_id!(WorkItemId, "Stable identity for one Workspace WorkItem.");
define_stable_id!(
    IndexIncarnationId,
    "Stable identity for one physical index.db incarnation: kept across reopen and \
     migration, new whenever index.db is created or rebuilt."
);
define_stable_id!(
    WorkNoteId,
    "Stable identity for one WorkItem-owned observation, proposal, or open question."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_through_bytes() {
        let project_id = ProjectId::generate();
        let workspace_id = WorkspaceId::generate();
        let resource_id = ResourceId::generate();

        assert_eq!(ProjectId::from_bytes(project_id.to_bytes()), project_id);
        assert_eq!(
            WorkspaceId::from_bytes(workspace_id.to_bytes()),
            workspace_id
        );
        assert_eq!(ResourceId::from_bytes(resource_id.to_bytes()), resource_id);
    }

    #[test]
    fn ids_serialize_as_canonical_uuid_strings() {
        let id: ProjectId = "550E8400-E29B-41D4-A716-446655440000"
            .parse()
            .expect("known UUID should parse");

        assert_eq!(id.to_string(), "550e8400-e29b-41d4-a716-446655440000");

        let json = serde_json::to_string(&id).expect("ID should serialize");
        assert_eq!(json, "\"550e8400-e29b-41d4-a716-446655440000\"");

        let decoded: ProjectId = serde_json::from_str(&json).expect("ID should deserialize");
        assert_eq!(decoded, id);
    }

    #[test]
    fn invalid_text_is_rejected() {
        let error = "not-an-id"
            .parse::<WorkspaceId>()
            .expect_err("invalid identity must fail");

        assert_eq!(error, ParseStableIdError);
    }
}
