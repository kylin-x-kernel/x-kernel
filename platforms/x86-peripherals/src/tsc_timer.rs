// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TSC/LAPIC-based timer implementation for x86 platforms.

use int_ratio::Ratio;
use raw_cpuid::CpuId;

const LAPIC_TICKS_PER_SEC: u64 = 1_000_000_000;
static mut NANOS_TO_LAPIC_TICKS_RATIO: Ratio = Ratio::zero();
static mut INIT_TICK: u64 = 0;
static mut CPU_FREQ_MHZ: u64 = kbuild_config::TIMER_FREQUENCY_HZ as u64 / 1_000_000;
static mut RTC_EPOCHOFFSET_NANOS: u64 = 0;

/// Performs early timer initialization and TSC calibration.
pub fn early_init() {
    if let Some(freq) = CpuId::new()
        .get_processor_frequency_info()
        .map(|info| info.processor_base_frequency())
        && freq > 0
    {
        unsafe { CPU_FREQ_MHZ = freq as u64 }
    }
    kplat::kprintln!("TSC frequency: {} MHz", unsafe { CPU_FREQ_MHZ });
    unsafe {
        INIT_TICK = core::arch::x86_64::_rdtsc();
    }
    #[cfg(feature = "rtc")]
    {
        use x86_rtc::Rtc;
        let epoch_time_nanos = Rtc::new().get_unix_timestamp() * 1_000_000_000;
        unsafe {
            RTC_EPOCHOFFSET_NANOS = epoch_time_nanos - kplat::timer::t2ns(INIT_TICK);
        }
    }
}

/// Initializes the local APIC timer on the boot CPU.
pub fn init_primary() {
    unsafe {
        use x2apic::lapic::{TimerDivide, TimerMode};
        let lapic = crate::apic::local_apic();
        lapic.set_timer_mode(TimerMode::OneShot);
        lapic.set_timer_divide(TimerDivide::Div1);
        lapic.enable_timer();
        NANOS_TO_LAPIC_TICKS_RATIO =
            Ratio::new(LAPIC_TICKS_PER_SEC as u32, kplat::timer::NS_SEC as u32);
    }
}

/// Initializes the local APIC timer on a secondary CPU.
pub fn init_secondary() {
    unsafe {
        crate::apic::local_apic().enable_timer();
    }
}

/// Reads the current TSC tick count (relative to boot).
#[inline]
pub fn now_ticks() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() - INIT_TICK }
}

/// Converts TSC ticks to nanoseconds.
#[inline]
pub fn t2ns(ticks: u64) -> u64 {
    ticks * 1_000 / unsafe { CPU_FREQ_MHZ }
}

/// Converts nanoseconds to TSC ticks.
#[inline]
pub fn ns2t(nanos: u64) -> u64 {
    nanos * unsafe { CPU_FREQ_MHZ } / 1_000
}

/// Returns the RTC epoch offset in nanoseconds (0 if `rtc` feature is disabled).
#[inline]
pub fn offset_ns() -> u64 {
    unsafe { RTC_EPOCHOFFSET_NANOS }
}

/// Returns the actual TSC frequency in Hz (runtime-calibrated).
#[inline]
pub fn freq() -> u64 {
    unsafe { CPU_FREQ_MHZ * 1_000_000 }
}

/// Arms the LAPIC one-shot timer to fire at `deadline_ns`.
pub fn arm_timer(deadline_ns: u64) {
    let lapic = crate::apic::local_apic();
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

/// Implement `kplat::timer::GlobalTimer` using the TSC/LAPIC backend.
#[allow(clippy::crate_in_macro_def)]
#[macro_export]
macro_rules! time_if_impl {
    ($name:ident) => {
        struct $name;
        #[impl_dev_interface]
        impl kplat::timer::GlobalTimer for $name {
            fn now_ticks() -> u64 {
                $crate::tsc_timer::now_ticks()
            }

            fn t2ns(ticks: u64) -> u64 {
                $crate::tsc_timer::t2ns(ticks)
            }

            fn ns2t(nanos: u64) -> u64 {
                $crate::tsc_timer::ns2t(nanos)
            }

            fn offset_ns() -> u64 {
                $crate::tsc_timer::offset_ns()
            }

            fn freq() -> u64 {
                $crate::tsc_timer::freq()
            }

            fn interrupt_id() -> usize {
                kbuild_config::TIMER_IRQ
            }

            fn arm_timer(deadline_ns: u64) {
                $crate::tsc_timer::arm_timer(deadline_ns)
            }
        }
    };
}
