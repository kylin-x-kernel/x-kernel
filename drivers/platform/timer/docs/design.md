# timer-driver design

## Purpose and boundaries

`timer-driver` supplies the architecture-specific clock-source and clock-event
providers for `khal::time`. It discovers or accepts counter frequencies, converts
counter ticks to elapsed time, and programs per-CPU timer interrupts.

Platform boot code supplies configuration and orders shared and per-CPU
initialization. `khal::time` exposes the runtime interfaces; its callers own
software deadlines, timeout queues, and scheduling decisions. Interrupt-controller
bring-up and dispatch belong to `x86_apic`, `irq-driver`, and `kirq`.
This crate configures timer hardware after those controllers are ready.
Wall-clock maintenance belongs to `ktime`.

## Components and interactions

| Source | Responsibility |
| --- | --- |
| `src/lib.rs` | Target-specific module selection and configuration provenance through `TimerSource`. |
| `src/arm_generic.rs` | ARM physical or virtual counter, generic timer events, device-tree configuration, and optional counter-discontinuity repair. |
| `src/resume_counter.rs` | Private ARM counter state and test mounts. |
| `src/resume_notifications.rs` | Per-CPU atomic recovery requests and test mounts. |
| `src/riscv_sbi.rs` | RISC-V time counter and SBI timer events. |
| `src/x86_lapic_tsc.rs` | TSC frequency discovery and elapsed-time conversion, with LAPIC one-shot events. |

```text
platform boot hooks ---> backend configuration and initialization
                                      |
                         kiface provider registration
                                      |
runtime clients ------> khal::time ---+--> CounterSourceIf: ticks and conversion
                                      +--> ClockEventIf: local timer programming
                                                                  |
                                                        hardware timer IRQ
                                                                  |
                                                        kirq and its consumers
```

Each backend implements CounterSourceIf and ClockEventIf through `kiface::provide`.
HAL supplies the logical ClockSourceIf, including suspend-time mapping.
Clock-source frequency describes the counter used for timekeeping. Clock events
use the selected timer's own programming model; x86 uses separate TSC and LAPIC
rates. Runtime clients use the HAL while platform code calls the backend's public
configuration and initialization entries.

## Initialization and execution context

Shared clock state is initialized once during bootstrap, before concurrent time
readers and secondary-CPU timer initialization. Initialization requires access
to the selected counter and firmware or hardware interfaces, but has no
current-process or scheduler dependency.

On x86, `early_init` runs on the bootstrap CPU and establishes the frequency,
conversion ratios, and TSC origin. PIT calibration requires exclusive channel 2
ownership before secondary CPUs and other drivers can use its ports. Each sample
polls with local interrupts disabled. The platform then initializes each CPU's
APIC before calling the common `init_percpu`, which configures and unmasks that
CPU's timer without repeating calibration. CPU IRQ delivery remains masked
until runtime installs the timer handler. `early_init` reports the selected
frequency in whole kHz and its discovery source through the boot console with
`kernel_boot::bootln!`,
because the runtime logger is not installed during early driver initialization;
a platform-static selection is reported as a discovery failure.

On ARM, `config_from_device_tree` reads firmware configuration and `init`
establishes shared state. `init_percpu` enables each CPU's timer and IRQ after
local interrupt-controller initialization. The physical timer uses the physical
counter directly; the virtual timer subtracts the counter value captured at
initialization. The `vmm` feature selects EL2 physical-timer writes and assumes
that the platform has established the required EL2 execution environment.

On RISC-V, `init` installs the platform timebase and IRQ. `init_percpu` requests
an immediately expired SBI event on the calling hart. The platform supplies
supervisor access to the counter and SBI timer service and arranges IRQ delivery.

Initialized clock reads and event programming are non-sleeping and can run in
interrupt context. Event operations act on the current CPU. ARM resume repair
serializes shared counter samples and keeps timer-register programming on the
detecting CPU. Boot initialization must not
race runtime operations or be repeated with a different configuration.

## Frequency and time conversion

### x86

Frequency discovery tries the following sources in order:

| Environment | Source order |
| --- | --- |
| QEMU TCG identification | PIT measurement, hypervisor CPUID, Intel CPUID, platform fallback. |
| Other environments | Hypervisor CPUID, Intel CPUID, PIT measurement, platform fallback. |

