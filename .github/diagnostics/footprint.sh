#!/usr/bin/env bash
set -euo pipefail
{
  git diff --exit-code HEAD -- Cargo.lock Cargo.toml
  sha256sum Cargo.lock Cargo.toml crates/pumpkin-data/src/generated/advancement.rs tools/pumpkin-codegen/src/advancement.rs
  df -B1 .
  du -sb target 2>/dev/null || true
  find target -type f \( -name 'libpumpkin_wasm_host*.rlib' -o -name pumpkin \) -printf '%s %p\n' 2>/dev/null | sort -nr || true
  binary=target/aarch64-unknown-linux-musl/release/pumpkin
  if [ -f "$binary" ]; then
    sha256sum "$binary"
    size -A "$binary"
    readelf -SW "$binary"
  fi
} | tee diagnostic-output/final-footprint.txt
