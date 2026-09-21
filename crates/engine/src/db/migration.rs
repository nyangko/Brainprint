//! Versioned migration definitions and their integrity checksum.
//!
//! Migration SQL bodies live in compiled Brainprint source (this crate),
//! never in a runtime `.brainprint` directory — see #13 task 3/13. Domain
//! schema for each DB kind is populated by later tasks (#15 task 5); this
//! module only defines the shape a migration takes and how its checksum is
//! computed.

/// One versioned, checksummed schema change for a single DB kind.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

impl Migration {
    /// Deterministic integrity checksum of this migration's SQL body.
    ///
    /// This is a change-detection checksum, not a security control, so a
    /// dependency-free FNV-1a hash is sufficient.
    #[must_use]
    pub fn checksum(&self) -> String {
        checksum_hex(self.sql)
    }
}

fn checksum_hex(text: &str) -> String {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_is_stable_for_identical_sql() {
        let a = Migration {
            version: 1,
            name: "create_widgets",
            sql: "CREATE TABLE widgets (id INTEGER PRIMARY KEY);",
        };
        let b = Migration {
            version: 1,
            name: "create_widgets",
            sql: "CREATE TABLE widgets (id INTEGER PRIMARY KEY);",
        };

        assert_eq!(a.checksum(), b.checksum());
    }

    #[test]
    fn checksum_changes_with_sql_body() {
        let original = Migration {
            version: 1,
            name: "create_widgets",
            sql: "CREATE TABLE widgets (id INTEGER PRIMARY KEY);",
        };
        let edited = Migration {
            version: 1,
            name: "create_widgets",
            sql: "CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT);",
        };

        assert_ne!(original.checksum(), edited.checksum());
    }
}
