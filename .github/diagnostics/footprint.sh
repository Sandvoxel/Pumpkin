#!/usr/bin/env bash
set -euo pipefail
{
  source=$(cat diagnostic-output/source-commit.txt)
  git diff --quiet "$source" -- . ':(exclude).github'
  sha256sum Cargo.lock Cargo.toml crates/pumpkin-plugin-runtime/src/executor.rs
  df -B1 .
  du -sb target 2>/dev/null || true
  find target -type f \( -name 'libpumpkin_wasm_host*.rlib' -o -name 'libpumpkin_wasm_host*.rmeta' -o -name pumpkin \) -printf '%s %p\n' 2>/dev/null | sort -nr || true
  binary=target/aarch64-unknown-linux-musl/release/pumpkin
  if [ -f "$binary" ]; then
    sha256sum "$binary"
    size -A "$binary"
    readelf -SW "$binary"
  fi
} | tee diagnostic-output/final-footprint.txt
