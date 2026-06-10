// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use int_ratio::Ratio;
use log::info;
use raw_cpuid::CpuId;

use crate::TimerSource;

const LAPIC_TICKS_PER_SEC: u64 = 1_000_000_000;
static mut NANOS_TO_LAPIC_TICKS_RATIO: Ratio = Ratio::zero();
static mut INIT_TICK: u64 = 0;
static mut CPU_FREQ_MHZ: u64 = 0;

struct X86LapicTscMonotonicTimer;

#[kplat::impl_dev_interface]
impl khal::time::MonotonicTimerIf for X86LapicTscMonotonicTimer {
    fn now_ticks() -> u64 {
        now_ticks()
    }

    fn t2ns(ticks: u64) -> u64 {
        t2ns(ticks)
    }

    fn freq() -> u64 {
        freq()
    }

    fn ns2t(nanos: u64) -> u64 {
        ns2t(nanos)
    }

    fn interrupt_id() -> usize {
        interrupt_id()
    }

    fn arm_timer(deadline_ns: u64) {
        arm_timer(deadline_ns)
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
    unsafe {
        CPU_FREQ_MHZ = config.nominal_frequency_hz / 1_000_000;
    }

    if let Some(freq) = CpuId::new()
        .get_processor_frequency_info()
        .map(|info| info.processor_base_frequency())
        .filter(|freq| *freq > 0)
    {
        unsafe { CPU_FREQ_MHZ = freq as u64 }
    }
    info!("TSC frequency: {} MHz", unsafe { CPU_FREQ_MHZ });
    unsafe {
        INIT_TICK = core::arch::x86_64::_rdtsc();
    }
}

pub fn init_primary() {
    unsafe {
        use x2apic::lapic::{TimerDivide, TimerMode};
        let lapic = x86_apic::local_apic();
        lapic.set_timer_mode(TimerMode::OneShot);
        lapic.set_timer_divide(TimerDivide::Div1);
        lapic.enable_timer();
        NANOS_TO_LAPIC_TICKS_RATIO = Ratio::new(LAPIC_TICKS_PER_SEC as u32, 1_000_000_000u32);
    }
}

pub fn init_secondary() {
    unsafe {
        x86_apic::local_apic().enable_timer();
    }
}

#[inline]
pub fn now_ticks() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() - INIT_TICK }
}

#[inline]
pub fn t2ns(ticks: u64) -> u64 {
    let freq_mhz = cpu_freq_mhz() as u128;
    let nanos = (ticks as u128 * 1_000) / freq_mhz;
    assert!(nanos <= u64::MAX as u128, "nanosecond timestamp overflow");
    nanos as u64
}

#[inline]
pub fn ns2t(nanos: u64) -> u64 {
    let freq_mhz = cpu_freq_mhz() as u128;
    let ticks = (nanos as u128 * freq_mhz) / 1_000;
    assert!(ticks <= u64::MAX as u128, "timer tick count overflow");
    ticks as u64
}

#[inline]
pub fn freq() -> u64 {
    cpu_freq_mhz()
        .checked_mul(1_000_000)
        .expect("TSC frequency overflow")
}

#[inline]
fn cpu_freq_mhz() -> u64 {
    // SAFETY: CPU_FREQ_MHZ is initialized during early timer setup and only read after initialization.
    let freq_mhz = unsafe { CPU_FREQ_MHZ };
    assert!(freq_mhz != 0, "x86 LAPIC/TSC timer is not initialized");
    freq_mhz
}

#[inline]
pub fn interrupt_id() -> usize {
    x86_apic::APIC_TIMER_VECTOR as usize
}

pub fn arm_timer(deadline_ns: u64) {
    let lapic = x86_apic::local_apic();
    let now_ns = t2ns(now_ticks());
    unsafe {
        if now_ns < deadline_ns {
            let apic_ticks = NANOS_TO_LAPIC_TICKS_RATIO.mul_trunc(deadline_ns - now_ns);
            assert!(apic_ticks <= u32::MAX as u64);
            lapic.set_timer_initial(apic_ticks.max(1) as u32);
        } else {
            lapic.set_timer_initial(1);
        }
    }
}
