# kiface-logging-api Security

This analysis covers the entire crate, `src/lib.rs`.

Providers cannot recurse into logging. The sink must not sleep while called under logger exclusion. CPU/task IDs may be unavailable before runtime initialization; do not invent a current task. This is ordinary logging, not a replacement for emergency NMI output.

See [capability ownership rules](../../../docs/design.md) for the common
contract/provider boundary. This crate contains no implementation state.
