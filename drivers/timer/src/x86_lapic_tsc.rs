// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicU64, Ordering};

use int_ratio::Ratio;
use klazy::Once;
use log::info;
use raw_cpuid::CpuId;

use crate::TimerSource;

const LAPIC_TICKS_PER_SEC: u64 = 1_000_000_000;
static INIT_TICK: AtomicU64 = AtomicU64::new(0);
static CPU_FREQ_MHZ: AtomicU64 = AtomicU64::new(0);
static TSC_TICKS_TO_NANOS_RATIO: Once<Ratio> = Once::new();
static NANOS_TO_TSC_TICKS_RATIO: Once<Ratio> = Once::new();
static NANOS_TO_LAPIC_TICKS_RATIO: Once<Ratio> = Once::initialized(Ratio::new(
    LAPIC_TICKS_PER_SEC as u32,
    ktime_types::NANOS_PER_SEC as u32,
));

#[kplat::impl_dev_interface]
impl khal::time::MonotonicTimerIf {
    fn now_ticks() -> khal::time::TimerTicks {
        now_ticks()
    }

    fn ticks_to_span(ticks: khal::time::TimerTicks) -> ktime_types::TimeSpan {
        ktime_types::TimeSpan::from_nanos(ticks_to_nanos(ticks.as_raw()))
    }

    fn freq() -> u64 {
        freq()
    }

    fn span_to_ticks(span: ktime_types::TimeSpan) -> khal::time::TimerTicks {
        khal::time::TimerTicks::from_raw(nanos_to_ticks(span.as_nanos_u64_saturating()))
    }

    fn interrupt_id() -> usize {
        interrupt_id()
    }

    fn arm_timer(deadline: ktime_types::MonotonicInstant) {
        arm_timer(deadline)
    }

    fn disarm_timer() {
        disarm_timer()
    }

