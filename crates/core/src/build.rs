//! Build and protocol identity shared by Brainprint components.

/// Human-readable product name.
pub const PRODUCT_NAME: &str = "Brainprint";

/// Cargo package version for the shared core contract.
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version of the local Brainprint client/daemon protocol.
///
/// This is intentionally independent from the package version.
pub const PROTOCOL_VERSION: u32 = 1;

/// Minimal build identity that benchmark and runtime boundaries can record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildInfo {
    pub product: &'static str,
    pub version: &'static str,
    pub protocol_version: u32,
}

impl BuildInfo {
    /// Build identity for the currently compiled Brainprint version.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            product: PRODUCT_NAME,
            version: PACKAGE_VERSION,
            protocol_version: PROTOCOL_VERSION,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_build_info_is_populated() {
        let info = BuildInfo::current();

        assert_eq!(info.product, "Brainprint");
        assert!(!info.version.is_empty());
        assert!(info.protocol_version > 0);
    }
}
