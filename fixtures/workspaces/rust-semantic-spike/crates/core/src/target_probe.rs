//! One target function, called from every shape #44's measurement gate
//! needs to tell CALLS from a plain reference: same file, a different
//! file in the same crate, a sibling workspace crate, and inside a
//! macro's argument tree -- plus one site that names the function
//! without calling it, so a naive "any reference" reading would be
//! wrong.
//!
//! #45 adds two more shapes the reciprocal outgoingCalls confirmation
//! must handle: a caller with both a real call and a bare reference to
//! the same target in one function body, and a call reached through a
//! renamed import.

use crate::target_probe::target_probe as aliased_probe;

/// The single target Symbol every probe site below points at.
pub fn target_probe() -> u32 {
    7
}

/// Same file as the declaration.
pub fn same_file_caller() -> u32 {
    target_probe()
}

/// #45: one real call plus a separate bare reference to the same
/// target, inside the same caller. Reciprocal outgoingCalls must keep
/// only the call's fromRange and drop the reference's.
pub fn mixed_call_and_reference_caller() -> u32 {
    let value = target_probe();
    let _not_a_call: fn() -> u32 = target_probe;
    value
}

/// #45: a call through a renamed import. Target matching must resolve
/// this by semantic identity, not by the spelling at the call site.
pub fn aliased_import_caller() -> u32 {
    aliased_probe()
}

#[cfg(test)]
mod tests {
    use super::target_probe;

    #[test]
    fn macro_nested_call_matches_the_target() {
        // A call written inside `assert_eq!`'s argument tree: opaque
        // to the structural extractor, the same shape as #43's
        // `assert!(matches!(...))` gap.
        assert_eq!(target_probe(), 7);
    }
}
