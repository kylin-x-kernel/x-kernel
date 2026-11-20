# kplat-aarch64 Security Scope

## Scope

This note covers the guest tests in `ut-tests/peripherals_smccc.rs` and
`ut-tests/peripherals_trng.rs` and their mounts in `src/peripherals/smccc.rs`
and `src/peripherals/trng.rs`, plus PMU mapping preparation in
`src/peripherals/pmu.rs` and its NMI preparation integration with
`src/peripherals/nmi.rs` and global PMU retirement in
`src/peripherals/pmu/lifecycle.rs`. It is not a whole-platform unsafe-code audit.

## Local NMI Preparation

PMU registration relies on the early-initialized, stable runtime NMI mode.
Even with `nmi-pmu` compiled, mode `None` installs an ordinary IRQ action and
skips NMI preparation at enable. Ordinary PPI registration uses the dedicated
disabled PerCpu startup API in both `nmi-pmu` and PMU-only builds; runtime CPU
services explicitly activate each local line. KIRQ rejects a mismatched resolved
flow rather than falling back to generic registration. Ordinary SPI registration retains the existing activation policy: disabled
with `nmi-pmu`, auto-enabled without it, with `restrict_user_affinity()` added
in both cases. This is a user-only policy carried by the regular action; it
does not block the trusted kernel setter, alter initial routing, or make the
single global firmware IRQ selection a per-CPU SPI routing contract. The PMU
adapter still does not establish SPI-to-CPU assignment. No synthetic
preparation capability is used to publish an NMI action on an unsupported
controller.
Periodic NMI shutdown and global retirement reject mode `None` as unsupported, leaving ordinary
fallback ownership outside that protocol. Mode changes after registration
are not supported.

Periodic PMU startup holds one IRQ/preemption guard across counter setup,
callback registration, PPI preparation, producer start and any rollback. The
individual per-CPU access helpers alone cannot prevent migration between those
steps. This guard does not mask NMI or serialize remote management operations.

`arm_periodic_source` rejects existing ownership through shared access. Its sole
installation-time `PMU.get_mut_unchecked` follows successful disabled-PPI
preparation, which checks completed disable and no active controller claim.
No exclusive reference survives counter activation or PPI enable. The counter
becomes handler-ready while delivery remains masked, including stale-overflow
cleanup. Failed activation uses checked shutdown, not unconditional slot deletion.
The removed split configuration helpers no longer permit live callback/threshold
mutation with only ordinary IRQ exclusion. Shutdown skips the exclusive borrow
entirely when no counter is allocated.

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
The stop set covers all CPUs allowed to start, not just enabled/prepared slots.
This includes a start that has passed admission but not yet prepared, so action
retirement need not provide the source owner's start-versus-restore barrier.
SMP requires the boot-static present CPU prefix to have completed service setup;
UP targets only CPU 0. Transport validates readiness and retains Closing on any
failure rather than silently omitting a CPU. Claim participants are included
even if outside the expected prefix, so an unexpected obligation cannot vanish.
The global mutex is never acquired by NMI dispatch or target callbacks. The
retained claim and pending CPU set survive failed transport, teardown or final
action revocation checks. Only completed CPU obligations are removed from that set.
Restoration is rejected until shutdown has retired the old action; admission
reopens only after a new handler is published. No automatic counter restart or
watchdog-consumer reinitialization is implied.

The owner requires boot-static CPU services and exclusion from system stop and
CPU hotplug. Transport success proves callback/capture completion, not completion
of the surrounding IPI controller claim. It is the PMU stop and GIC teardown
inside the callback that establish source quiescence. CPU-local action revocation
follows the last local dispatch; finalization rejects any remaining publication.
Directory RCU independently protects the Line through original claim completion;
it cannot clear a pending interrupt or prevent a newly generated event. Both proofs are required
before reusing the registration. Normal-IRQ-only PMU builds reject global NMI
retirement as unsupported.

## PMU Mapping Preparation

Reusable periodic shutdown in `src/peripherals/pmu.rs` pins all local steps and
rejects NMI, unavailable mechanisms and other allocated counters. Stopping the
producer does not authorize mutable access to its storage: the GIC backend must
also confirm disable completion, reject active claims and clear pending state.
Only then may the unsafe `PMU.get_mut_unchecked` clear counter/callback slots.
IRQ/preemption exclusion spans this proof and mutation; it is not NMI exclusion.
Failure preserves storage for retry. The source
owner excludes concurrent restart/global handler removal and collects each
participant's completed operation; this API does not establish remote quiescence.

An absent cycle counter does not prove the local PPI is unprepared. The shutdown
path still requires controller teardown before reporting success on a CPU whose
line was prepared at boot but whose periodic source was never armed. All other
counter slots must remain unallocated under the same exclusive-source contract.
KIRQ owns the non-nesting local enable state, so shutdown retries need no
duplicate PMU state even before the lazy manager has been initialized. Every
retry stops production, masks the PPI and attempts teardown under the same
whole-transaction CPU/IRQ guard. Repeated shutdown cannot accumulate a hidden
disable depth. Source-owner transport completion supplies remote ordering;
local masking alone does not prove remote shutdown or local claim completion.

`ut-tests/peripherals_pmu_shutdown.rs` covers operation order and failure retry
without devices. Its exclusive-source integration case uses live PMU/GIC state
only when no counter is allocated, and skips rather than taking over a watchdog.
`ut-tests/pmu_lifecycle.rs` uses actual PMU overflow callbacks on each boot CPU,
real IPI execution and GIC teardown. An injected failed CPU step checks retained
ownership and retry without revisiting completed CPUs. Callback acknowledgements
drive progress, with external QEMU timeout only as a hang diagnostic. These tests
do not establish silicon-specific late-delivery or hardware-NMI correctness.

Unittest-only setup failure hooks reject progress but never bypass hardware
validation. A per-CPU fault mask is restored by `FailureScope` while its CPU/IRQ
pin remains held. Failed activation plus failed teardown must retain the stopped
counter and callback; subsequent arm cannot overwrite them. The Closing fixture
checks participation after preparation but before counter publication, using
explicit operation ordering rather than a scheduler timing threshold.
The mid-arm gate injection restores its previous value before releasing the CPU
pin. It does not run sleepable global shutdown from the pinned configuration
path. A separate fixture guard attempts checked source cleanup on early test
return and only restores delivery after that cleanup succeeds.

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


## PMU delivery preparation ownership

`peripherals::pmu::prepare_nmi_delivery` owns the GIC descriptor and rejects NMI
callers or unavailable mechanisms before entering KIRQ's per-CPU preparation
protocol. The mechanism interface exposes no IRQ-promotion hook. Existing KIRQ
admission, disabled/inactive PPI checks and retry obligations are unchanged;
source arming and activation still prepare before enabling delivery.
