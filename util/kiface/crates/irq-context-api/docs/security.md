# kiface-irq-context-api Security

Providers must inspect NMI before ordinary context state and preserve IRQ/preemption state. Queries neither schedule nor perform cleanup. A snapshot grants no lasting permission to sleep; callers must preserve their own exclusion around dependent operations.

See [capability ownership rules](../../../docs/design.md) for the common
contract/provider boundary. This crate contains no implementation state.
