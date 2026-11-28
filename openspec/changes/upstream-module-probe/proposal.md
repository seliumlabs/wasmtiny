# Proposal: Module probe API

## Why

Embedders classify guests from facts in the wasm binary — shared-memory
declarations, exports, imports, custom sections, and whether the code contains
an atomic notify. Today each embedder reimplements a section cursor, a LEB128
reader, and import/memory/opcode scans. The copies duplicate engine knowledge,
drift from the parser, and put engine-format bugs in the wrong repository.

## What Changes

- Retain custom sections (name and payload) on the parsed `Module` instead of
  discarding them.
- Record on the parsed `Module` whether the code section contains the
  `memory.atomic.notify` opcode sequence.
- Add a fact-only `probe(bytes)` entry point that reports memories, imports,
  exports, custom sections, and atomic-notify presence. The probe parses and
  validates with the engine loader, so malformed input returns `Err`.

The probe reports facts, not decisions. Fast-path eligibility, worker-pool
selection, and ABI negotiation remain embedder policy.

## Impact

- `wasmtiny` core: `loader::probe`, `runtime::CustomSection`, and two new
  `Module` fields.
- No ABI, artifact-format, or execution change.