The hypervisor path accepts timing leaf `0x40000010` only for KVM and ACRN.
The Intel path requires a valid numerator and denominator in leaf `0x15`.
It uses the rate derived from the reported crystal frequency and ratio when that
rate is in range; otherwise it uses the nonzero leaf `0x16` processor base
frequency as an estimate of TSC frequency. Each source is evaluated only when
earlier sources fail.

PIT channel 2 runs a mode-0 countdown of about 10 ms. Calibration takes three
samples and selects the lowest valid frequency to reduce the effect of polling
delays. Each sample has a one-million-iteration limit and a TSC tick budget of
one fifth of the nominal platform frequency. This tick budget is a time bound
only to the extent that the nominal frequency matches the running TSC.

Normal samples require at least 1,000 polls, nonzero adjacent TSC progress, and a
maximum polling interval no larger than ten times the minimum. QEMU TCG samples
require at least one poll and a nonzero minimum interval, allowing the wider
polling jitter caused by device emulation and host scheduling. An immediately
completed or timed-out sample is discarded. The measurement uses the last TSC
read before the observed PIT completion, so polling granularity also affects
its accuracy.

Runtime candidates must be at least 1 MHz and fit in `u32` after conversion to
kHz. Bootstrap separately checks the platform minimum and the selected value's
representability. The selected frequency is stored in Hz; conversion ratios use
whole kHz to fit `int_ratio::Ratio` parameters. Counter reads subtract the TSC
origin captured at the end of `early_init`.

### ARM and RISC-V

ARM accepts an optional frequency override and otherwise reads `CNTFRQ_EL0`.
The selected frequency must be nonzero and fit in `u32` Hz. Two immutable ratios
convert between counter ticks and nanoseconds.

RISC-V uses an integer scale of `1_000_000_000 / frequency_hz` nanoseconds per
tick. A frequency that does not divide 1 GHz loses fractional precision, and a
frequency above 1 GHz produces a zero scale. The platform must choose a usable
timebase; `init` checks only that the frequency and IRQ are nonzero.

## Clock events and idle repair

x86 programs a divide-by-one LAPIC one-shot timer using a fixed 1 GHz event rate.
A future deadline becomes a relative count clamped to `1..=u32::MAX`; an expired
deadline uses count 1. Writing count 0 disarms the timer. TSC calibration does
not calibrate the LAPIC event rate.

ARM samples logical counter time in nanoseconds, subtracts it from the software
deadline with saturation, then converts the remaining duration to ticks rounded
up and clamped to `i32::MAX`. Multiplication saturates before division: since
`i32::MAX * 1_000_000_000` fits in `u64`, an overflowing product already requires
the maximum countdown. This avoids absolute-deadline conversion wrap without
wide division on the event path. The logical counter accounting is unchanged.
The backend writes the interval before enabling the selected timer. An expired
deadline uses interval 0; disarming clears the enable bit. Long x86 and ARM deadlines can
therefore generate an earlier interrupt, at which software must recheck and
rearm the remaining deadline.

RISC-V passes an absolute tick deadline to `sbi_rt::set_timer`. Disarming writes
`u64::MAX`. The backend does not inspect the SBI return value.

With `arm-timer-resume-fixup`, every ARM clock read samples a shared counter
state under an IRQ-safe spinlock. The register is read after an ISB while holding
the lock, so sampling and baseline updates are ordered across CPUs. A wrapping
delta of at least half the 64-bit range is a backward discontinuity, including
a negative virtual-counter value represented as a large unsigned number. The
logical clock stays at its last confirmed value and subsequent samples advance
from the new raw baseline. Natural 64-bit rollover with a small forward delta
is accepted. This assumes synchronized CPU counters and forward intervals below
half the counter range. It cannot identify arbitrary plausible forward jumps
or reconstruct elapsed sleep time without an independent trusted clock.

A repair rearms only the detecting CPU's timer registers after releasing the
state lock. Remote IPI delivery and diagnostics are deferred to the timer handler
so clock reads under IRQ-controller or console locks do not reacquire them. The
IRQ line remains enabled from initialization. The idle-return callback samples
the same path; detection does not depend on reaching idle first. If no CPU
executes a clock read after host resume, the guest cannot force execution or
repair a stalled host. The feature is restricted by Kconfig to non-NMI guests;
an NMI could otherwise reenter a clock read while the interrupted CPU holds the
counter lock. Other backends report no idle repair.

## Concurrency and lifetime

Atomic frequency, IRQ, mode, and origin fields hold boot configuration.
ARM and x86 conversion ratios use `klazy::Once`. The boot sequence establishes
when these values become usable; relaxed atomic accesses alone do not publish
the complete configuration as one transaction.

