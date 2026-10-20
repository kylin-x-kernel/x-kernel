# timer-driver security and reliability

## Scope and trust model

This analysis covers the entire crate: `src/lib.rs`, `src/arm_generic.rs`,
`src/riscv_sbi.rs`, `src/x86_lapic_tsc.rs`, `src/resume_counter.rs`, and
`src/resume_notifications.rs`, plus guest fixtures in `ut-tests/`, including the ARM
`arm-timer-resume-fixup` and `vmm` paths.

Extracted tests keep private parent access and the existing selection gates.
The two resume suites run only in the kernel harness. Removing the duplicate
host suites preserves their assertions in the guest suites and avoids divergent
copies. Outer guest cfg gates avoid runtime macro references in non-UT builds.
Layout changes neither widen production APIs nor change hardware
access, state transitions, or existing lint exceptions.

Platform boot code owns privilege level, CPU-local storage readiness, and
initialization order. `of` supplies the parsed device tree. `x86_apic` owns APIC
mapping, per-CPU handles, and guarded register access; `irq-driver` and `kirq`
own interrupt-controller setup and delivery. SBI firmware owns the RISC-V timer
service. These external implementations are outside this crate's analysis;
their contracts are prerequisites for the operations described here.

The protected resources are PIT channel 2 and its control ports, the current
CPU's timer registers and LAPIC handle, shared conversion state, and ARM's
shared logical-counter state. Hardware and firmware metadata are trusted to
describe real devices after the local checks below. The crate has no caller
identity or permission checks; access is through privileged kernel code.

## External inputs and validation

| Entry and input direction | Local checks and failure behavior |
| --- | --- |
| Platform configuration to x86 `early_init` | Requires a nominal frequency of at least 1 MHz. The selected frequency must fit in `u32` kHz. Violations panic. The nominal value also supplies the PIT timeout budget. |
| CPU or hypervisor CPUID to x86 discovery | Hypervisor leaf `0x40000010` is interpreted only for KVM and ACRN. Architectural timing leaves require Intel identification and a nonzero leaf `0x15` ratio. Invalid or absent candidates return `None`; accepted candidates are at least 1 MHz and fit in `u32` kHz. |
| PIT ports `0x42`, `0x43`, and `0x61` to x86 calibration | A fixed nonzero reload starts channel 2. Poll count, TSC progress, jitter policy, and timeout bounds determine whether the sample is usable. Invalid samples return `None`; discovery continues with the remaining sources. |
| Device tree through ARM `config_from_device_tree` | Looks for `arm,armv8-timer` or `arm,armv7-timer`. Reads `interrupts`, `interrupt-names`, and `clock-frequency`. Missing node or usable interrupt returns `None`; the current AArch64 platform boot hook requires a result with `expect`. |
| ARM interrupt properties to `timer_irq_from_device_tree` | Parses complete 12-byte entries, accepts type 0 or 1, and adds the corresponding SPI or PPI base. Names use valid UTF-8 entries, with positional fallback. The parser does not validate the resulting ID against controller capacity. |
| ARM `clock-frequency` to `timer_frequency_from_device_tree` | Accepts a 4- or 8-byte big-endian value. Missing or malformed properties yield `None`, so `init` reads `CNTFRQ_EL0`. `init` rejects zero IRQs, zero frequency, and frequency above `u32::MAX` Hz. |
| Platform timebase to RISC-V `init` | Rejects zero IRQ or frequency. It does not reject a zero nanoseconds-per-tick scale caused by frequency above 1 GHz. |
| Counter registers, LAPIC, ARM timer registers, and SBI | Register accessibility, timer availability, and interrupt routing are platform prerequisites. The RISC-V backend discards SBI return values. |

There are no user pointers, DMA buffers, file contents, network payloads, or
FFI entry points in this crate. Device-tree data and hardware observations are
its external data boundaries.

## Unsafe inventory

