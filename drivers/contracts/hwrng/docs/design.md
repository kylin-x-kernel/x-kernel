# hwrng — Design

## Purpose and scope

`src/lib.rs` defines the OS-independent hardware entropy read contract;
`src/completion.rs` defines the host-injected wait capability. Neither owns
scheduler state, DMA storage, entropy accounting, or source trust policy.

## Architecture and execution context

Consumers use `HwrngDevice::read`; concrete drivers receive an
`Arc<dyn RngCompletion>` implemented by the host. Nonempty reads require
sleepable task context, including kernel workers; no process identity is needed.
Empty reads return zero without I/O. Successful nonempty reads may be short.
Drivers must keep in-flight DMA storage independent of the caller's buffer.

## Wait flow and concurrency

`wait_until` registers for notification before checking its predicate. A true
predicate commits the result. Spurious notification only causes another
predicate check. Task interruption does not cancel this internal read. The
driver publishes completion or a terminal fault under its own lock and calls
IRQ-safe `notify` after releasing it. No device or IRQ-disabling lock may span
a wait. There is no polling timeout.

## Failures and resource lifecycle

Reads and host waits return the shared `DriverError`. `InvalidInput` identifies
a context that cannot sleep, while `NoMemory` can mean wait registration
failed. These errors alone do not establish device failure, and the error type
does not encode whether a failure came from the host or the device. Entropy
consumers must not permanently disable a source for every read error. A read
error delivers no bytes and does not invalidate a submitted DMA request. The
contract does not define hotplug, reset or an offline event protocol; the
driver and host own resource lifetime.