ARM raw sampling and logical tick accounting use one `SpinNoIrq` critical
section, accessed from task and maskable IRQ contexts. It contains a register
read and bounded arithmetic; no logging, IRQ-controller access or IPI delivery
occurs under that lock. An outer no-preemption/IRQ guard keeps post-lock timer
writes on the detecting CPU. `IPI_FIXUP_PENDING` independently tracks remote
notifications; `REMOTE_FIXUP_REQUESTED` publishes deferred notification work. The first detector's report is retained in counter state until dispatch; further detections
before dispatch coalesce. Report consumption and flag clearing hold the same
lock as detection publication. Dispatch rearms its own timer as well as notifying
peers, even when the detecting CPU differs. It never resamples to construct
diagnostics.
x86 LAPIC access goes through `x86_apic::with_local_apic`,
which masks local interrupts while borrowing the current CPU's APIC handle.

Each PIT sample owns a stack-local `PitChannel2` guard. Its destructor restores
the saved port `0x61` speaker and gate control byte on every normal return path,
including rejected samples, before the interrupt guard restores the preceding
interrupt state. The PIT mode and reload registers remain as programmed.
Shared conversion state lasts for the kernel lifetime; disarming a timer stops
its event generation without releasing controller resources.

## Validation

Guest tests live in crate-root `ut-tests/`, mounted by
`unittest::path_mod_test!` as private `tests` children of their original parents.
Names, parent access, registration and inherited architecture/feature gates
remain unchanged. There are no serial or user flags.

| File under `ut-tests/` | Cases | Gate beyond `cfg(unittest)` |
| --- | --- | --- |
| `lib.rs` | 1 | None |
| `arm_generic.rs` | 4 | AArch64 |
| `resume_notifications.rs` | 6 | AArch64; available even without resume fixup |
| `resume_counter.rs` | 11 | AArch64 and `arm-timer-resume-fixup` |
| `x86_lapic_tsc.rs` | 4 | x86_64 |

Across all gates there are 26 cases: AArch64 selects 11 by default or 22 with
`KFEAT_ARM_TIMER_RESUME_FIXUP=y`; x86_64 selects 5 and RISC-V64 selects 1.
Resume fixup depends on `!KFEAT_NMI`; disable NMI before enabling this option
and verify the resolved `.config` after `make olddefconfig`.
There is no LoongArch backend or dependency from `kplat-loongarch64`; if the
crate is linked on that target, only the single common case is available.
`vmm` changes production timer selection but adds no test-selection gate.
Use the configured kernel guest unittest workflow (`UNITTEST=y`).

The resume helpers have one set of cases in `ut-tests/resume_counter.rs`
(11 cases) and `ut-tests/resume_notifications.rs` (6 cases). These kernel UTs
retain all assertions from the former host copies; there is no separate host
suite or runner to maintain. With AArch64 resume fixup enabled and NMI disabled,
`make unittest UNITTEST_CRATE=timer_driver` runs all 22 selected cases, including
these 17 resume cases.

The x86 unit tests in `ut-tests/x86_lapic_tsc.rs` exercise frequency-source ordering,
lazy calibration and fallback, frequency calculations, and PIT sample-quality
rules through simulated readers. They do not exercise port I/O or APIC hardware.
Hardware validation must check both elapsed time against an independent clock
and deadline delivery on each CPU, including long and already expired deadlines.

Tests in `ut-tests/resume_counter.rs` cover the captured unsigned underflow, ordinary
and repeated resets, serialized CPU samples, 64-bit rollover and the ambiguous
half-range boundary. Arithmetic tests do not replace hardware sleep validation.

The shared lock intentionally serializes the read and baseline update: a
per-CPU last value cannot atomically distinguish a global counter reset from a
repair already performed elsewhere. This opt-in correctness path is not the
default clock path. `static_lock!` registers its acquisitions and contention
when lock statistics are enabled. Cross-CPU contention can lengthen masked IRQ
intervals; high-core-count performance is not established by the four-vCPU tests.

The idle-return hook still samples the clock; its boolean only describes repair
by that call. A reset may already have been repaired by another read. The task
idle loop no longer compares repaired logical samples as a raw-reset diagnostic.
Repeated detections before dispatch coalesce; repeated real discontinuities
after dispatch still require new recovery. An unsynchronized or continuously
oscillating counter violates the clock contract and can cause repeated local
IRQs/IPIs; the guest cannot guarantee service in that environment.

