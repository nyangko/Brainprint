//! Reached through `super` and `self`.

use super::Nested;
use self::inner::Inner;

mod inner {
    pub struct Inner;

    impl Inner {
        pub fn value(&self) -> u32 {
            3
        }
    }
}

pub fn combine() -> u32 {
    Nested.depth() + Inner.value()
}
