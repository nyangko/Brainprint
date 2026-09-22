#!/usr/bin/env bash
# Make the pinned rust-analyzer available for the task 13 spike and its
# real-backend acceptance.
#
# Nothing in production Brainprint runs this, searches PATH, or installs
# a toolchain: the engine is handed an explicit executable path. This is
# the developer-side equivalent of `restore.sh` for the C# backend.
set -euo pipefail

rustup component add rust-analyzer
rustup which rust-analyzer

# `rust-src` is optional on purpose. Task 13 measures the backend with
# it present and absent, and Brainprint never installs it:
#
#   rustup component add rust-src
