#!/usr/bin/env bash
# Regenerate the audited unsafe-code allowlist (tools/unsafe_allowlist.txt)
# from the current tree, so count drift and newly off-list files stop
# failing CI mechanically.
#
# The static gate (scripts/check_unsafe.sh) counts unsafe-code items
# (unsafe blocks, unsafe fn, unsafe impl) per file under crates/core/src/
# and requires an exact match against tools/unsafe_allowlist.txt. When the
# code changes, the counts drift and the gate fails with
# "update tools/unsafe_allowlist.txt (with audit)". This script performs
# the mechanical half of that update:
#
#   * counts are re-synced for every file that already has an entry,
#     preserving the audited reason verbatim;
#   * entries whose file disappeared (or no longer contains unsafe code)
#     are dropped;
#   * newly off-list files are appended with an UNAUDITED placeholder
#     reason that a human must replace after auditing the sites.
#
# The audit itself is a human step. This script never invents a reason:
# new entries carry "UNAUDITED:" and every changed/new entry is listed in
# the run summary. Pass --audit to make the script exit non-zero while any
# UNAUDITED marker remains (use it in CI once the list is fully audited).
#
# The counting here MUST match scripts/check_unsafe.sh exactly, or the two
# tools disagree. Both use:
#   grep -cE 'unsafe[[:space:]]*(\{|fn|impl)' "$file"
# over the files reported by
#   grep -rlE 'unsafe[[:space:]]*(\{|fn|impl)' <root> --include='*.rs'
#
# Guests (tests/corpus) are exempt by design, exactly as in the gate.
#
# Usage: scripts/update_unsafe_allowlist.sh [options] [src-root]
#   src-root       default: crates/core/src. A scratch root maps back onto
#                  crates/core/src/... like check_unsafe.sh, so the
#                  failure-path self-test can exercise this script too.
#   -n, --dry-run  show the changes (unified diff) but do not write
#   -c, --check    do not write; exit 1 if the allowlist is out of date
#   -a, --audit    exit 1 if any UNAUDITED placeholder remains (after an
#                  update, or in --check mode)
#   -h, --help     show this help
#
# Exit status: 0 on success; 1 if --check found drift, or --audit found a
# placeholder; 2 on usage errors.
set -euo pipefail

cd "$(dirname "$0")/.."

ALLOWLIST="tools/unsafe_allowlist.txt"
SRC_ROOT="crates/core/src"
DRY_RUN=0
CHECK_ONLY=0
AUDIT=0

usage() {
    cat <<'EOF'
Regenerate tools/unsafe_allowlist.txt from the current source tree.

Usage: scripts/update_unsafe_allowlist.sh [options] [src-root]

  src-root       default: crates/core/src (scratch roots map onto
                 crates/core/src/... so the failure-path self-test works;
                 a non-default root is read-only, i.e. --check/--dry-run)
  -n, --dry-run  show the unified diff but do not write the allowlist
  -c, --check    do not write; exit 1 if the allowlist is out of date
  -a, --audit    exit 1 if any UNAUDITED placeholder remains
  -h, --help     show this help

Existing reasons are preserved verbatim; only counts are re-synced. New
files are appended with an UNAUDITED placeholder that a human must replace
after auditing the sites. The audit itself is never automated.
EOF
}

die() { echo "update_unsafe_allowlist: $*" >&2; exit 2; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        -n|--dry-run) DRY_RUN=1 ;;
        -c|--check)   CHECK_ONLY=1 ;;
        -a|--audit)   AUDIT=1 ;;
        -h|--help)    usage; exit 0 ;;
        -*)           die "unknown option '$1' (try --help)" ;;
        *)            SRC_ROOT="$1" ;;
    esac
    shift
done

[[ -d "$SRC_ROOT" ]] || die "source root '$SRC_ROOT' does not exist"
[[ -f "$ALLOWLIST" ]] || die "allowlist '$ALLOWLIST' does not exist"

# A scratch root maps onto crates/core/src/... paths; never let it rewrite
# the real allowlist. Non-default roots are read-only (--check/--dry-run).
if [[ "$SRC_ROOT" != "crates/core/src" && "$CHECK_ONLY" -eq 0 && "$DRY_RUN" -eq 0 ]]; then
    die "refusing to write $ALLOWLIST from non-default src-root '$SRC_ROOT'; use --dry-run or --check"
fi

# Same count as scripts/check_unsafe.sh (kept in sync deliberately).
count_unsafe() {
    local file="$1"
    grep -cE 'unsafe[[:space:]]*(\{|fn|impl)' "$file" || true
}

PLACEHOLDER='UNAUDITED: new unsafe item(s) — audit each site, add a SAFETY rationale, then delete this marker'

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

