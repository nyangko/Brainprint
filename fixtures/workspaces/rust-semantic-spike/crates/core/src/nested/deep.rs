//! Reached through `super` and `self`, each grouped and aliased (#42).

use super::{Nested, Nested as AliasedNested};
use self::inner::{Inner, Inner as AliasedInner};

mod inner {
    pub struct Inner;

    impl Inner {
        pub fn value(&self) -> u32 {
            3
        }
    }
}

pub fn combine() -> u32 {
    Nested.depth() + Inner.value() + AliasedNested.depth() + AliasedInner.value()
}
