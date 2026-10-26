# kiface-console-api Security

Emergency output cannot allocate or acquire ordinary console locks and may truncate output. Input setup requires serialized sleepable boot-task execution before TTY publication; failure retains polling. Report installed IRQ readiness, not firmware metadata. Ordinary console I/O is not automatically NMI safe.

See [capability ownership rules](../../../docs/design.md) for the common
contract/provider boundary. This crate contains no implementation state.
