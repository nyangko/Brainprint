//! A build script that leaves evidence if it runs.
//!
//! Nothing in the crate needs it. It exists so acceptance can prove
//! that Brainprint's P0 rust-analyzer configuration does not execute
//! build scripts: if this ever runs, `BRAINPRINT_BUILD_RS_MARKER`
//! names a file that appears, and the test fails.
use std::{env, fs};

fn main() {
    if let Ok(marker) = env::var("BRAINPRINT_BUILD_RS_MARKER") {
        let _ = fs::write(&marker, "build.rs executed\n");
    }
    // Also write into OUT_DIR, so generated-source handling has
    // something real to stay away from.
    if let Ok(out_dir) = env::var("OUT_DIR") {
        let _ = fs::write(
            std::path::Path::new(&out_dir).join("generated.rs"),
            "pub fn generated() -> u32 { 42 }\n",
        );
    }
    println!("cargo:rerun-if-changed=build.rs");
}
