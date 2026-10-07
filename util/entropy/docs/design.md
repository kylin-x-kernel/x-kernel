# entropy — Design

## Scope

The crate owns the ChaCha20 entropy pool and source-selection policy. Sources
are CPU instructions, SMCCC TRNG, VirtIO RNG and software jitter, each gated by
its Cargo/Kconfig feature. `source.rs` owns the common trait, availability and
trust types, registry, selection functions and shared read helpers.
The four `EntropySource` implementations live in `source/arch_cpu.rs`,
`source/smccc_trng.rs`, `source/virtio.rs` and `source/jitter.rs`.
They delegate to the existing crate-level backends; `virtio.rs` adapts the
Hwrng class into asynchronous pool replenishment.

## Architecture and execution context

Initialization mixes eligible eager sources and registers VirtIO discovery.
VirtIO discovery enumerates existing devices and subscribes to later class
availability. It does not synchronously read hardware during probe or boot.
Enabling `entropy_virtio_rng` trusts host-provided entropy for pool seeding.

Each source prepares one reusable `kworkng::WorkHandle` during registration,
after normal workqueue services are ready. A demand queues one callback that
performs blocking `HwrngDevice::read`, mixes the result, and returns. No
dedicated entropy task or idle demand wait remains. An unresponsive device or
persistent retry errors can still occupy one shared worker and its queue active
allowance indefinitely; this migration does not add completion timeouts.

## State and concurrency

`SELECTED_SOURCE` holds the selected `VirtioEntropySource` behind a mutex.
Its `EntropyCollector` owns `has_demand` and `is_failed` in a
`SpinNoIrq<CollectorState>`. Demand publication and
nonblocking work submission share that lock. The same Work serializes callbacks
and permits one pending occurrence while running, so requests during callback
exit are not lost. AlreadyQueued coalesces demand; other submission errors are
logged and restore the previous demand flag. No read or retry sleep holds the
source lock. Work preparation failure is logged before publishing the source.
The pool mutex protects mixing; readiness is published after the mix completes.
The source's synchronous sampling method requests work and returns `NoEntropy`
so the caller continues checking other sources.
After a reseed attempt yields no immediate sample, the next attempt still waits
for another output interval; the pending VirtIO read does not trigger a new
request on every pool read.

## Failures and resource lifecycle

A read error only permanently fails the source when `HwrngDevice::is_failed`
confirms a terminal device failure. Otherwise the same worker retains its
current demand and retries after a 100 ms sleep, without waiting for another
pool read or getrandom call. Repeated failures log once per demand and never
busy-loop. This delay backs off failed read setup; it does not time out or poll
an in-flight device request. Invalid successful lengths remain contract
violations and permanently fail the source. New demands still coalesce while
recovery runs. Context errors are not evidence of device failure; this worker
always calls read with no source/device lock in sleepable task context.
Retries have no failure-count cutoff; a persistent host failure can still
prevent seeding, but cannot establish a terminal device failure.
A failed source can be replaced by a newly discovered device instance.

The source and Work both retain an Arc to the collector; the collector does not
own Work, avoiding an ownership cycle. The callback retains the class handle.
Device hot removal and rebind are not supported by the VirtIO adapter. The collector does not reset hardware
or release outstanding DMA; those responsibilities stay with the driver/host.

## Validation

Controlled collector tests cover demand coalescing, return to idle, reuse,
repeated read errors without new demand, successful recovery, and explicit
terminal device failure.
Real-device QEMU tests cover empty/short reads, destination guards and repeated
queue reuse. A separate VirtIO-only guest getrandom test exercises initialization
and replenishment without other credited entropy sources.

## VirtIO source configuration

`KFEAT_DRIVER_VIRTIO_RNG` only enables the concrete driver and Hwrng class.
`KFEAT_ENTROPY_VIRTIO_RNG` independently enables this crate's VirtIO source
adapter and depends on the driver switch in Kconfig. `entropy` consumes the
Hwrng contract and class registry, not the concrete driver crate.
Enabling the source also trusts host-provided bytes: they are mixed into the
pool and may establish readiness without another entropy source. There is no
separate trust switch. All QEMU/crosvm defconfigs enable the driver and source
to preserve their existing behavior. Isolated deployments that do not trust
the host must disable the source; the driver may remain enabled.
When migrating a configuration that formerly enabled the source but disabled
host trust, disable `KFEAT_ENTROPY_VIRTIO_RNG` to preserve that policy.
