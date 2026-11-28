#!/usr/bin/env bash
# Short, reproducible fuzz lane for the engine fuzz workspace.
#
# Runs one or more cargo-fuzz targets from `fuzz/` against the checked-in seed
# corpus for a bounded wall-clock budget. This is a smoke lane, not a
# replacement for long fuzzing: it catches a target that crashes on its seeds
# or immediately after, and keeps the corpus and harness compiling in CI.
#
# Checked-in seeds live in fuzz/seeds/<target>/; inputs discovered by a run are
# written to the gitignored fuzz/corpus/<target>/ so a run never dirties the
# tree.
#
# Usage:
#   scripts/fuzz-smoke.sh                    # default budget per target
#   FUZZ_SECONDS=60 scripts/fuzz-smoke.sh    # longer local run
#   FUZZ_TARGETS="module_load" scripts/fuzz-smoke.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$ROOT/fuzz"

FUZZ_SECONDS="${FUZZ_SECONDS:-15}"
FUZZ_TARGETS="${FUZZ_TARGETS:-module_load load_and_instantiate interpreter_dispatch aot_artifact_load aot_dispatch shared_region}"
MAX_LEN="${FUZZ_MAX_LEN:-262144}"
NIGHTLY="${WASMTINY_NIGHTLY_TOOLCHAIN:-nightly}"

if ! command -v cargo-fuzz >/dev/null 2>&1; then
  echo "fuzz-smoke: install cargo-fuzz (cargo install cargo-fuzz --locked)" >&2
  exit 2
fi
if ! rustc +"$NIGHTLY" --version >/dev/null 2>&1; then
  echo "fuzz-smoke: install the nightly with rustup toolchain install $NIGHTLY" >&2
  exit 2
fi

for target in $FUZZ_TARGETS; do
  echo "fuzz-smoke: $target (${FUZZ_SECONDS}s, max_len=${MAX_LEN})"
  mkdir -p "corpus/$target"
  # The first corpus directory is writable and gitignored; the checked-in
  # seeds are passed as read-only extra corpora.
  corpora=("corpus/$target")
  if [ -d "seeds/$target" ]; then
    corpora+=("seeds/$target")
  fi
  cargo +"$NIGHTLY" fuzz run "$target" "${corpora[@]}" -- \
    -max_total_time="$FUZZ_SECONDS" \
    -max_len="$MAX_LEN" \
    -rss_limit_mb=2048 \
    -timeout=10
done

echo "fuzz-smoke: all targets passed"
