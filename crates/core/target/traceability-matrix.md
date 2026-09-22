# Traceability Matrix

| Threat | Title | Coverage |
|---|---|---|
| TM-01 | Out-of-bounds linear memory access | fixture `oob-load-overflow-addr`, fixture `oob-store-wrap`, fixture `oob-load-edge`, fuzz `aot-native-dispatch` |
| TM-02 | Malformed or binary-patched modules | fixture `malformed-excessive-memarg`, fixture `malformed-length-overrun`, fixture `malformed-truncated`, fixture `malformed-tag-section`, fixture `malformed-bad-leb128`, fuzz `aot-loader-verifier` |
| TM-03 | Table, global, and index-space abuse | fixture `elem-segment-past-table`, fixture `table-callindirect-null`, fixture `global-index-oob`, fixture `table-callindirect-oob`, fuzz `aot-native-dispatch` |
| TM-04 | Host-call abuse | fixture `hostfn-straddle-read`, fixture `hostfn-huge-len`, fixture `hostfn-negative-offset` |
| TM-05 | Resource exhaustion | fixture `exhaust-memory-grow-spam`, fixture `exhaust-deep-recursion`, fixture `exhaust-infinite-loop` |
| TM-06 | Shared-region boundary probing | fixture `region-atomic-oob` |
| TM-07 | Loader input fuzzing (unknown-unknowns) | fuzz `loader-validator`, fuzz `aot-loader-verifier` |
| TM-08 | Interpreter dispatch fuzzing (unknown-unknowns) | fuzz `interpreter-dispatch`, fuzz `aot-native-dispatch` |
| TM-09 | Shared-region API fuzzing (unknown-unknowns) | fuzz `shared-region-api` |
