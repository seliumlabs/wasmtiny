# wasmtiny Threat Model

This document is the source of truth for sandbox-containment test
coverage. Every entry (`TM-xx`) must map to at least one corpus fixture
or fuzz target; `tests/traceability.rs` fails CI when it does not.

## Scope and assumptions

- **Guest payloads are untrusted binaries, not untrusted source.** A
  malicious author compiles guests however best sabotages the runtime:
  hand-crafted bytecode, absent bounds checks, binary-patched modules.
  No static gate applies to guest code; all guarantees are established
  by observing runtime behaviour from outside the sandbox.
- Static gates (unsafe-code allowlist, dependency advisories, lints)
  apply to wasmtiny's own code only.
- The interpreter runs unvalidated-at-author-time code; the loader and
  validator are the first trust boundary, the interpreter and host
  functions the second, the process boundary the third.

## Threat entries

### TM-01: Out-of-bounds linear memory access
Guest executes loads/stores whose effective address falls outside its
linear memory (edge cases: base+size straddling the last page,
offset+address overflow wrapping to a small value, `memory.grow`
interactions shrinking effective bounds).
**Expected containment:** trap (`MemoryOutOfBounds`), no host memory
outside the guest allocation is touched.

### TM-02: Malformed or binary-patched modules
Loader/validator defects reachable from hostile bytes: truncated
sections, invalid LEB128, length fields overruning the input buffer,
section-order violations, duplicate sections, excessive memarg
alignments, `if`-without-`else` arity abuse, non-declared `ref.func`,
tag sections.
**Expected containment:** rejected at load, no panic, no read beyond
the input buffer.

### TM-03: Table, global, and index-space abuse
Out-of-bounds table indices, `call_indirect` on out-of-bounds/null/
type-mismatched entries, global index abuse past declared globals,
element-segment offsets past table bounds.
**Expected containment:** rejected at validation or trap at runtime.

### TM-04: Host-call abuse
Guest invokes host imports with adversarial argument values designed
to induce out-of-bounds access, huge sizes, or type confusion in the
host function or in argument marshalling.
**Expected containment:** host function rejects safely; runtime
remains intact.

### TM-05: Resource exhaustion
Infinite loops, deep recursion, `memory.grow` spam, stack exhaustion —
attempts to exhaust runner CPU, memory, or wall-clock budget.
**Expected containment:** configured limits enforced; guest terminated
within the configured budget (fuel/depth trap, allocation failure, or
budget timer). On the AOT path the execution budget is enforced by the
inline fuel charge at function entry and each loop back-edge, which traps
`ExecutionBudgetExceeded`. The charge targets an invocation-local cell
whose budget field is the allowance remaining in the instance meter at the
invocation's most recent flush point (host call or invocation start), so a
runaway loop cannot outrun the budget by more than one charge between
flushes; the shared counter is written once per host call and once per
invocation, so concurrent invocations of one instance do not contend on
one cache line.

### TM-06: Shared-region boundary probing
Guests attempting to access shared regions beyond granted bounds:
atomic operations on offsets past the region, cross-instance access to
regions not attached to them, wait/notify on addresses outside the
shared mapping.
**Expected containment:** denied or trapped; no access outside the
granted region.

### TM-07: Loader input fuzzing (unknown-unknowns)
Byte-level mutations of valid and malformed modules exercising parser
and validator paths no hand-written fixture predicts.
**Expected containment:** load outcome is `Ok` or `Err` — never a
panic, abort, or non-termination.

### TM-08: Interpreter dispatch fuzzing (unknown-unknowns)
Mutated-but-loadable modules reaching interpreter execution, plus
adversarial argument values to exported functions.
**Expected containment:** execution result is `Ok` or trap — never a
panic in the runtime.

### TM-09: Shared-region API fuzzing (unknown-unknowns)
Fuzzed region sizes, offsets, protections, and attach/detach
sequences against the shared-memory registry and mapping code.
**Expected containment:** API result is `Ok` or `Err` — never a panic,
never an out-of-grant mapping.

## Known limitations (accepted residual risk)

- The corpus encodes only *known* techniques; novel escapes pass by
  construction. Fuzzing (TM-07..TM-09) and external audit cover part
  of this gap.
- Canaries check *after* a fixture completes; a fast escape that
  cleans up after itself evades them. Per-fixture resource caps bound
  what an escape can do while the process lives.
- CI resource caps bound damage to the job; they are belt-and-braces
  around the runtime's own containment, not a substitute for it.
