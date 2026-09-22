## Purpose

Core runtime semantics for loaded WebAssembly modules: module representation and instantiation, isolated memories and tables, globals, cross-module import aliasing, trap handling, error handling, thread safety, and build configurations.

## Requirements

### Requirement: Consumer-driven public API surface
Public API items SHALL exist only where they serve the interpreter-based embedder use case (module loading, host-function registration, instantiation, invocation, shared-region management, memory access). Items without any caller in the crate or its known embedder SHALL be removed rather than retained.

#### Scenario: No dead convenience methods
- **WHEN** the public APIs of `engine`, `runtime`, and `application` modules are audited for callers
- **THEN** every public method SHALL have at least one caller in the crate, the test suites, or the known embedder (Selium)

### Requirement: Module initialization
The runtime SHALL provide a `Module` struct representing a loaded WASM module with types, functions, memories, tables, globals, and exports.

#### Scenario: Loaded module exposes its sections
- **WHEN** a valid wasm binary containing types, functions, memories, tables, globals, and exports is loaded
- **THEN** the returned `Module` exposes those collections and exports resolve by name

### Requirement: Instance creation
The runtime SHALL allow instantiation of a module into an `Instance` with isolated linear memory and table spaces. Instance construction and binding SHALL be managed by the core engine; per-invocation instance state SHALL be cached and reused across calls to the same loaded module rather than rebuilt from a cloned module.

#### Scenario: Instantiation through the engine
- **WHEN** a loaded module is instantiated via `WasmApplication::instantiate`
- **THEN** an instance with isolated linear memory and table spaces is created and associated with that loaded module

### Requirement: Memory access
The runtime SHALL provide safe read/write access to linear memory with bounds checking. Memory access SHALL include both owned pages and mapped shared region pages. Writes to read-only shared pages SHALL trap before reaching memory.

#### Scenario: Out of bounds memory access
- **WHEN** a WASM module attempts to read memory at an offset beyond allocation
- **THEN** a trap error is returned with `TrapCode::MemoryOutOfBounds`

#### Scenario: Errors carry typed information
- **WHEN** a trap or validation failure is returned
- **THEN** the error value SHALL expose its kind programmatically (variant + typed fields), not solely via message text

#### Scenario: Write to read-only shared region
- **WHEN** a WASM module attempts to write to a shared region page mapped with `PROT_READ`
- **THEN** a trap error is returned with `TrapCode::MemoryOutOfBounds`

#### Scenario: Read from mapped shared region
- **WHEN** a WASM module reads memory at an offset within a mapped shared region
- **THEN** the read SHALL succeed and return the shared memory contents

### Requirement: Table operations
The runtime SHALL support WebAssembly table operations including get, set, and size.

#### Scenario: Table get, set, and size
- **WHEN** a guest executes `table.get`, `table.set`, and `table.size` on an in-bounds table
- **THEN** values are read and written at the requested indices and the reported size matches the table's element count

#### Scenario: Out-of-bounds table access traps
- **WHEN** a guest executes `table.get` or `table.set` at an index at or beyond the table size
- **THEN** execution traps with `TrapCode::TableOutOfBounds`

### Requirement: Cross-module import aliasing
The runtime SHALL preserve shared state for imported guest functions, tables, memories, and globals across module boundaries. Imported tables SHALL be shared by reference (mutations visible to all importers), and nested instantiation for imported guest functions SHALL share the caller's store (native registry and shared-memory registry).

#### Scenario: Imported table aliases exported table state
- **GIVEN** module A exports a table and module B imports that table
- **WHEN** module B mutates the imported table contents
- **THEN** subsequent reads through module A SHALL observe the same table contents

#### Scenario: Imported guest function binding executes real guest code
- **GIVEN** module A exports a WebAssembly function and module B imports it
- **WHEN** module B calls the imported function directly or through a funcref stored in a table
- **THEN** the exported WebAssembly function body from module A SHALL execute with the correct type checks and results, with access to the same store state as the caller

### Requirement: Global variables
The runtime SHALL support reading and writing mutable global variables.

#### Scenario: Mutable global read/write
- **WHEN** a module declares a mutable global and a function writes then reads it
- **THEN** the read returns the written value, and the value persists across invocations sharing the module's state

### Requirement: Trap handling
The runtime SHALL propagate traps as errors and provide trap codes for common failure modes.

#### Scenario: Division by zero traps with a typed code
- **WHEN** a guest executes an integer division where the divisor is zero
- **THEN** execution returns an `Err` carrying `TrapCode::IntegerDivisionByZero`

#### Scenario: Trap propagates to the caller
- **WHEN** an exported function traps during invocation
- **THEN** the error returned to the host preserves the typed `TrapCode`

### Requirement: Callback-safe lock discipline
The engine SHALL NOT hold any store, instance, memory, or registry lock across a call into embedder-provided code (`HostFunc` implementations), and lock acquisition order across these objects SHALL follow a single global order to prevent ABBA deadlock.

#### Scenario: Host callback may re-enter engine APIs
- **WHEN** a `HostFunc` implementation calls back into engine APIs that acquire store or registry locks
- **THEN** the call SHALL complete without deadlock

#### Scenario: Concurrent attach and instance drop
- **WHEN** one thread attaches/detaches shared regions on a memory while another thread drops an instance sharing that memory and registry
- **THEN** both operations SHALL complete without deadlock and attachment accounting SHALL remain consistent

