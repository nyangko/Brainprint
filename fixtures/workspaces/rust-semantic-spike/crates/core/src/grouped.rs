//! Grouped `use` acceptance fixtures (#42): every leaf a flat `use`
//! would name is still confirmable once written with braces, nested two
//! levels deep, or reached through a glob.

use crate::{
    model::{identity, Boxed},
    runner::Worker,
};
use crate::nested::deep::*;

pub fn use_grouped() -> u32 {
    let boxed = Boxed::new(identity(4_u32));
    let worker = Worker::new(2);
    worker.execute() + boxed.value + combine()
}
