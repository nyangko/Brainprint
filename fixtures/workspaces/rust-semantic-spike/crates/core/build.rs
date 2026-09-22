//! A build script that leaves evidence if it runs.
//!
//! Nothing in the crate needs it. It exists so acceptance can prove
//! that Brainprint's P0 rust-analyzer configuration does not execute
//! build scripts: if this ever runs, `build-rs-ran.marker` appears
//! beside it, and the test fails.
//!
//! The marker is written next to the manifest rather than to a path
//! from the environment, so the check needs no variable to be set and
//! works in whatever directory the fixture was copied to.
use std::{env, fs, path::Path};

fn main() {
    let here = env!("CARGO_MANIFEST_DIR");
    let _ = fs::write(
        Path::new(here).join("build-rs-ran.marker"),
        "build.rs executed\n",
    );
    // Also write into OUT_DIR, so generated-source handling has
    // something real to stay away from.
    if let Ok(out_dir) = env::var("OUT_DIR") {
        let _ = fs::write(
            Path::new(&out_dir).join("generated.rs"),
            "pub fn generated() -> u32 { 42 }\n",
        );
    }
    println!("cargo:rerun-if-changed=build.rs");
}
