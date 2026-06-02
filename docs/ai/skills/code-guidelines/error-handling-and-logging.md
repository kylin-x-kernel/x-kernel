# Error Handling And Logging

Use this file when the change touches `Result`,
error mapping, retry behavior, or log statements.

## Error Handling

- use `?` for ordinary error propagation;
- avoid `.unwrap()` and hidden panics
  where failure is a legitimate possibility;
- return typed errors that match the layer's contract;
- validate at subsystem boundaries,
  then trust validated internal invariants;
- use `debug_assert!` only for correctness checks
  that must not become runtime policy in release builds.
- format error messages consistently and specifically;
- for syscalls or Linux-shaped interfaces,
  keep user-visible error semantics aligned with the ABI contract.

## Logging

- follow the logging facade and macro style already used by the touched crate;
- choose log levels that match the real severity;
- keep crate-local logging style consistent rather than introducing a new one ad hoc;
- use `warn!` for recoverable problems, degraded behavior, dropped work, fallback paths, or unexpected-but-survivable states that deserve operator attention;
- use `debug!` for development diagnostics, state transitions, resource lifecycle details, and infrequent internal observations that are useful during debugging but too noisy for routine operation;
- use `trace!` only for very high-frequency or highly detailed diagnostics such as per-packet, per-event, per-iteration, or tight-loop instrumentation;
- avoid `warn!` for routine expected control flow;
- avoid `debug!` and especially `trace!` when the resulting volume would drown out signal without a concrete debugging need;
- do not replace proper error returns with logs alone.

## When Reviewing

Check specifically for:

- unwrap-like failure paths hidden inside normal control flow;
- weak or lossy error mapping at subsystem boundaries;
- runtime validation accidentally downgraded into `debug_assert!`;
- logs at the wrong severity;
- logs that duplicate caller-visible error handling without adding signal;
- new logging style drift inside a crate that already has an established pattern;
- `warn!`, `debug!`, or `trace!` used at a level inconsistent with the frequency or severity of the event.
