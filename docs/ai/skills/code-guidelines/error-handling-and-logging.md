# Error Handling And Logging

Use this file when the change touches `Result`,
error mapping, retry behavior, or log statements.

## Error Handling

- return typed errors that match the layer's contract;
- validate at subsystem boundaries,
  then trust validated internal invariants;
- format error messages consistently and specifically;
- for syscalls or Linux-shaped interfaces,
  keep user-visible error semantics aligned with the ABI contract.

## Logging

- follow the logging facade and macro style already used by the touched crate;
- keep crate-local logging style consistent rather than introducing a new one ad hoc;
- do not replace proper error returns with logs alone.

## When Reviewing

Check specifically for:

- weak or lossy error mapping at subsystem boundaries;
- new logging style drift inside a crate that already has an established pattern;
