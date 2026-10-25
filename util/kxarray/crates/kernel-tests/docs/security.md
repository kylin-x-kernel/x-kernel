# Test Safety

Fixtures execute only in kernel unittest images, with trusted constructed values
and no external input. They use the permanent boot RCU and task services, never
replace global providers, and restore changed task affinity before assertions.

Unsafe operations adopt callback-safe payloads, inspect retained task reader
storage, construct owned callback records, or enter balanced software NMI context.
Their call sites retain the corresponding ownership and execution-context proofs.
Timers are synchronously cancelled before captured state is released; readers
exit before callback draining and destruction counts are checked.

Publication observers must not allocate, wait or mutate the locked array.
Software NMI fixtures explicitly mask IRQs for entry; the writer lock does not.
They are controlled-context evidence, not hardware NMI or exhaustive memory-order
proof. A stalled delivery/migration may prevent in-guest cleanup; the external
bounded QEMU runner terminates the disposable test image.