| Source and operation | Preconditions and enforcing path |
| --- | --- |
| `src/x86_lapic_tsc.rs::read_tsc_raw` calls `_rdtsc` | Compiled only for supported x86_64 targets, where the instruction is available. It accesses counter state without dereferencing memory. All local TSC reads use this helper. |
| `src/x86_lapic_tsc.rs::PitChannel2::{acquire,start_one_shot,has_elapsed}` and `Drop` perform port I/O | Bootstrap executes at CPL0 with exclusive ownership of channel 2. `start_one_shot` asserts a nonzero reload and writes both bytes. The platform calls `early_init` before secondary CPUs and other port users; calibration masks local interrupts around each sample. The guard restores the saved control byte before that interval ends. |
| `src/x86_lapic_tsc.rs::{init_primary,init_secondary,arm_timer,disarm_timer}` program LAPIC registers | The calling CPU's APIC must already be initialized. `x86_apic::with_local_apic` checks the local handle and masks local interrupts during its mutable borrow. Boot hooks order APIC initialization before timer setup; runtime calls depend on that completed setup. |
| `src/arm_generic.rs::write_physical_timer_tval`, with `vmm`, executes `msr CNTHP_TVAL_EL2` | The host runs at EL2 with VHE as required by the physical-timer helpers. The assembly takes one `in(reg)` integer, writes the timer register, and has no pointer operand or Rust output. Initialization passes zero; deadline programming bounds positive intervals to `i32::MAX`, within the register's signed 32-bit range. |

The RISC-V backend contains no explicit unsafe block; privileged counter and SBI
operations are encapsulated by its dependencies. The crate declares no custom
unsafe `Send` or `Sync` implementation.

## Invariants and concurrency

Boot initialization must complete before runtime clock conversion. ARM and x86
ratios are initialized once; repeating initialization can update atomic fields
without replacing those ratios. Shared configuration must therefore remain
stable after boot. Atomic scalar accesses do not replace boot-time ordering.

PIT ownership is global across CPUs. Local interrupt masking protects one sample
from local IRQ handlers, while the boot sequence prevents other CPUs and drivers
from reprogramming channel 2. Every normal sample return releases the local guard.

LAPIC handles belong to the current CPU and use an IRQ-masked borrow supplied
by `x86_apic`. ARM samples and state updates share an IRQ-safe lock; remote
repair requests use independent per-CPU atomic pending flags.

## Threat analysis

| ID | Threat and trigger | Impact | Current controls and residual risk |
| --- | --- | --- | --- |
| T-01 | CPU or hypervisor reports a plausible but incorrect TSC rate. | Medium: elapsed time and deadlines are distorted. | Vendor and numeric checks reject unsupported encodings; TCG prefers measurement. An in-range enumerated value is trusted without independent cross-checking. |
| T-02 | PIT is absent, stuck, or delayed by emulation or host scheduling. | Medium: calibration fails or produces inaccurate time. | Poll and TSC budgets bound sampling; progress and jitter checks reject unsuitable samples, and three attempts retain the minimum valid rate. TCG's relaxed checks retain measurement error risk. |
| T-03 | Another CPU or driver reprograms PIT channel 2 during calibration. | Medium: corrupted samples or disrupted device use. | Bootstrap-only ownership and local IRQ masking prevent the expected competitors. Violating boot ownership remains unsupported. |
| T-04 | Firmware supplies extreme frequency values or unusable ARM interrupt data. | Medium: boot panic, incorrect time, or missing timer delivery. | Checked frequency scaling, x86 range filters, ARM property-length checks, and initialization assertions limit accepted data. ARM interrupt capacity and the RISC-V conversion scale remain platform responsibilities. |
| T-05 | TSC rate changes or CPU counters are unsynchronized. | Medium: drift or cross-CPU time regression. | The backend relies on a stable, system-consistent TSC. It provides no runtime recalibration or synchronization repair. |
| T-06 | Timer operations run before local setup or ARM counter samples race. | High: invalid hardware access or inconsistent logical time. | Platform boot order establishes local state; APIC access guards its borrow. ARM sampling and updates share an IRQ-safe lock. |
| T-07 | Timer register or SBI service behavior differs from platform assumptions. | Medium: timer interrupts arrive late, early, or stop. | ARM and x86 clamp relative counts to their register ranges. x86 still assumes a fixed LAPIC rate, and the RISC-V backend does not report SBI failures. |

## Failure modes

