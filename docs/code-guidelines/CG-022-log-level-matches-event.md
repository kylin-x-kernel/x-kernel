---
id: CG-022-log-level-matches-event
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: ai
triggers:
  - "warn!"
  - "debug!"
  - "trace!"
  - "error!"
---

# CG-022-log-level-matches-event: Match logging levels to severity and frequency

- **Rule**: Log statements SHOULD use levels matching the event's severity and
  frequency, reserve `warn!` for problems deserving operator attention, and avoid
  debug or trace volume that drowns out signal without a concrete debugging need.
- **Scope**: Logging in all Rust code. `warn!` covers recoverable problems,
  degraded behavior, dropped work, fallback paths, and unexpected but survivable
  states deserving operator attention, not routine expected control flow.
  `debug!` covers development diagnostics, state transitions, resource lifecycle
  details, and infrequent internal observations too noisy for routine operation.
  `trace!` is only for very frequent or highly detailed diagnostics such as
  per-packet, per-event, per-iteration, or tight-loop instrumentation.
  Not this rule: [CG-021](CG-021-no-panic-on-reachable-failure.md) owns
  panic/release-validation behavior and the pending Dylint lint `untyped_error_result` ([pending](pending.md))
  owns unit/string `Result` errors; this rule owns a log event's severity, frequency
  and debugging value. Whether logging replaces a required error return
  remains error-handling guidance in
  [the skill](../ai/skills/code-guidelines/error-handling-and-logging.md),
  rather than a second severity finding.
- **Why**: Severity and frequency determine whether logs help operators and
  developers find useful events or bury them in routine output.
- **How to check**:
  1. Identify the actual event, its severity, and whether it needs operator
     attention or serves development diagnostics.
  2. Inspect the call path and loop context to estimate frequency and detail;
     compare the chosen level with the scope above.
  3. For debug or trace output with substantial volume, identify the concrete
     debugging need and check whether it would drown out useful signal.
  4. For logs that duplicate caller-visible error handling, require added
     diagnostic signal rather than repeating the already-reported failure.
- **Exceptions**:
  1. None.
- **Counter-example**:

```rust,ignore
warn!("packet received: len={}", packet.len());
```

A routine packet arrival does not deserve a warning and can flood operator logs.

- **Example**:

```rust,ignore
// Per-packet details are needed to diagnose receive-length mismatches.
trace!("packet received: len={}", packet.len());
warn!("receive queue full; dropping packet");
```
