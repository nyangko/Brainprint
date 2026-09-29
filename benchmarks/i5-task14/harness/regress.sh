#!/bin/bash
. @T14_ROOT@/env.sh
cd @T14_ROOT@/clean/repo
E="env -i HOME=@T14_ROOT@/clean/home PATH=$CLEAN_PATH RUSTUP_HOME=$RUSTUP_HOME CARGO_HOME=$CARGO_HOME"
$E cargo fmt --all --check; echo "fmt exit=$?"
$E cargo clippy --workspace --all-targets --locked -- -D warnings; echo "clippy exit=$?"
$E cargo build --workspace --locked; echo "build exit=$?"
$E cargo test --workspace --locked --no-fail-fast; echo "test exit=$?"
echo REGRESS_DONE