    fn handle_idle_return(_previous_ticks: khal::time::TimerTicks) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerConfig {
    pub nominal_frequency_hz: u64,
    pub source: TimerSource,
}

impl TimerConfig {
    pub const fn platform_static(nominal_frequency_hz: u64) -> Self {
        Self {
            nominal_frequency_hz,
            source: TimerSource::PlatformStatic,
        }
    }
}

pub fn early_init(config: TimerConfig) {
    assert!(
        config.nominal_frequency_hz >= 1_000_000,
        "x86 LAPIC/TSC timer frequency must be at least 1MHz"
    );
    let mut freq_mhz = config.nominal_frequency_hz / 1_000_000;

    if let Some(freq) = CpuId::new()
        .get_processor_frequency_info()
        .map(|info| info.processor_base_frequency())
        .filter(|freq| *freq > 0)
    {
        freq_mhz = freq as u64;
    }
    assert!(
        u32::try_from(freq_mhz).is_ok(),
        "TSC frequency must fit in u32 MHz"
    );
    CPU_FREQ_MHZ.store(freq_mhz, Ordering::Relaxed);
    let freq_mhz_u32 = freq_mhz as u32;
    TSC_TICKS_TO_NANOS_RATIO
        .call_once(|| Ratio::new(ktime_types::NANOS_PER_MICROS as u32, freq_mhz_u32));
    NANOS_TO_TSC_TICKS_RATIO
        .call_once(|| Ratio::new(freq_mhz_u32, ktime_types::NANOS_PER_MICROS as u32));
    info!(
        "TSC frequency: {} MHz",
        CPU_FREQ_MHZ.load(Ordering::Relaxed)
    );
    // SAFETY: `_rdtsc` is available on the x86_64 target and is used here during
    // early timer initialization to capture the monotonic TSC baseline.
    INIT_TICK.store(unsafe { core::arch::x86_64::_rdtsc() }, Ordering::Relaxed);
}

pub fn init_primary() {
    // SAFETY: the bootstrap CPU has already initialized the local APIC, and the
    // helper holds its lock while programming the LAPIC timer registers.
    unsafe {
        use x2apic::lapic::{TimerDivide, TimerMode};
        x86_apic::with_local_apic(|lapic| {
            lapic.set_timer_mode(TimerMode::OneShot);
            lapic.set_timer_divide(TimerDivide::Div1);
            lapic.enable_timer();
        });
    }
}

pub fn init_secondary() {
    // SAFETY: secondary CPUs call this only after LAPIC bring-up; the helper
    // programs the current CPU's private LAPIC timer state to match the BSP's
    // one-shot / divide-by-1 configuration.
    unsafe {
        use x2apic::lapic::{TimerDivide, TimerMode};
        x86_apic::with_local_apic(|lapic| {
            lapic.set_timer_mode(TimerMode::OneShot);
            lapic.set_timer_divide(TimerDivide::Div1);
            lapic.enable_timer();
        })
    }
}

#[inline]
pub fn now_ticks() -> khal::time::TimerTicks {
    khal::time::TimerTicks::from_raw(read_tsc_ticks_raw())
}

#[inline]
fn read_tsc_ticks_raw() -> u64 {
    // SAFETY: `_rdtsc` is available on the x86_64 target and provides the current
    // monotonically increasing TSC value used by this timer source.
    unsafe { core::arch::x86_64::_rdtsc() - INIT_TICK.load(Ordering::Relaxed) }
}

#[inline]
pub(crate) fn ticks_to_nanos(ticks: u64) -> u64 {
    TSC_TICKS_TO_NANOS_RATIO
        .get()
        .expect("x86 LAPIC/TSC timer conversion ratio is not initialized")
        .mul_trunc(ticks)
}

#[inline]
pub(crate) fn nanos_to_ticks(nanos: u64) -> u64 {
    NANOS_TO_TSC_TICKS_RATIO
        .get()
        .expect("x86 LAPIC/TSC timer conversion ratio is not initialized")
        .mul_trunc(nanos)
}

#[inline]
pub fn freq() -> u64 {
    cpu_freq_mhz()
        .checked_mul(1_000_000)
        .expect("TSC frequency overflow")
}

#[inline]
fn cpu_freq_mhz() -> u64 {
    let freq_mhz = CPU_FREQ_MHZ.load(Ordering::Relaxed);
    assert!(freq_mhz != 0, "x86 LAPIC/TSC timer is not initialized");
    freq_mhz
}

#[inline]
pub fn interrupt_id() -> usize {
    x86_apic::APIC_TIMER_VECTOR as usize
}

pub fn arm_timer(deadline: ktime_types::MonotonicInstant) {
    let now_ns = ticks_to_nanos(now_ticks().as_raw());
    let deadline_ns = deadline.as_nanos_u64_saturating();
    // SAFETY: the local APIC timer is initialized before deadline programming, and
    // the helper holds the LAPIC lock while updating the timer registers.
    unsafe {
        x86_apic::with_local_apic(|lapic| {
            if now_ns < deadline_ns {
                let apic_ticks = NANOS_TO_LAPIC_TICKS_RATIO
                    .get()
                    .expect("x86 LAPIC deadline ratio is not initialized")
                    // Initial-count is 32-bit; clamp and re-arbitrate on the early IRQ.
                    .mul_trunc(deadline_ns - now_ns)
                    .clamp(1, u32::MAX as u64);
                lapic.set_timer_initial(apic_ticks as u32);
            } else {
                lapic.set_timer_initial(1);
            }
        });
    }
}

pub fn disarm_timer() {
    // SAFETY: LAPIC bring-up completes before any deadline programming, the
    // same precondition as `arm_timer`. `with_local_apic` holds the local APIC
    // lock, so this CPU exclusively updates the timer registers. Writing 0 to
    // the initial-count register stops the current countdown (Intel SDM).
    unsafe {
        x86_apic::with_local_apic(|lapic| {
            lapic.set_timer_initial(0);
        });
    }
}
