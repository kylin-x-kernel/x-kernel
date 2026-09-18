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
| `src/arm_generic.rs` | ARM physical or virtual counter, generic timer events, device-tree configuration, and optional idle-return repair. |
| `src/riscv_sbi.rs` | RISC-V time counter and SBI timer events. |
| `src/x86_lapic_tsc.rs` | TSC frequency discovery and elapsed-time conversion, with LAPIC one-shot events. |

```text
platform boot hooks ---> backend configuration and initialization
                                      |
                         kiface provider registration
                                      |
runtime clients ------> khal::time ---+--> ClockSourceIf: ticks and conversion
                                      +--> ClockEventIf: local timer programming
                                                                  |
                                                        hardware timer IRQ
                                                                  |
                                                        kirq and its consumers
```

Each backend implements both interfaces through `kiface::provide`.
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
polls with local interrupts disabled. The platform then initializes the APIC and
calls `init_primary`; secondary CPUs initialize their own APIC before calling
`init_secondary`. `early_init` reports the selected frequency in whole kHz and
its discovery source through the boot console with `kernel_boot::bootln!`,
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
also depends on initialized CPU-local storage; its local state accesses must
remain on that CPU and avoid reentrant mutation. Boot initialization must not
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
wide division on the event path. The counter origin and resume offset are unchanged.
The backend writes the interval before enabling the selected timer. An expired
deadline uses interval 0; disarming clears the enable bit. Long x86 and ARM deadlines can
therefore generate an earlier interrupt, at which software must recheck and
rearm the remaining deadline.

RISC-V passes an absolute tick deadline to `sbi_rt::set_timer`. Disarming writes
`u64::MAX`. The backend does not inspect the SBI return value.

With `arm-timer-resume-fixup`, ARM maintains a shared counter offset and a per-CPU
last logical tick. An idle-return regression advances the shared offset,
rearms the local timer, and requests remote fixups through an IPI bitmask.
`handle_ipi_fixup` consumes the current CPU's pending bit and rearms its timer.
Other backends report no idle-return repair.

## Concurrency and lifetime

Atomic frequency, IRQ, mode, and origin fields hold boot configuration.
ARM and x86 conversion ratios use `klazy::Once`. The boot sequence establishes
when these values become usable; relaxed atomic accesses alone do not publish
the complete configuration as one transaction.

ARM resume repair uses compare-exchange on `TICK_RESUME_OFFSET` and atomic updates
to `IPI_FIXUP_PENDING`. `LAST_LOGICAL_TICKS` is CPU-local state rather than an
atomic shared counter. x86 LAPIC access goes through `x86_apic::with_local_apic`,
which masks local interrupts while borrowing the current CPU's APIC handle.

Each PIT sample owns a stack-local `PitChannel2` guard. Its destructor restores
the saved port `0x61` speaker and gate control byte on every normal return path,
including rejected samples, before the interrupt guard restores the preceding
interrupt state. The PIT mode and reload registers remain as programmed.
Shared conversion state lasts for the kernel lifetime; disarming a timer stops
its event generation without releasing controller resources.

## Validation

The x86 unit tests in `src/x86_lapic_tsc.rs` exercise frequency-source ordering,
lazy calibration and fallback, frequency calculations, and PIT sample-quality
rules through simulated readers. They do not exercise port I/O or APIC hardware.
Hardware validation must check both elapsed time against an independent clock
and deadline delivery on each CPU, including long and already expired deadlines.
