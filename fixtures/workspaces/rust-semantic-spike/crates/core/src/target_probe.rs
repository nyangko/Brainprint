//! One target function, called from every shape #44's measurement gate
//! needs to tell CALLS from a plain reference: same file, a different
//! file in the same crate, a sibling workspace crate, and inside a
//! macro's argument tree -- plus one site that names the function
//! without calling it, so a naive "any reference" reading would be
//! wrong.
//!
//! #45 adds a caller with both a real call and a bare reference to the
//! same target in one function body (the reciprocal outgoingCalls
//! confirmation must keep only the call's range). The aliased-import
//! shape moved to `runner.rs` for #46: #45's version was
//! self-referential (imported from within its own declaring module)
//! and is not evidence of general alias behavior.
//!
//! #46 adds the signatureHelp-confirmation negative shapes: a
//! `stringify!` macro whose tokens look like a call but never execute
//! it, a token-swallowing `macro_rules!` arm that discards its input
//! entirely, a nested-macro positive shape (`assert!(matches!(...))`,
//! matching Brainprint's own `load_workspace_config` gap), and an
//! unrelated call sitting next to a bare reference so a bounded probe
//! position cannot accidentally borrow the neighbor's signatureHelp.

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

/// #46 negative: the tokens `target_probe()` appear in source, but
/// `stringify!` never executes them -- it turns them into a string
/// literal. signatureHelp at the probe position must not report a
/// callable argument list here.
pub fn stringify_reference_only() -> &'static str {
    stringify!(target_probe())
}

macro_rules! swallow_tokens {
    ($($_tt:tt)*) => {
        0
    };
}

/// #46 negative: a `macro_rules!` arm that accepts arbitrary tokens and
/// discards them, never expanding them into the target call it
/// textually contains.
pub fn token_swallowing_caller() -> u32 {
    swallow_tokens!(target_probe())
}

/// #46 negative: an unrelated real call (`same_file_caller`) sitting
/// immediately next to a bare reference to the target. A bounded probe
/// position derived from the reference's own range must not borrow
/// signatureHelp from the neighboring call.
pub fn unrelated_call_beside_reference() -> u32 {
    let _other = same_file_caller();
    let _not_a_call: fn() -> u32 = target_probe;
    _other
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

    #[test]
    fn nested_macro_call_matches_the_target() {
        // #46: a call nested two macros deep, matching the shape of
        // Brainprint's own `assert!(matches!(load_workspace_config(...), ...))`.
        assert!(matches!(target_probe(), 7));
    }
}
