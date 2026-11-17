// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86 monotonic clock source and local APIC clock-event provider.
//!
//! The driver discovers the TSC frequency during bootstrap, publishes TSC-based
//! monotonic time, and programs the per-CPU LAPIC timer for one-shot deadlines.
//!
//! Platform boot code calls [`early_init`] once to establish clock-source state,
//! then each CPU calls [`init_percpu`] after its own LAPIC initialization. Runtime users
//! access the providers through `khal::time`.
//!
//! [`TimerConfig`] supplies the nominal frequency used for the calibration
//! timeout and as the final fallback. TSC discovery does not calibrate the
//! separate LAPIC event rate.

use core::sync::atomic::{AtomicU64, Ordering};

use int_ratio::Ratio;
use klazy::Once;
use ktime_types::Frequency;
use raw_cpuid::{CpuId, CpuIdReader, Hypervisor};
use x86_64::instructions::port::{Port, PortWriteOnly};

use crate::TimerSource;

const LAPIC_TICKS_PER_SEC: u64 = 1_000_000_000;
// The PC-compatible PIT provides the reference interval for TSC calibration.
const PIT_TICK_RATE: Frequency = Frequency::from_hz(1_193_182);
// A roughly 50 ms countdown keeps poll overhead and host preemption a small
// fraction of the reference interval.
const PIT_CALIBRATION_TICKS: u16 = (PIT_TICK_RATE.as_hz() / 20) as u16;
const PIT_CALIBRATION_SAMPLES: usize = 8;
// A sample whose sound interval is this narrow is treated as trustworthy: the
// midpoint is then within half the spread of the real frequency.
const PIT_SAMPLE_SPREAD_PERCENT: u128 = 2;
// When no sample is trustworthy, the tightest bound is reported as degraded
// only while its uncertainty stays inside this ceiling.
const PIT_DEGRADED_SPREAD_PERCENT: u128 = 25;
// The TSC budget for programming the countdown: at most 1% of the countdown
// expected at the nominal rate. The reload lies inside that window, so the
// midpoint keeps the residual interval shift within half a percent.
const PIT_START_GAP_DIVISOR: u128 = 100;
// A PIT that never fires (or a vCPU descheduled this long) abandons discovery
// instead of spending the whole sample budget.
const PIT_MAX_SAMPLE_TIMEOUTS: usize = 2;
const PIT_MAX_POLL_ITERATIONS: usize = 1_000_000;
const MIN_TSC_FREQUENCY: Frequency = Frequency::from_hz(1_000_000);
static INIT_TICK: AtomicU64 = AtomicU64::new(0);
static TSC_FREQUENCY_HZ: AtomicU64 = AtomicU64::new(0);
static TSC_TICKS_TO_NANOS_RATIO: Once<Ratio> = Once::new();
static NANOS_TO_TSC_TICKS_RATIO: Once<Ratio> = Once::new();
static NANOS_TO_LAPIC_TICKS_RATIO: Once<Ratio> = Once::initialized(Ratio::new(
    LAPIC_TICKS_PER_SEC as u32,
    ktime_types::NANOS_PER_SEC as u32,
));

#[kiface::provide]
impl khal::time::CounterSourceIf {
    fn now_ticks() -> khal::time::TimerTicks {
        now_ticks()
    }

    fn resume_ticks() -> khal::time::TimerTicks {
        now_ticks()
    }

    fn ticks_to_span(ticks: khal::time::TimerTicks) -> ktime_types::TimeSpan {
        ktime_types::TimeSpan::from_nanos(ticks_to_nanos(ticks.as_raw()))
    }

    fn frequency() -> Frequency {
        frequency()
    }

    fn span_to_ticks(span: ktime_types::TimeSpan) -> khal::time::TimerTicks {
        khal::time::TimerTicks::from_raw(nanos_to_ticks(span.as_nanos_u64_saturating()))
    }
}

#[kiface::provide]
impl khal::time::ClockEventIf {
    fn interrupt_id() -> usize {
        interrupt_id()
    }

    fn arm_timer_after(interval: ktime_types::TimeSpan) {
        arm_timer_after(interval)
    }

    fn restore_local_timer() {
        // Stop any retained countdown before restoring one-shot mode/divisor.
        // The platform has already restored LAPIC access and vector routing.
        disarm_timer();
        init_percpu();
        disarm_timer();
    }