| ID | Failure and cause | Local effect | System effect | Severity | Handling |
| --- | --- | --- | --- | --- | --- |
| F-01 | All runtime TSC sources are unavailable or rejected. | Platform frequency is selected. | Clock accuracy depends on the configured fallback. | 2 | Initialization checks the selected representation; discovery represents the source as `TscFrequency::PlatformStatic`. |
| F-02 | PIT completes without usable progress or exceeds a budget. | Sample returns `None`. | Calibration falls back to other samples or sources. | 3 | Fixed sample count and discovery order preserve a bounded fallback path. |
| F-03 | Required configuration or conversion state is invalid or missing. | Assertions or ratio lookups panic. | Boot or the calling kernel path stops. | 2 | Platform initialization must establish the documented prerequisites. |
| F-04 | LAPIC rate differs from the fixed event rate. | Counts represent the wrong duration. | Deadline delivery is early or late. | 2 | Software can recheck early deadlines; late delivery remains a platform timing limitation. |
| F-05 | RISC-V timebase produces a zero or truncated scale. | Conversion divides by zero or loses precision. | Panic or systematic time error. | 2 | Platform configuration must provide a usable timebase; current initialization checks only nonzero frequency. |
| F-06 | ARM timer node or usable IRQ is absent. | Discovery returns `None`. | The current platform boot hook panics. | 2 | Correct firmware timer data is required; malformed frequency data alone falls back to `CNTFRQ_EL0`. |

## Failure handling and resource recovery

ARM event programming converts the remaining logical duration, not the absolute
deadline. Saturating multiplication, upward rounding, and the signed TVAL limit
prevent a far-future request from wrapping into an immediate interrupt loop.
This does not extend the clock source's valid lifetime across counter or
nanosecond wrap; software must still recheck deadlines after bounded interrupts.

Frequency discovery degrades through optional sources; invalid PIT samples
return `None`. Configuration and initialization violations use assertions or
`expect`, without a recovery protocol. `PitChannel2::drop` restores port `0x61`
on normal returns, including rejected samples. It does not restore the previous
PIT mode or reload value, and panic-abort or hardware failure provides no stack
cleanup guarantee.

Timer disarming stops the programmed event without freeing APIC or firmware
resources. Shared clock configuration has kernel lifetime.

## Privacy and limitations

The driver handles hardware metadata and numeric time values. It receives no
user payload and persists no personal data. ARM resume diagnostics record CPU
identifiers, raw counter values, and logical tick values.

Accuracy depends on the chosen frequency and backend conversion precision.
x86 ratios truncate the frequency to kHz, Intel base frequency is a fallback
estimate, and accepted CPUID values remain trusted. The PIT tick budget depends
on the nominal TSC rate; the iteration cap independently limits polling.
The LAPIC event rate is fixed. RISC-V integer scaling and ignored SBI results
are additional limits. ARM resume repair preserves logical progression under
its execution assumptions; it does not reconstruct elapsed suspend time from
an independent clock.

## Audit and verification

- `tsc_frequency_discovery_respects_sources_and_fallback` checks source order,
  supported hypervisor identification, lazy calibration, and fallback selection.
- `tsc_frequency_calculations_preserve_reference_units` checks rate conversion
  and rejected frequency inputs; `pit_samples_require_progress_and_reject_hardware_jitter`
  checks the normal and TCG sample policies. These tests use no real hardware.
- Inspect the assertions in each backend's initialization and the ARM property
  parsers when changing accepted firmware inputs.
- Check every PIT early return against the guard lifetime, the independent
  polling budgets, and exclusive bootstrap ownership.
- Trace platform APIC initialization before timer setup, and check that each
  unsafe operation retains its privilege, CPU-locality, and serialization contract.
- Validate real counter progression and timer delivery against an independent
  clock; the simulated frequency tests cannot establish hardware accuracy.

ARM counter discontinuity handling requires one synchronized counter domain and
forward intervals below half the 64-bit range. Backward intervals establish a
new baseline without advancing logical time. Plausible forward jumps cannot be
distinguished from elapsed time, and lost sleep time is not reconstructed. This
is a guest mitigation, not a host counter-integrity or sleep-accounting guarantee.
Clock-read repairs only rearm local registers; remote notification and logs run
from the timer handler after caller locks have been released. Kconfig excludes
NMI guests because an IRQ-safe lock does not protect against NMI reentrancy.

Deferred notification publication, diagnostic snapshots and consumption are
serialized by the counter lock. Pending events coalesce and each consumed
report is dispatched once; a later reset remains a new event. Both dispatching
CPU and peers are rearmed. The registered lock remains a possible IRQ-latency
bottleneck under contention. Tiny backward samples also count as resets: no
arbitrary threshold discards a real reset. A continuously oscillating or
unsynchronized source violates the assumed clock domain and can cause repeated
recovery traffic; this feature does not rate-limit or repair such a host.

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