### Requirement: Error handling
The runtime SHALL use `Result<T, WasmError>` for all fallible operations with structured error types. `WasmError` SHALL use structured, typed variants (via `thiserror`) rather than free-form string payloads where variant data has known shape; variants constructed or matched by known embedders (`Runtime`, `Instantiate`) SHALL remain constructible/matchable with compatible shapes or be migrated with the embedder.

#### Scenario: Fallible operations return typed errors
- **WHEN** any fallible runtime operation (loading, instantiation, invocation, memory or table access) fails
- **THEN** the result is an `Err(WasmError)` whose typed variant embedders can match programmatically without parsing message text

### Requirement: Thread safety
The runtime SHALL support `Send + Sync` on types where it is safe to share across threads. An instance SHALL support concurrent invocation of its exported functions from multiple host threads, each invocation carrying its own execution context (stack) and sharing the instance's linear memory, tables, and globals coherently, without serialising the whole instance behind a single instance-wide lock. Atomic instructions and `memory.atomic.wait`/`memory.atomic.notify` SHALL remain observable across concurrent invocations, and a thread parked in `memory.atomic.wait` SHALL NOT serialise the instance or prevent other threads from executing it.

#### Scenario: Shared instance invoked from multiple threads
- **WHEN** an instance wrapped in an `Arc` has its exported functions invoked concurrently from multiple threads
- **THEN** all invocations complete correctly without data races, panics, or lock poisoning

#### Scenario: Concurrent invocations share state coherently
- **WHEN** two threads invoke functions of the same instance concurrently and each reads or writes shared memory and globals
- **THEN** writes made by one invocation SHALL be observable to the other, with ordering governed by the module's atomics and memory model, and no instance-wide lock SHALL serialise the executions

#### Scenario: One invocation does not block another
- **WHEN** one thread is executing a long-running invocation of an instance
- **THEN** another thread MAY execute a different invocation of that same instance concurrently without waiting for the first to finish

#### Scenario: Waiter parks without blocking other invocations
- **WHEN** one thread is parked in `memory.atomic.wait` on a shared address
- **THEN** another thread SHALL still be able to invoke the instance and SHALL be able to notify the parked waiter

#### Scenario: Memory budget holds under concurrent growth
- **WHEN** multiple invocations grow an instance's memory concurrently while the instance has a memory-page budget configured (see `instance-metering`)
- **THEN** the budget check and the growth commit SHALL be atomic with respect to each other, so the instance's committed pages SHALL never exceed the configured budget

### Requirement: Bounded per-invocation engine cost
Calling an exported function on an already-instantiated module SHALL NOT deep-clone the module, SHALL reuse the module's instance state, and SHALL NOT permanently grow any engine registry (funcref store, native table) as a function of call count.

#### Scenario: Repeated calls do not grow the store
- **WHEN** an exported function is called N times on the same loaded module
- **THEN** engine registry sizes (e.g. the funcref store) SHALL be the same after the first call as after the Nth

#### Scenario: Repeated calls reuse instance state
- **WHEN** an exported function mutates memory or globals and is called again later
- **THEN** the second call SHALL observe the prior call's mutations (state persists across invocations of the same loaded module)

### Requirement: Value codec round-trip fidelity
`WasmValue` byte serialisation (`to_bytes`/`from_bytes`) SHALL round-trip every representable variant exactly, using self-consistent type tags.

#### Scenario: NullRef round-trip
- **WHEN** `WasmValue::NullRef(RefType::ExternRef)` is serialised and deserialised
- **THEN** the result SHALL equal the original value (reference kind preserved)

#### Scenario: All-variant round-trip
- **WHEN** any `WasmValue` (I32, I64, F32, F64, FuncRef, ExternRef, NullRef of either kind) is serialised and deserialised
- **THEN** the result SHALL equal the original value

### Requirement: Mmap-Backed Memory
Guest linear memory SHALL be backed by an `mmap`-based allocation rather than `Vec<u8>`, with a pre-reserved virtual address range supporting growth via `mprotect`.

#### Scenario: Memory created with mmap backing
- **WHEN** a `Memory` is created with `min_pages = 1`
- **THEN** the underlying storage SHALL be an `mmap`'d region with the full maximum virtual address range reserved

#### Scenario: Memory growth extends accessible range
- **WHEN** `Memory::grow(1)` is called
- **THEN** the additional pages SHALL be made accessible via `mprotect` without reallocation

### Requirement: Shared Page Tracking
The `Memory` struct SHALL track which page ranges are owned vs. mapped from shared regions, including the region identifier and protection level for each shared range.

#### Scenario: Shared range queryable
- **WHEN** a guest has attached shared regions
- **THEN** the `Memory` SHALL report the page offsets, region IDs, and protection levels of all attached regions

#### Scenario: Shared instance across threads
- **WHEN** an `Arc<Instance>` is created and shared between threads
- **THEN** compilation succeeds only if the instance is thread-safe

### Requirement: Execution build configurations
The crate SHALL build with the AOT execution path enabled by default (via the `aot` feature). The classic interpreter and `.wasm` parsing SHALL remain compiled and available across builds, and an `interpreter` feature SHALL exist to select the interpreter path for interpreter-only and differential AOT+interpreter builds.

#### Scenario: Default build executes AOT artifacts
- **WHEN** the crate is built with default features
- **THEN** it loads, verifies, and executes `.aot` artifacts through the AOT path

#### Scenario: AOT plus interpreter build
- **WHEN** the crate is built with both the AOT and `interpreter` features enabled
- **THEN** both execution paths are available for differential testing