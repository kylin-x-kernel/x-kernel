# kiface-logging-api Design

LogSinkIf routes formatted text; LogContextIf samples time, optional CPU/task
identity, and whether the caller runs in non-maskable-interrupt context (`in_nmi`,
which the logger asks before taking its locks). Runtime supplies both contracts. klogger owns formatting/filtering, while concrete console and task subsystems retain their state. Optional log identity deliberately differs from TaskExecutionIf's mandatory nonzero owner identity.

See [capability ownership rules](../../../docs/design.md) for the common
contract/provider boundary. This crate contains no implementation state.