    fn disarm_timer() {
        disarm_timer()
    }
}

#[kiface::provide]
impl kiface_time_api::ClockResumeIf {
    fn handle_idle_return(_previous_ticks: khal::time::TimerTicks) -> bool {
        false
    }
}

/// Platform-provided fallback configuration for the x86 timer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimerConfig {
    /// Nominal TSC frequency used for the calibration timeout and final fallback.
    pub nominal_frequency: Frequency,
    /// Configuration provenance; frequency discovery does not inspect this field.
    pub source: TimerSource,
}

impl TimerConfig {
    /// Creates a configuration backed by a platform-static fallback frequency.
    ///
    /// Frequency validation occurs in [`early_init`].
    pub const fn platform_static(nominal_frequency: Frequency) -> Self {
        Self {
            nominal_frequency,
            source: TimerSource::PlatformStatic,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TscFrequency {
    Pit(Frequency),
    /// Bounded PIT sample whose uncertainty exceeded the trustworthy limit.
    PitDegraded(Frequency),
    Hypervisor(Frequency),
    Cpuid(Frequency),
    PlatformStatic(Frequency),
}

impl TscFrequency {
    const fn source_name(self) -> &'static str {
        match self {
            Self::Pit(_) => "PIT calibration",
            Self::PitDegraded(_) => "PIT calibration (degraded)",
            Self::Hypervisor(_) => "hypervisor CPUID",
            Self::Cpuid(_) => "CPUID",
            Self::PlatformStatic(_) => "platform static fallback",
        }
    }

    const fn frequency(self) -> Frequency {
        match self {
            Self::Pit(frequency)
            | Self::PitDegraded(frequency)
            | Self::Hypervisor(frequency)
            | Self::Cpuid(frequency)
            | Self::PlatformStatic(frequency) => frequency,
        }
    }
}

// PC/AT PIT channel 2 and system-control I/O ports.
const PIT_CHANNEL2_DATA_PORT: u16 = 0x42;
const PIT_COMMAND_PORT: u16 = 0x43;
const SYSTEM_CONTROL_PORT: u16 = 0x61;
// Port 0x61 bits: timer 2 gate and speaker data enable (RW), timer 2 output (RO).
const SYSTEM_CONTROL_TIMER2_GATE: u8 = 0x01;
const SYSTEM_CONTROL_SPEAKER_DATA_ENABLE: u8 = 0x02;
const SYSTEM_CONTROL_TIMER2_OUT: u8 = 0x20;
// PIT command byte: channel 2 (bits 7:6 = 10), LSB/MSB access (bits 5:4 = 11),
// mode 0 (bits 3:1 = 000), binary countdown (bit 0 = 0).
const PIT_CMD_CHANNEL2_MODE0_LSB_MSB: u8 = 0b1011_0000;

/// Wraps the x86 I/O ports used to configure and read PIT channel 2
/// during TSC frequency calibration.
struct PitChannel2 {
    system_control: Port<u8>,
    command: PortWriteOnly<u8>,
    channel_data: PortWriteOnly<u8>,
    original_system_control: u8,
}

impl PitChannel2 {
    /// Saves the control state while bootstrap owns PIT channel 2.
    fn acquire() -> Self {
        let mut system_control = Port::new(SYSTEM_CONTROL_PORT);
        // SAFETY: bootstrap runs at CPL0 on the BSP and owns legacy PIT channel 2.
        // Port 0x61 is the PC system-control register associated with this channel.
        let original_system_control = unsafe { system_control.read() };
        Self {
            system_control,
            command: PortWriteOnly::new(PIT_COMMAND_PORT),
            channel_data: PortWriteOnly::new(PIT_CHANNEL2_DATA_PORT),
            original_system_control,
        }
    }

    /// Starts a PIT channel 2 mode-0 countdown with the given nonzero reload.
    fn start_one_shot(&mut self, ticks: u16) {
        assert_ne!(ticks, 0, "PIT one-shot reload must be nonzero");
        let gated_system_control = (self.original_system_control
            & !SYSTEM_CONTROL_SPEAKER_DATA_ENABLE)
            | SYSTEM_CONTROL_TIMER2_GATE;
        // SAFETY: The bootstrap guard exclusively owns PIT channel 2 and its mapped PC control port
        // at CPL0.
        unsafe { self.system_control.write(gated_system_control) };
        // SAFETY: The guard owns the PIT command port; select channel 2 binary mode 0 with LSB/MSB
        // writes.
        unsafe { self.command.write(PIT_CMD_CHANNEL2_MODE0_LSB_MSB) };
        // SAFETY: The preceding command selected channel 2 reload LSB; the guard excludes competing
        // PIT access.
        unsafe { self.channel_data.write(ticks as u8) };
        // SAFETY: The next write completes channel 2 reload MSB under the same exclusive guard.
        unsafe { self.channel_data.write((ticks >> 8) as u8) };
    }

    /// Reports whether the PIT channel 2 countdown has finished.
    fn has_elapsed(&mut self) -> bool {
        // SAFETY: bootstrap retains exclusive channel ownership, and bit 5
        // of port 0x61 is the read-only output state of PIT channel 2.
        unsafe { self.system_control.read() & SYSTEM_CONTROL_TIMER2_OUT != 0 }
    }
}

impl Drop for PitChannel2 {
    // Restore speaker and gate controls; the programmed mode and reload remain.
    fn drop(&mut self) {
        // SAFETY: the guard still owns the same port-0x61 control state and
        // restores the speaker and gate bits captured before calibration.
        unsafe { self.system_control.write(self.original_system_control) };
    }
}

/// Initializes TSC conversion state during bootstrap on the primary CPU.
///
/// The platform must call this function exactly once from primary-CPU early
/// driver initialization, before secondary CPUs or device drivers can access
/// legacy PIT channel 2. Clock reads must begin after this call completes.
/// Calibration polls with local interrupts disabled and requires CPL0 port access.
///
/// # Panics
///
/// Panics if the platform fallback is below 1 MHz or the selected frequency
/// cannot be represented by the timer conversion ratios.
pub fn early_init(config: TimerConfig) {
    assert!(
        config.nominal_frequency >= MIN_TSC_FREQUENCY,
        "x86 LAPIC/TSC timer frequency must be at least 1MHz"
    );
    let selected = discover_tsc_frequency(config.nominal_frequency);
    init_tsc_conversion(selected.frequency());
    // The runtime logger is not installed until after early driver initialization.
    let frequency_khz = selected.frequency().as_khz_floor();
    if matches!(selected, TscFrequency::PlatformStatic(_)) {
        kernel_boot::bootln!(
            "TSC discovery failed; using {} kHz from {}",
            frequency_khz,
            selected.source_name()
        );
    } else {
        if matches!(selected, TscFrequency::PitDegraded(_)) {
            log::warn!(
                "TSC frequency discovery is degraded: best PIT bounds are wider than {}%",
                PIT_SAMPLE_SPREAD_PERCENT
            );
        }
        kernel_boot::bootln!(
            "TSC frequency: {} kHz, source: {}",
            frequency_khz,
            selected.source_name()
        );
    }
    INIT_TICK.store(read_tsc_raw(), Ordering::Relaxed);
}

fn init_tsc_conversion(frequency: Frequency) {
    let frequency_khz = frequency.as_khz_floor();
    assert!(
        u32::try_from(frequency_khz).is_ok(),
        "TSC frequency must fit in u32 kHz"
    );
    TSC_FREQUENCY_HZ.store(frequency.as_hz(), Ordering::Relaxed);
    let frequency_khz_u32 = frequency_khz as u32;
    TSC_TICKS_TO_NANOS_RATIO
        .call_once(|| Ratio::new(ktime_types::NANOS_PER_MILLIS as u32, frequency_khz_u32));
    NANOS_TO_TSC_TICKS_RATIO
        .call_once(|| Ratio::new(frequency_khz_u32, ktime_types::NANOS_PER_MILLIS as u32));
}

// Discover the TSC frequency using real CPU and PIT hardware.
fn discover_tsc_frequency(platform_frequency: Frequency) -> TscFrequency {
    discover_tsc_frequency_with(
        &CpuId::new(),
        platform_frequency,
        |nominal, _is_qemu_tcg| {
            calibrate_samples(nominal, |timeout_tsc_ticks, start_gap| {
                x86_64::instructions::interrupts::without_interrupts(|| {
                    let mut probe = KernelCalibrationProbe {
                        pit: PitChannel2::acquire(),
                    };
                    measure_with(&mut probe, timeout_tsc_ticks, start_gap)
                })
            })
            .and_then(|(frequency, is_degraded)| {
                let frequency = filter_tsc_frequency(Some(frequency))?;
                Some(if is_degraded {
                    TscFrequency::PitDegraded(frequency)
                } else {
                    TscFrequency::Pit(frequency)
                })
            })
        },
    )
}

/// Selects the TSC frequency from PIT, hypervisor CPUID, CPU CPUID,
/// and the platform fallback. Hardware readers are replaceable for tests.
fn discover_tsc_frequency_with(
    cpuid: &CpuId<impl CpuIdReader>,
    platform_frequency: Frequency,
    calibrate_fn: impl FnOnce(Frequency, bool) -> Option<TscFrequency>,
) -> TscFrequency {
    let is_qemu_tcg = cpuid
        .get_hypervisor_info()
        .is_some_and(|info| matches!(info.identify(), Hypervisor::QEMU));
    let try_pit = || calibrate_fn(platform_frequency, is_qemu_tcg);
    let try_hypervisor = || hypervisor_tsc_frequency(cpuid).map(TscFrequency::Hypervisor);
    let try_cpuid = || cpuid_tsc_frequency(cpuid).map(TscFrequency::Cpuid);
    // TCG: PIT -> hypervisor -> CPUID.
    // Others: hypervisor -> CPUID -> PIT.
    let selected = if is_qemu_tcg {
        try_pit().or_else(try_hypervisor).or_else(try_cpuid)
    } else {
        try_hypervisor().or_else(try_cpuid).or_else(try_pit)
    };
    // Use the platform value when all runtime sources fail.
    selected.unwrap_or(TscFrequency::PlatformStatic(platform_frequency))
}

fn hypervisor_tsc_frequency(cpuid: &CpuId<impl CpuIdReader>) -> Option<Frequency> {
    let info = cpuid.get_hypervisor_info()?;
    // CPUID 0x40000010 is vendor-defined. QEMU publishes TSC kHz there only
    // under its KVM signature, while ACRN defines the same timing leaf.
    if !matches!(info.identify(), Hypervisor::KVM | Hypervisor::ACRN) {
        return None;
    }
    let frequency = Frequency::checked_from_khz(u64::from(info.tsc_frequency()?));
    filter_tsc_frequency(frequency)
}

fn cpuid_tsc_frequency(cpuid: &CpuId<impl CpuIdReader>) -> Option<Frequency> {
    if cpuid.get_vendor_info()?.as_str() != "GenuineIntel" {
        return None;
    }
    let tsc_info = cpuid.get_tsc_info()?;
    if tsc_info.numerator() == 0 || tsc_info.denominator() == 0 {
        return None;
    }
    filter_tsc_frequency(tsc_info.tsc_frequency().map(Frequency::from_hz)).or_else(|| {
        // With a valid TSC ratio but no usable crystal frequency, use the
        // processor base frequency as a fallback estimate of TSC frequency.
        let frequency_info = cpuid.get_processor_frequency_info()?;
        let base_mhz = frequency_info.processor_base_frequency();
        if base_mhz == 0 {
            return None;
        }
        filter_tsc_frequency(Frequency::checked_from_mhz(u64::from(base_mhz)))
    })
}

fn filter_tsc_frequency(frequency: Option<Frequency>) -> Option<Frequency> {
    frequency.filter(|frequency| {
        *frequency >= MIN_TSC_FREQUENCY && frequency.as_khz_floor() <= u64::from(u32::MAX)
    })
}

/// Hardware operations of one calibration sample.
///
/// Split from the arithmetic so tests can drive the exact read order and
/// inject host pauses between a TSC read and the following PIT poll.
trait CalibrationProbe {
    fn tsc(&mut self) -> u64;
    fn start_pit(&mut self);
    fn pit_elapsed(&mut self) -> bool;
}

/// One PIT countdown observed through TSC reads placed around the polls.
///
/// `low_tsc_ticks` comes from a TSC read that preceded a poll reporting "still
/// counting": the terminal count is after that poll, hence after the read.
/// `high_tsc_ticks` comes from a read taken after a poll observed the terminal
/// count. The true interval therefore lies between them even when the host
/// pauses the vCPU between any two operations.
#[derive(Clone, Copy)]
struct PitObservation {
    low_tsc_ticks: u64,
    high_tsc_ticks: u64,
}

impl PitObservation {
    /// Converts the sound interval into its midpoint and spread.
    fn estimate(self) -> Option<PitEstimate> {
        let low = frequency_from_ticks(self.low_tsc_ticks)?;
        let high = frequency_from_ticks(self.high_tsc_ticks)?;
        if high < low {
            return None;
        }
        let spread_hz = high.as_hz() - low.as_hz();
        Some(PitEstimate {
            frequency: Frequency::from_hz(low.as_hz() + spread_hz / 2),
            spread_hz: u128::from(spread_hz),
        })
    }
}

/// The frequency implied by one observation and its uncertainty.
struct PitEstimate {
    /// Midpoint between the rates implied by the two bounds.
    frequency: Frequency,
    /// Distance between the two implied rates.
    spread_hz: u128,
}

impl PitEstimate {
    /// Returns whether the interval is within `percent` of the midpoint. The
    /// midpoint of such an interval is within half that percentage of truth.
    fn is_within(&self, percent: u128) -> bool {
        self.spread_hz * 100 <= u128::from(self.frequency.as_hz()) * percent
    }
}

fn frequency_from_ticks(elapsed_tsc_ticks: u64) -> Option<Frequency> {
    if elapsed_tsc_ticks == 0 {
        return None;
    }
    let frequency_hz = u128::from(elapsed_tsc_ticks)
        .checked_mul(u128::from(PIT_TICK_RATE.as_hz()))?
        / u128::from(PIT_CALIBRATION_TICKS);
    u64::try_from(frequency_hz).ok().map(Frequency::from_hz)
}

/// Calibrates the TSC against the PIT and reports whether the result is a
/// degraded estimate rather than a tight one.
///
/// A tight sample is accepted as soon as it appears. If no sample is tight,
/// the tightest interval is reported as degraded only while its uncertainty
/// stays inside [`PIT_DEGRADED_SPREAD_PERCENT`]; otherwise discovery fails
/// instead of trusting a best guess.
///
/// A stalled countdown counts toward [`PIT_MAX_SAMPLE_TIMEOUTS`]; a transient
/// host pause only discards its own sample, because the remaining sample
/// budget already bounds the retries.
fn calibrate_samples(
    nominal_frequency: Frequency,
    mut sample: impl FnMut(u64, u64) -> Result<PitObservation, SampleRejection>,
) -> Option<(Frequency, bool)> {
    let nominal_hz = nominal_frequency.as_hz();
    let expected_ticks = (u128::from(nominal_hz) * u128::from(PIT_CALIBRATION_TICKS))
        / u128::from(PIT_TICK_RATE.as_hz());
    let start_gap_tsc_ticks = u64::try_from(expected_ticks / PIT_START_GAP_DIVISOR).ok()?;
    // A PIT that never fires, or a vCPU descheduled for this long, cannot
    // produce a sample. The countdown is only 1/20 of this budget at the
    // nominal rate, so a healthy sample never approaches it.
    let timeout_tsc_ticks = nominal_hz;
    let mut tightest: Option<PitEstimate> = None;
    let mut timeouts = 0;
    for _ in 0..PIT_CALIBRATION_SAMPLES {
        let observation = match sample(timeout_tsc_ticks, start_gap_tsc_ticks) {
            Ok(observation) => {
                timeouts = 0;
                observation
            }
            Err(SampleRejection::TimedOut) => {
                timeouts += 1;
                if timeouts >= PIT_MAX_SAMPLE_TIMEOUTS {
                    break;
                }
                continue;
            }
            Err(SampleRejection::Transient) => continue,
        };
        let Some(estimate) = observation.estimate() else {
            continue;
        };
        if estimate.is_within(PIT_SAMPLE_SPREAD_PERCENT) {
            return filter_tsc_frequency(Some(estimate.frequency))
                .map(|frequency| (frequency, false));
        }
        if tightest
            .as_ref()
            .is_none_or(|best| estimate.spread_hz < best.spread_hz)
        {
            tightest = Some(estimate);
        }
    }
    let estimate = tightest.filter(|estimate| estimate.is_within(PIT_DEGRADED_SPREAD_PERCENT))?;
    filter_tsc_frequency(Some(estimate.frequency)).map(|frequency| (frequency, true))
}

/// Why one calibration sample produced no usable interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SampleRejection {
    /// The countdown did not reach its terminal count inside the sample
    /// budget, so the PIT or the vCPU is stalled rather than merely delayed.
    TimedOut,
    /// A host pause or a stale hardware level spoiled this sample while the
    /// PIT kept running. Another sample can succeed.
    Transient,
}

/// Runs one countdown while keeping the interval bounds sound under host
/// preemption.
///
/// The TSC is read before every PIT poll. A read only becomes the lower bound
/// after its poll reports "still counting", which places the terminal count
/// after the read. The upper bound is read after a poll observed the terminal
/// count. A pause therefore widens the interval instead of moving both bounds
/// together, and the spread check can reject it.
///
/// A sample that runs past its timeout reports [`SampleRejection::TimedOut`];
/// a pause or stale level reports [`SampleRejection::Transient`], which the
/// caller may retry without treating the PIT as stuck.
fn measure_with(
    probe: &mut impl CalibrationProbe,
    timeout_tsc_ticks: u64,
    start_gap_tsc_ticks: u64,
) -> Result<PitObservation, SampleRejection> {
    let start_before = probe.tsc();
    probe.start_pit();
    let start_after = probe.tsc();
    let start_gap = start_after.wrapping_sub(start_before);
    // A pause while programming the PIT moves the measured start away from the
    // reload. The reload lies inside the window bounded by the two reads, so
    // use its midpoint and discard samples whose window exceeds the budget.
    if start_gap > start_gap_tsc_ticks {
        return Err(SampleRejection::Transient);
    }
    let start_tsc = start_before.wrapping_add(start_gap / 2);

    let mut low_tsc = start_tsc;
    for poll_count in 0..PIT_MAX_POLL_ITERATIONS {
        // Read before polling. If the poll reports "still counting", this read
        // is provably earlier than the terminal count.
        let probe_tsc = probe.tsc();
        if probe.pit_elapsed() {
            // The first poll already reports a terminal count: mode 0 drives
            // the output low on the reload, so that level is stale.
            if poll_count == 0 {
                return Err(SampleRejection::Transient);
            }
            let high_tsc = probe.tsc();
            let low_tsc_ticks = low_tsc.wrapping_sub(start_tsc);
            // The measured interval is the reference for the programming
            // window; the nominal rate may be far from reality.
            if u128::from(start_gap) * PIT_START_GAP_DIVISOR > u128::from(low_tsc_ticks) {
                return Err(SampleRejection::Transient);
            }
            return Ok(PitObservation {
                low_tsc_ticks,
                high_tsc_ticks: high_tsc.wrapping_sub(start_tsc),
            });
        }
        // The poll confirmed the countdown is still running; the read that
        // preceded it is a sound lower bound.
        low_tsc = probe_tsc;
        if probe_tsc.wrapping_sub(start_tsc) >= timeout_tsc_ticks {
            return Err(SampleRejection::TimedOut);
        }
    }
    // Exhausting the poll budget means the countdown never completed.
    Err(SampleRejection::TimedOut)
}

/// Drives the real PIT channel 2 and TSC through [`measure_with`].
struct KernelCalibrationProbe {
    pit: PitChannel2,
}

impl CalibrationProbe for KernelCalibrationProbe {
    fn tsc(&mut self) -> u64 {
        read_tsc_raw()
    }

