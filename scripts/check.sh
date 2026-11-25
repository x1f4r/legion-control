#!/usr/bin/env bash
# Run the checks for one component, or the complete suite.
#
# CI runs each component on its own, so what is checked here matches what CI holds a change to.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$ROOT"
component="${1:-all}"
case "$component" in all|scripts|rust) ;; *) echo "Usage: $0 [all|scripts|rust]" >&2; exit 2 ;; esac

if [[ "$component" == all || "$component" == scripts ]]; then
  # Parse every JavaScript file in the checkout.
  node scripts/check-syntax.mjs
  # Refuse invisible, zero-width, bidirectional, or stray byte order mark characters.
  node scripts/check-invisible.mjs
  # Keep every media-type mirror synchronized with crates/legion-core/src/media.rs.
  node scripts/sync-media-table.mjs --check
  node --test tests/*.test.mjs
  node --test apps/ui/lib/test/*.test.mjs
  # Last, so a private value found never hides what the tests above would have said. CI runs it
  # once more on its own, with the private terms from the repository's secret.
  node scripts/check-private-data.mjs
fi
if [[ "$component" == all || "$component" == rust ]]; then
  # --locked holds the build to the committed Cargo.lock, so a check never resolves a dependency anew.
  cargo fmt --all --check
  cargo clippy --workspace --all-targets --locked -- -D warnings
  # --no-fail-fast: one failing crate must not hide what every later crate would have said on this system.
  cargo test --workspace --locked --no-fail-fast
  node scripts/legion-tools-doc.mjs --bin target/debug/legion --check
fi
