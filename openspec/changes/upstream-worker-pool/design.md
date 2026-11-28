# Design

## Decisions

### Policy crosses the boundary through two callbacks

`StopSignal::signal` is the only way the engine asks the embedder to stop its
workers; the embedder decides what a stop is (an ABI stop word plus wake-word
notify, a channel close, a flag). The finish callback receives a `PoolReport`
and decides what to log, drain, or reap. Neither callback can introduce an
embedder type into the engine.

### Fault capture happens on the worker thread

`last_trap_site` is thread-local and valid only immediately after the failed
invocation, so the worker records the trap code and site before delivering the
stop. The report carries the raw engine error text; presentation is the
embedder's job.

### Stop retries, wedge detaches

Each notify may release exactly one parked worker, so an N-worker pool needs N
deliveries; the stop path re-delivers on a short cadence until the monitor
reports every worker joined. If the budget expires, the monitor handle is
dropped rather than joined: joining a worker that trapped inside a critical
section would hang teardown forever. `StopOutcome::Wedged` tells the caller to
reclaim the process.

### Partial spawn still monitors

If one spawn fails, workers that did start are still joined by a monitor, and
the stop signal is delivered first so they return instead of parking forever.
The spawn error is returned to the caller.

## Risks

- **Lost wake deliveries** — covered by re-delivery on a cadence plus the
  embedder's stop signal making a not-yet-parked worker return immediately.
- **Leaked wedged threads** — they hold the shared instance and its memory, so
  the leak is bounded and the process is reclaimed regardless.
