#!/usr/bin/env bash
# Failure-path verification for the sandbox-escape test build.
#
# Verifies that CI failures are distinct and attributable:
#   (a) an intentionally-broken fixture (verdict mismatch) fails the
#       corpus run, naming the fixture;
#   (b) a hung fixture (budget-ignoring guest) fails the run as HUNG,
#       not as a flake or a hung runner;
#   (c) an off-list `unsafe` fails the allowlist gate, naming the file.
#
# Each case must FAIL in the expected way; this script exits 0 only
# when all three failures are observed. Run after a green
# `cargo test --features security-test` (needs the built test deps).
set -uo pipefail

cd "$(dirname "$0")/.."

status=0

# --- (a) broken fixture: expected clean, actually traps ---------------
TMP_A=$(mktemp -d)
mkdir -p "$TMP_A/broken-fixture"
cat > "$TMP_A/broken-fixture/manifest.txt" <<'EOF'
name: broken-fixture
threat: TM-01
expected: clean
wat: fixture.wat
entry: main
EOF
cat > "$TMP_A/broken-fixture/fixture.wat" <<'EOF'
(module
    (memory 1 1)
    (func (export "main")
        i32.const 65536
        i32.load
        drop))
EOF

if WASMTINY_CORPUS_DIR="$TMP_A" \
    cargo test --features security-test --test corpus corpus_fixtures_fail_closed \
    > "$TMP_A/out.log" 2>&1; then
    echo "FAIL (a): broken fixture did NOT fail the corpus run"
    status=1
else
    if grep -q "verdict mismatch: expected clean, got trapped" "$TMP_A/out.log" && \
       grep -q "\[broken-fixture\]" "$TMP_A/out.log"; then
        echo "OK (a): broken fixture fails, naming the fixture and verdicts"
    else
        echo "FAIL (a): corpus failed but not in the expected attributable way:"
        grep -E "corpus containment|verdict mismatch|broken-fixture" "$TMP_A/out.log" | head -5
        status=1
    fi
fi

# --- (b) hung fixture: budget-ignoring guest fails as HUNG ------------
TMP_B=$(mktemp -d)
mkdir -p "$TMP_B/hung-fixture"
cat > "$TMP_B/hung-fixture/manifest.txt" <<'EOF'
name: hung-fixture
threat: TM-05
expected: clean
wat: fixture.wat
entry: main
budget_ms: 200
selftest: hang
EOF
cat > "$TMP_B/hung-fixture/fixture.wat" <<'EOF'
(module
    (func (export "main")
        nop))
EOF

if WASMTINY_CORPUS_DIR="$TMP_B" \
    cargo test --features security-test --test corpus corpus_fixtures_fail_closed \
    > "$TMP_B/out.log" 2>&1; then
    echo "FAIL (b): hung fixture did NOT fail the corpus run"
    status=1
else
    if grep -q "HUNG: exceeded the harness deadline" "$TMP_B/out.log"; then
        echo "OK (b): hung fixture fails as HUNG (resource-management defect, not a flake)"
    else
        echo "FAIL (b): corpus failed but not with the HUNG verdict:"
        grep -E "HUNG|hung-fixture" "$TMP_B/out.log" | head -5
        status=1
    fi
fi

# --- (c) off-list unsafe: allowlist gate fails, naming the file -------
TMP_C=$(mktemp -d)
mkdir -p "$TMP_C/src"
cp crates/core/src/*.rs "$TMP_C/src/"
printf '\npub fn evil() { unsafe { std::hint::black_box(1u8); } }\n' >> "$TMP_C/src/lib.rs"

if ./scripts/check_unsafe.sh "$TMP_C/src" > "$TMP_C/out.log" 2>&1; then
    echo "FAIL (c): off-list unsafe did NOT fail the allowlist gate"
    status=1
else
    if grep -q "src/lib.rs has 1 unsafe item" "$TMP_C/out.log"; then
        echo "OK (c): off-list unsafe fails the gate, naming the file"
    else
        echo "FAIL (c): gate failed but not in the expected attributable way:"
        head -5 "$TMP_C/out.log"
        status=1
    fi
fi

rm -rf "$TMP_A" "$TMP_B" "$TMP_C"

if [[ "$status" -ne 0 ]]; then
    echo "failure-path verification: FAILED"
    exit "$status"
fi

echo "failure-path verification: all three failure paths are distinct and attributable"
