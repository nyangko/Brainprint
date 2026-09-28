//! One target function, called from every shape #44's measurement gate
//! needs to tell CALLS from a plain reference: same file, a different
//! file in the same crate, a sibling workspace crate, and inside a
//! macro's argument tree -- plus one site that names the function
//! without calling it, so a naive "any reference" reading would be
//! wrong.

/// The single target Symbol every probe site below points at.
pub fn target_probe() -> u32 {
    7
}

/// Same file as the declaration.
pub fn same_file_caller() -> u32 {
    target_probe()
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
