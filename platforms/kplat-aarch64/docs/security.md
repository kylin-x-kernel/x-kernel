# kplat-aarch64 Security Scope

## Scope

This note covers the guest tests in `ut-tests/peripherals_smccc.rs` and
`ut-tests/peripherals_trng.rs` and their mounts in `src/peripherals/smccc.rs`
and `src/peripherals/trng.rs`, plus PMU mapping preparation in
`src/peripherals/pmu.rs` and its NMI preparation integration with
`src/peripherals/nmi.rs` and global PMU retirement in
`src/peripherals/pmu/lifecycle.rs`. It is not a whole-platform unsafe-code audit.

## Local NMI Preparation

Periodic PMU startup holds one IRQ/preemption guard across counter setup,
callback registration, PPI preparation, producer start and any rollback. The
individual per-CPU access helpers alone cannot prevent migration between those
steps. This guard does not mask NMI or serialize remote management operations.

The platform must initialize the CPU's mechanism before preparing a PMU PPI.
Preparation precedes IRQ enable, with IRQ/migration exclusion retained through
KIRQ admission. Failed preparation does not activate the line. The producer
owner serializes counter start/stop against any shutdown; a successful prepare
call alone is not a lifetime lease for a later counter start. Current startup
has no concurrent source teardown; terminal quiescence deliberately retains the
registered callback and preparation. Full retirement still requires the source
shutdown and NMI-reader exclusion documented by KIRQ.

Global reusable shutdown closes source admission before claiming the original
Line. Local start/enable checks run inside the full CPU/IRQ-pinned transaction;
the same CPU cannot execute the ordinary IPI stop callback halfway through it.
The global mutex is never acquired by NMI dispatch or target callbacks. The
retained claim and pending CPU set survive failed transport, teardown or final
RCU-wait preparation. Only completed CPU obligations are removed from that set.
Restoration is rejected until shutdown has retired the old action; admission
reopens only after a new handler is published. No automatic counter restart or
watchdog-consumer reinitialization is implied.

The owner requires boot-static CPU services and exclusion from system stop and
CPU hotplug. Transport success proves callback/capture completion, not completion
of the surrounding IPI controller claim. It is the PMU stop and GIC teardown
inside the callback that establish source quiescence. RCU independently protects
old software borrows through their original claim completion; it cannot clear a
pending interrupt or prevent a newly generated event. Both proofs are required
before reusing the registration. Normal-IRQ-only PMU builds reject global NMI
retirement as unsupported.

## PMU Mapping Preparation

Reusable periodic shutdown in `src/peripherals/pmu.rs` pins all local steps and
rejects NMI, unavailable mechanisms and other allocated counters. Stopping the
producer does not authorize mutable access to its storage: the GIC backend must
also confirm disable completion, reject active claims and clear pending state.
Only then may the unsafe `PMU.get_mut_unchecked` clear counter/callback slots.
IRQ/preemption exclusion spans this proof and mutation; it is not NMI exclusion.
Failure preserves storage and the acquired disable credit for retry. The source
owner excludes concurrent restart/global handler removal and collects each
participant's completed operation; this API does not establish remote quiescence.

An absent cycle counter does not prove the local PPI is unprepared. The shutdown
path still requires controller teardown before reporting success on a CPU whose
line was prepared at boot but whose periodic source was never armed. All other
counter slots must remain unallocated under the same exclusive-source contract.
The independent `PERIODIC_IRQ_DISABLED` CPU slot retains the disable credit
even if the lazy PMU manager has never been initialized. All accesses use the
whole-transaction CPU/IRQ guard; relaxed atomics do not publish shutdown to
remote CPUs. Source-owner transport completion supplies that separate ordering.
Successful teardown does not release this credit. The marker survives repeated
shutdowns until successful local rearm, so two closes cannot leave a hidden
disable depth that makes a subsequent counter start appear to enable delivery.

`ut-tests/peripherals_pmu_shutdown.rs` covers operation order and failure retry
without devices. Its exclusive-source integration case uses live PMU/GIC state
only when no counter is allocated, and skips rather than taking over a watchdog.
`ut-tests/pmu_lifecycle.rs` uses actual PMU overflow callbacks on each boot CPU,
real IPI execution and GIC teardown. An injected failed CPU step checks retained
ownership and retry without revisiting completed CPUs. Callback acknowledgements
drive progress, with external QEMU timeout only as a hang diagnostic. These tests
do not establish silicon-specific late-delivery or hardware-NMI correctness.

Startup owns PMU mapping preparation and keeps the source quiescent until
handler installation and the existing activation/promotion steps permit
delivery. A mapping error is logged and rejects registration before handler
allocation. Successful mapping alone neither enables hardware nor establishes
NMI quiescence. An installation failure retains the platform-owned mapping;
no early-boot RCU disposal or rollback of controller writes is promised.

## Boundaries

The SMCCC tests call pure conduit helpers, not HVC/SMC assembly, and do not
change the global conduit. The TRNG tests supply local closures and buffers;
they do not call firmware or change the global availability flag. Neither
test file contains unsafe code or accesses live peripheral registers.

The original architecture/platform gates and `cfg(unittest)` keep these
modules out of non-test builds. Private module visibility is retained;
extraction does not widen the production API or change the existing unsafe
firmware-call contracts. These tests check result handling, not firmware
quality or entropy quality.
