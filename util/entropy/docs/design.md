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

Guest unit tests live in crate-root `ut-tests/` and are mounted with
`unittest::path_mod_test!` in their original private parent modules.
All seven modules retain `cfg(unittest)`, their module names and parent access.

| Test file | Module | Cases | Additional gate |
| --- | --- | --- | --- |
| `ut-tests/lib.rs` | `tests` | 12 | None |
| `ut-tests/source.rs` | `source::tests` | 8 | One case requires `entropy_virtio_rng`; existing in-body source gates remain |
| `ut-tests/arch_cpu.rs` | `arch_cpu::tests` | 2 | `entropy_arch_cpu` |
| `ut-tests/smccc_trng.rs` | `smccc_trng::tests` | 2 | `entropy_smccc_trng` and AArch64 |
| `ut-tests/jitter.rs` | `jitter::tests` | 5 | `entropy_jitter` |
| `ut-tests/virtio.rs` | `virtio::tests` | 2 | `entropy_virtio_rng` |
| `ut-tests/virtio_real_device_tests.rs` | `virtio::real_device_tests` | 3 | `entropy_virtio_rng` |

There are 34 cases across all gates: 19 without additional case/module gates,
two CPU, five jitter, two AArch64 SMCCC, and six VirtIO-dependent cases.
Four cases retain `serial`: the controlled collector case and all three
real-device cases. No cases carry a `user` flag. The real-device module remains
registered, requires a published VirtIO RNG device, and fails rather than skips
when no device is present. Use the configured kernel guest unittest workflow;
these files are not host Cargo integration-test targets.

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
