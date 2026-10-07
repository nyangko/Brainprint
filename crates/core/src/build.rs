//! Build and protocol identity shared by Brainprint components.

/// Human-readable product name.
pub const PRODUCT_NAME: &str = "Brainprint";

/// Cargo package version for the shared core contract.
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version of the local Brainprint client/daemon protocol.
///
/// This is intentionally independent from the package version. Bumped to
/// 2 by #24 Task 11: the `Request`/`Response` enums gained `Query`/
/// `QueryAck` variants, which is wire-incompatible with a v1 peer.
/// Bumped to 3 by #41: `DeliveryPageWire` replaced its parallel
/// `evidence`/`references` arrays with one `evidence: Vec<DeliveredItemWire>`
/// (`Full` or `Reuse`, never both for one slot), which a v2 peer cannot
/// decode. Compatibility stays strict and symmetric -- an older client and
/// a newer daemon (or the reverse) reject each other explicitly rather
/// than negotiating or guessing. Bumped to 4 by #50: `Request`/`Response`
/// gained the `Work` variant, which a v3 peer cannot decode. Bumped to 5
/// by #51: `GitObservationWire::Observe` and
/// `WorkErrorWire::GitObservation` are variants a v4 peer cannot decode.
/// Bumped to 6 by #52: the Work Result `verification` input, the
/// per-command results on `Recorded`/`Failed` and
/// `WorkErrorWire::VerificationBusy`. Bumped to 7 by #53: the per-command
/// `capture` request and result (compact diagnostics, raw artifact
/// references) and the `ArtifactRead` request/response. Bumped to 8 by
/// #54: the `VerificationJobStart`/`Poll`/`Cancel` requests and responses.
/// Bumped to 9 by #55: the post-command refresh fact on managed Job
/// terminal events and on a synchronous verification's Work response,
/// `JobEndReasonWire::BaselineCurrentness`, the Work Result input's
/// `verification_job` (a managed Job attached without running anything)
/// and the stored result's `verification_job` provenance, and the typed
/// verification freshness/attach `WorkErrorWire` variants. Bumped to 10
/// by #56: the `Doctor`/`Rebuild` requests and responses. Bumped to 11
/// by #58: the `EvidenceWire::Outline` variant. Bumped to 12 by #57: the
/// `Sync`/`Uninit` requests and responses and
/// `NotInitializedReasonWire::WorkspaceDetached`. Bumped to 13 by #70: a
/// path-scoped `StatusRequest` and the Workspace status it answers with.
/// Bumped to 14 by #73: `EvidenceWire::Relation` carries
/// `ProjectedRelationWire` (each evidence site's path, 1-based line and
/// containing Symbol name). Bumped to 15 by #89: the `Shutdown` request
/// and response, which a daemon also honours after a protocol mismatch so
/// a newer client can stop it.
pub const PROTOCOL_VERSION: u32 = 15;

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