## Per-CPU resume notifications

The optional ARM64 resume path stores one atomic pending flag per configured
logical CPU instead of a machine-word bitmap. The dispatcher marks every active
peer before sending AllButSelf IPI; each peer consumes only its own flag with an
atomic swap. Repeated requests may coalesce before consumption; a request
published after consumption remains pending. Topology must fit `NR_CPUS` and use
contiguous logical CPU IDs. Invalid topology produces no notification or flag
mutation. This change does not add RTC compensation or modify clock semantics.
The exceptional notification path scans active CPUs; normal clock reads do not.

Notification state regressions run as kernel unit tests through the existing
AArch64 unittest pipeline, including when resume fixup is disabled. They cover
130 logical flags, topology validation, coalescing, independent consumption and
publication after consumption using deterministic sequences. These state tests
do not claim simultaneous SMP execution or actual 130-CPU IPI routing coverage.
No feature-specific Jenkins stage or standalone host CI entry is added.

### AArch64 logical CPU topology contract

The notification range is justified by the current AArch64 CPU-map loader,
not by assuming hardware IDs are dense. `arch/kcpu_id_map/src/arch/aarch64.rs`
loads through `CpuIdMap::load_from_fdt` in `arch/kcpu_id_map/src/cpu_id.rs`:
`of::enabled_cpu_nodes` filters disabled nodes, `filter_map(of::cpu_node_reg)`
removes nodes without valid register IDs, and only then
`load_from_raw_cpu_ids` uses `enumerate()` to insert logical indices starting
at zero, truncating at `NR_CPUS`. The only `CpuIdMap::insert` caller is this
loader. Normalized MPIDRs are masked by `MPIDR_AFFINITY_MASK` and cannot equal
the `usize::MAX` invalid sentinel. Thus no holes are introduced by disabled
nodes or sparse hardware MPIDRs, and present logical IDs are exactly
`0..nr_cpus()` once routing is initialized. The immutable `Once<CpuIdMap>`
has no runtime removal/hotplug path. If this mapping contract changes to permit
sparse logical IDs, the notification publisher must use explicit present IDs
rather than a present count; the helper does not independently validate holes.

## Explicit monotonic suspend mapping

Backends now provide `CounterSourceIf`, while khal provides the logical
`ClockSourceIf` over the suspend mapping. Counter frequency and duration
conversions stay in the backend. `resume_ticks` establishes a backend anchor
without enabling timer events or dispatching anomaly recovery. ARM with fixup
uses `ResumeCounter::resume` to discard suspended raw progress and pending repair
reports, remote dispatch requests and all peer notification flags. Formal resume
does not trigger fixup logging or IPIs. PM must quiesce publishers and consumers,
including in-flight dispatches and IPI handlers, before freezing the clock.
Without fixup, ordinary origin subtraction remains saturating, preserving the
pre-existing behavior for unexpected counter regression outside formal suspend.
Only resume_ticks changes TIMER_INIT_TICKS to the restored raw count and returns
zero as the new backend anchor. Subsequent reads advance from that origin; the
HAL separately preserves the pre-suspend logical boundary. This also covers a
restored counter below the original boot origin without publishing a wrapped
value. Unexpected runtime resets are not repaired by this non-fixup path.

`ClockEventIf::arm_timer_after` accepts an interval, not an absolute logical
deadline. HAL owns deadline subtraction and suppresses programming while frozen.
ARM uses bounded signed TVAL; x86 uses bounded LAPIC countdown; RISC-V adds the
interval to its hardware counter before SBI programming. Platform PM must
restore controller/local-event configuration and re-arbitrate every CPU's
software deadlines after time recovery. This change supplies no device PM or
host/guest protocol and does not enable sleep compensation.

## Local clock-event restoration

Each event backend implements ClockEventIf::restore_local_timer independently
of clock-source origin/calibration. PM restores controller access and vector
routing, masks local IRQs and quiesces programming/repair before calling it.
ARM stops CNTP/CNTV and restores IRQ delivery without the boot path's zero-count
arm. x86 stops the retained LAPIC countdown, restores one-shot mode and Div1 via
existing per-CPU initialization, then ensures the count is zero; LAPIC vector
routing is a platform prerequisite. RISC-V writes the disabled SBI comparator
value; hart interrupt enables remain a platform duty. No backend restarts the
source, emits callbacks or restores a cached deadline. The task timer service
keeps programming inhibited until PM finishes timekeeping and sleep accounting.
