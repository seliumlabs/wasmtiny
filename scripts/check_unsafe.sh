#!/usr/bin/env bash
# Static gate: wasmtiny's own unsafe code must appear in the audited
# allowlist (tools/unsafe_allowlist.txt) with matching counts.
#
# Counts unsafe-code items (unsafe blocks, unsafe fn, unsafe impl)
# per file under crates/core/src/ and compares against the allowlist. Any
# off-list occurrence, count drift, or missing entry fails.
#
# Guests (tests/corpus) are exempt by design — no static gate applies
# to hostile payloads (docs/threat-model.md).
#
# Usage: scripts/check_unsafe.sh [src-root]
#   The optional src-root (default: crates/core/src/) lets the failure-path
#   self-test run this gate against a scratch copy with an injected
#   violation.
set -euo pipefail

cd "$(dirname "$0")/.."

SRC_ROOT="${1:-crates/core/src}"
ALLOWLIST="tools/unsafe_allowlist.txt"

if [[ ! -d "$SRC_ROOT" ]]; then
    echo "FAIL: source root '$SRC_ROOT' does not exist"
    exit 1
fi

# Count unsafe items per file: 'unsafe {' blocks, 'unsafe fn',
# 'unsafe impl' — word-bounded so comments mentioning unsafe do not
# match unless they precede an actual item.
count_unsafe() {
    local file="$1"
    grep -cE 'unsafe[[:space:]]*(\{|fn|impl)' "$file" || true
}

status=0
while IFS= read -r file; do
    # Repo-relative path so allowlist entries match regardless of the
    # src-root argument (scratch copies map back onto crates/core/src/...).
    if [[ "$SRC_ROOT" == "crates/core/src" ]]; then
        rel="$file"
    else
        rel="crates/core/src/${file#"$SRC_ROOT"/}"
    fi
    actual="$(count_unsafe "$file")"
    allowed="$(grep -E "^${rel//\\/\\\\}[[:space:]]" "$ALLOWLIST" | awk '{print $2}' || true)"
    if [[ -z "$allowed" ]]; then
        echo "FAIL: $rel has $actual unsafe item(s) but is NOT in the allowlist"
        status=1
    elif [[ "$actual" != "$allowed" ]]; then
        echo "FAIL: $rel has $actual unsafe item(s), allowlist says $allowed — update tools/unsafe_allowlist.txt (with audit)"
        status=1
    fi
done < <(grep -rlE 'unsafe[[:space:]]*(\{|fn|impl)' "$SRC_ROOT" --include='*.rs' | sort)

# Also flag allowlist entries whose files no longer exist or no longer
# contain unsafe code (stale entries hide drift).
while IFS= read -r line; do
    [[ "$line" =~ ^#.*$ || -z "$line" ]] && continue
    rel="$(echo "$line" | awk '{print $1}')"
    allowed="$(echo "$line" | awk '{print $2}')"
    # Allowlist entries are repo-relative (crates/core/src/...); map back onto
    # the (possibly scratch) source root.
    file="$SRC_ROOT/${rel#crates/core/src/}"
    if [[ ! -f "$file" ]]; then
        echo "FAIL: allowlist entry $rel points at a missing file"
        status=1
    else
        actual="$(count_unsafe "$file")"
        if [[ "$actual" != "$allowed" ]]; then
            echo "FAIL: allowlist entry $rel expects $allowed, found $actual"
            status=1
        fi
    fi
done < "$ALLOWLIST"

if [[ "$status" -ne 0 ]]; then
    echo "unsafe allowlist gate: FAILED"
    exit "$status"
fi

echo "OK: all unsafe code in $SRC_ROOT is on the audited allowlist"