    fn start_pit(&mut self) {
        self.pit.start_one_shot(PIT_CALIBRATION_TICKS);
    }

    fn pit_elapsed(&mut self) -> bool {
        self.pit.has_elapsed()
    }
}

#[inline]
fn read_tsc_raw() -> u64 {
    // SAFETY: `_rdtsc` is available on every x86_64 target supported by this
    // architecture-specific driver and does not access memory.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Configures and unmasks the current CPU's LAPIC timer in one-shot mode.
///
/// The local APIC must already be initialized on this CPU. Shared clock
/// calibration belongs to [`early_init`]; local setup is identical on the
/// bootstrap CPU and application CPUs. The caller keeps CPU IRQ delivery
/// masked until the timer handler is installed.
///
/// # Panics
///
/// Panics if the calling CPU has no initialized local APIC handle.
pub fn init_percpu() {
    use x2apic::lapic::{TimerDivide, TimerMode};

    x86_apic::with_local_apic(|lapic| {
        // SAFETY: with_local_apic retains the initialized current-CPU
        // controller with IRQs/preemption disabled and exclusive access.
        unsafe { lapic.set_timer_mode(TimerMode::OneShot) };
        // SAFETY: with_local_apic retains the initialized current-CPU
        // controller with IRQs/preemption disabled and exclusive access.
        unsafe { lapic.set_timer_divide(TimerDivide::Div1) };
        // SAFETY: with_local_apic retains the initialized current-CPU
        // controller with IRQs/preemption disabled and exclusive access.
        unsafe { lapic.enable_timer() };
    });
}

/// Returns elapsed TSC ticks since primary timer initialization.
///
/// [`early_init`] must complete before this function is used as a monotonic
/// time source.
#[inline]
fn now_ticks() -> khal::time::TimerTicks {
    khal::time::TimerTicks::from_raw(read_tsc_ticks_raw())
}

#[inline]
fn read_tsc_ticks_raw() -> u64 {
    read_tsc_raw().wrapping_sub(INIT_TICK.load(Ordering::Relaxed))
}

#[inline]
fn ticks_to_nanos(ticks: u64) -> u64 {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: TSC initialization installs both \
                  conversion ratios before publishing clock operations"
    )]
    TSC_TICKS_TO_NANOS_RATIO
        .get()
        .expect("x86 LAPIC/TSC timer conversion ratio is not initialized")
        .mul_trunc(ticks)
}

