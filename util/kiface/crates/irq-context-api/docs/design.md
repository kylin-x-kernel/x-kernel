# kiface-irq-context-api Design

The common IRQ core provides InterruptContextIf directly. The broad interrupt-context predicate includes BH-disabled tasks; the handler predicate includes only actual NMI, hardirq and softirq execution. Consumers retain admission policy. This crate has no descriptor, context storage or dispatch machinery.

See [capability ownership rules](../../../docs/design.md) for the common
contract/provider boundary. This crate contains no implementation state.
