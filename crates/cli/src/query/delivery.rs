//! Budget profiles, retention and continuation CLI grammar (#24 §11-§13).

use std::num::NonZeroUsize;

use brainprint_core::protocol::query::*;
use clap::Args;

use super::{
    base64url,
    vocab::{BudgetProfileArg, RetentionArg},
};

/// #24 §11's exact locked profiles.
fn budget_profile(profile: BudgetProfileArg) -> (NonZeroUsize, NonZeroUsize) {
    const KIB: usize = 1024;
    match profile {
        BudgetProfileArg::Compact => (
            NonZeroUsize::new(16).expect("16 != 0"),
            NonZeroUsize::new(16 * KIB).expect("16 KiB != 0"),
        ),
        BudgetProfileArg::Standard => (
            NonZeroUsize::new(64).expect("64 != 0"),
            NonZeroUsize::new(64 * KIB).expect("64 KiB != 0"),
        ),
        BudgetProfileArg::Wide => (
            NonZeroUsize::new(128).expect("128 != 0"),
            NonZeroUsize::new(128 * KIB).expect("128 KiB != 0"),
        ),
    }
}

/// Common flags every planner-backed command requires (#24 §16, §22).
#[derive(Debug, Clone, Args)]
pub struct DeliveryArgs {
    /// Named delivery budget profile.
    #[arg(long, value_enum)]
    pub budget: BudgetProfileArg,
    /// Overrides the profile's `max_items` axis.
    #[arg(long)]
    pub budget_items: Option<NonZeroUsize>,
    /// Overrides the profile's `max_bytes` axis.
    #[arg(long)]
    pub budget_bytes: Option<NonZeroUsize>,
    #[arg(long, value_enum)]
    pub retention: RetentionArg,
    /// Continuation token from a previous `MORE_AVAILABLE` page.
    #[arg(long)]
    pub continuation: Option<String>,
}

#[derive(Debug)]
pub enum DeliveryArgsError {
    /// `--retention retained` without `--client-id`/`--session-id`.
    RetainedWithoutCorrelation,
    /// The `--continuation` token is not valid base64url or not a valid
    /// `DeliveryContinuationWire` JSON encoding of it.
    InvalidContinuation,
}

impl DeliveryArgs {
    pub fn into_wire(self, has_correlation: bool) -> Result<DeliveryWire, DeliveryArgsError> {
        let (profile_items, profile_bytes) = budget_profile(self.budget);
        // #24 §11: an override replaces that axis; the profile always
        // supplies both, so at least one axis remains set by construction.
        let max_items = Some(self.budget_items.unwrap_or(profile_items));
        let max_bytes = Some(self.budget_bytes.unwrap_or(profile_bytes));
        if matches!(self.retention, RetentionArg::Retained) && !has_correlation {
            return Err(DeliveryArgsError::RetainedWithoutCorrelation);
        }
        let continuation = self
            .continuation
            .map(|token| decode_continuation(&token))
            .transpose()?;
        Ok(DeliveryWire {
            budget: DeliveryBudgetWire {
                max_items,
                max_bytes,
            },
            continuation,
            retention: self.retention.into(),
        })
    }
}

/// Decode a compact `--continuation` token (base64url of the
/// `DeliveryContinuationWire` JSON) back into its structured form.
pub fn decode_continuation(token: &str) -> Result<DeliveryContinuationWire, DeliveryArgsError> {
    let bytes = base64url::decode(token).ok_or(DeliveryArgsError::InvalidContinuation)?;
    serde_json::from_slice(&bytes).map_err(|_| DeliveryArgsError::InvalidContinuation)
}

/// Encode a `DeliveryContinuationWire` as the compact `CONTINUATION`
/// token line prints (#24 §7, §21).
pub fn encode_continuation(continuation: &DeliveryContinuationWire) -> String {
    let bytes = serde_json::to_vec(continuation).expect("wire type always serializes");
    base64url::encode(&bytes)
}

/// #24 §12's exact locked text-search profiles.
fn search_budget_profile(profile: BudgetProfileArg) -> SearchBudgetWire {
    const MIB: u64 = 1024 * 1024;
    match profile {
        BudgetProfileArg::Compact => SearchBudgetWire {
            max_results: 50,
            max_files: 500,
            max_bytes: 8 * MIB,
            deadline_ms: Some(1_000),
        },
        BudgetProfileArg::Standard => SearchBudgetWire {
            max_results: 200,
            max_files: 5_000,
            max_bytes: 64 * MIB,
            deadline_ms: Some(3_000),
        },
        BudgetProfileArg::Wide => SearchBudgetWire {
            max_results: 500,
            max_files: 20_000,
            max_bytes: 256 * MIB,
            deadline_ms: Some(10_000),
        },
    }
}

fn search_budget_max_file_bytes(profile: BudgetProfileArg) -> u64 {
    const MIB: u64 = 1024 * 1024;
    match profile {
        BudgetProfileArg::Compact => MIB,
        BudgetProfileArg::Standard => 4 * MIB,
        BudgetProfileArg::Wide => 8 * MIB,
    }
}

#[derive(Debug, Clone, Args)]
pub struct SearchBudgetArgs {
    #[arg(long = "search-budget", value_enum)]
    pub profile: BudgetProfileArg,
    #[arg(long)]
    pub max_results: Option<usize>,
    #[arg(long)]
    pub max_files: Option<usize>,
    #[arg(long)]
    pub max_search_bytes: Option<u64>,
    #[arg(long)]
    pub deadline_ms: Option<u64>,
    #[arg(long)]
    pub max_file_bytes: Option<u64>,
}

impl SearchBudgetArgs {
    #[must_use]
    pub fn into_wire(self) -> (SearchBudgetWire, u64) {
        let mut budget = search_budget_profile(self.profile);
        if let Some(value) = self.max_results {
            budget.max_results = value;
        }
        if let Some(value) = self.max_files {
            budget.max_files = value;
        }
        if let Some(value) = self.max_search_bytes {
            budget.max_bytes = value;
        }
        if let Some(value) = self.deadline_ms {
            budget.deadline_ms = Some(value);
        }
        let max_file_bytes = self
            .max_file_bytes
            .unwrap_or_else(|| search_budget_max_file_bytes(self.profile));
        (budget, max_file_bytes)
    }
}