#[inline]
fn nanos_to_ticks(nanos: u64) -> u64 {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: TSC initialization installs both \
                  conversion ratios before publishing clock operations"
    )]
    NANOS_TO_TSC_TICKS_RATIO
        .get()
        .expect("x86 LAPIC/TSC timer conversion ratio is not initialized")
        .mul_trunc(nanos)
}

/// Returns the discovered TSC frequency in hertz.
///
/// # Panics
///
/// Panics if [`early_init`] has not completed.
#[inline]
fn frequency() -> Frequency {
    let frequency_hz = TSC_FREQUENCY_HZ.load(Ordering::Relaxed);
    assert!(frequency_hz != 0, "x86 LAPIC/TSC timer is not initialized");
    Frequency::from_hz(frequency_hz)
}

/// Returns the architectural LAPIC timer interrupt vector.
#[inline]
fn interrupt_id() -> usize {
    x86_apic::APIC_TIMER_VECTOR as usize
}

/// Arms the current CPU's LAPIC timer for a relative interval.
///
/// [`early_init`] and local APIC initialization for the calling CPU must
/// complete before this function is called.
///
/// # Panics
///
/// Panics if the TSC conversion ratio has not been initialized.
fn arm_timer_after(interval: ktime_types::TimeSpan) {
    let interval_ns = interval.as_nanos_u64_saturating();
    // SAFETY: the local APIC timer is initialized before deadline programming, and
    // the helper masks local interrupts while borrowing the CPU-local LAPIC handle.
    {
        x86_apic::with_local_apic(|lapic| {
            if interval_ns != 0 {
                #[expect(
                    clippy::expect_used,
                    reason = "CG-021-no-panic-on-reachable-failure: LAPIC timer initialization \
                              installs its conversion ratio before publishing timer operations"
                )]
                let apic_ticks = NANOS_TO_LAPIC_TICKS_RATIO
                    .get()
                    .expect("x86 LAPIC deadline ratio is not initialized")
                    // Initial-count is 32-bit; clamp and re-arbitrate on the early IRQ.
                    .mul_trunc(interval_ns)
                    .clamp(1, u32::MAX as u64);
                // SAFETY: with_local_apic holds the initialized current-CPU
                // controller exclusively with IRQs and preemption disabled.
                unsafe { lapic.set_timer_initial(apic_ticks as u32) };
            } else {
                // SAFETY: with_local_apic holds the initialized current-CPU
                // controller exclusively with IRQs and preemption disabled.
                unsafe { lapic.set_timer_initial(1) };
            }
        });
    }
}

/// Stops the current CPU's LAPIC timer countdown.
///
/// The local APIC must be initialized for the calling CPU before this call.
///
/// # Panics
///
/// Panics if the calling CPU has no initialized local APIC handle.
fn disarm_timer() {
    // SAFETY: LAPIC bring-up completes before any deadline programming, the
    // same precondition as `arm_timer`. `with_local_apic` masks local interrupts
    // while borrowing this CPU's LAPIC handle. Writing 0 to
    // the initial-count register stops the current countdown (Intel SDM).
    unsafe {
        x86_apic::with_local_apic(|lapic| {
            lapic.set_timer_initial(0);
        });
    }
}

unittest::path_mod_test!(tests, "../ut-tests/x86_lapic_tsc.rs");