current="$tmp/current"   # "<repo-relative-path> <count>" per off-list file
out="$tmp/out"           # regenerated allowlist
handled="$tmp/handled"   # repo-relative paths already emitted
: > "$current"; : > "$out"; : > "$handled"

# --- snapshot the tree: repo-relative path + unsafe count ---------------
while IFS= read -r file; do
    if [[ "$SRC_ROOT" == "crates/core/src" ]]; then
        rel="$file"
    else
        rel="crates/core/src/${file#"$SRC_ROOT"/}"
    fi
    printf '%s %s\n' "$rel" "$(count_unsafe "$file")" >> "$current"
done < <(grep -rlE 'unsafe[[:space:]]*(\{|fn|impl)' "$SRC_ROOT" --include='*.rs' | sort)

# --- rebuild the allowlist, preserving order and reasons ---------------
changed=()   # "path: old -> new"
removed=()   # "path"
untouched=0

while IFS= read -r line || [[ -n "$line" ]]; do
    # Comments and blank lines are carried across verbatim.
    if [[ "$line" =~ ^[[:space:]]*# ]] || [[ -z "${line//[[:space:]]/}" ]]; then
        printf '%s\n' "$line" >> "$out"
        continue
    fi

    path="$(printf '%s' "$line" | awk '{print $1}')"
    old_count="$(printf '%s' "$line" | awk '{print $2}')"
    # Everything after "<path> <count>" is the audited reason; keep it.
    reason="$(printf '%s' "$line" | sed -E 's/^[[:space:]]*[^[:space:]]+[[:space:]]+[^[:space:]]+[[:space:]]*//')"
    [[ "$reason" == "$line" ]] && reason=""

    actual="$(awk -v p="$path" '$1 == p { print $2 }' "$current")"
    if [[ -z "$actual" ]]; then
        removed+=("$path")
        continue
    fi

    if [[ -z "$reason" ]]; then
        reason="$PLACEHOLDER"
    fi

    printf '%s %s %s\n' "$path" "$actual" "$reason" >> "$out"
    printf '%s\n' "$path" >> "$handled"

    if [[ "$actual" != "$old_count" ]]; then
        changed+=("$path: $old_count -> $actual")
    else
        untouched=$((untouched + 1))
    fi
done < "$ALLOWLIST"

# --- append files that are off-list (they need a human audit) ----------
new=()
while IFS= read -r rec; do
    p="${rec%% *}"
    c="${rec##* }"
    if ! grep -Fxq "$p" "$handled"; then
        printf '%s %s %s\n' "$p" "$c" "$PLACEHOLDER" >> "$out"
        new+=("$p: $c")
    fi
done < "$current"

# --- summarise ----------------------------------------------------------
echo "unsafe allowlist: source root=$SRC_ROOT entries=$untouched unchanged, ${#changed[@]} re-synced, ${#new[@]} new, ${#removed[@]} removed"

if [[ "${#changed[@]}" -gt 0 ]]; then
    echo "re-synced (count drift — re-audit the new sites):"
    printf '  %s\n' "${changed[@]}"
fi
if [[ "${#new[@]}" -gt 0 ]]; then
    echo "new (UNAUDITED — audit and replace the placeholder reason):"
    printf '  %s\n' "${new[@]}"
fi
if [[ "${#removed[@]}" -gt 0 ]]; then
    echo "removed (no unsafe code or file gone):"
    printf '  %s\n' "${removed[@]}"
fi

have_markers=0
if grep -q 'UNAUDITED:' "$out"; then
    have_markers=1
fi

# --- modes --------------------------------------------------------------
if cmp -s "$out" "$ALLOWLIST"; then
    if [[ "$AUDIT" -eq 1 && "$have_markers" -eq 1 ]]; then
        echo "FAIL: $ALLOWLIST is count-consistent but still contains UNAUDITED placeholders"
        grep -n 'UNAUDITED:' "$ALLOWLIST" | sed 's/^/  /'
        exit 1
    fi
    echo "OK: $ALLOWLIST is up to date"
    exit 0
fi

if [[ "$CHECK_ONLY" -eq 1 ]]; then
    echo "FAIL: $ALLOWLIST is out of date; run scripts/update_unsafe_allowlist.sh"
    diff -u "$ALLOWLIST" "$out" || true
    exit 1
fi

if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "dry-run (not written):"
    diff -u "$ALLOWLIST" "$out" || true
    exit 0
fi

cp "$out" "$ALLOWLIST"
echo "wrote $ALLOWLIST"

if [[ "$AUDIT" -eq 1 && "$have_markers" -eq 1 ]]; then
    echo "FAIL: $ALLOWLIST updated but still contains UNAUDITED placeholders:"
    grep -n 'UNAUDITED:' "$ALLOWLIST" | sed 's/^/  /'
    exit 1
fi
