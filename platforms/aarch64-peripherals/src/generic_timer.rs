// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ARM generic timer helpers and adapter macro.
use aarch64_cpu::registers::{CNTFRQ_EL0, CNTP_TVAL_EL0, CNTPCT_EL0, Readable, Writeable};
use int_ratio::Ratio;
static mut CNTPCT_TO_NANOS_RATIO: Ratio = Ratio::zero();
static mut NANOS_TO_CNTPCT_RATIO: Ratio = Ratio::zero();
/// Read the current timer counter value.
#[inline]
pub fn now_ticks() -> u64 {
    CNTPCT_EL0.get()
}
/// Convert timer ticks to nanoseconds.
#[inline]
pub fn t2ns(ticks: u64) -> u64 {
    unsafe { CNTPCT_TO_NANOS_RATIO.mul_trunc(ticks) }
}
/// Convert nanoseconds to timer ticks.
#[inline]
pub fn ns2t(nanos: u64) -> u64 {
    unsafe { NANOS_TO_CNTPCT_RATIO.mul_trunc(nanos) }
}
/// Arm the timer to fire at the specified deadline (ns).
pub fn arm_timer(deadline_ns: u64) {
    let cnptct = CNTPCT_EL0.get();
    let cnptct_deadline = ns2t(deadline_ns);
    if cnptct < cnptct_deadline {
        let interval = cnptct_deadline - cnptct;
        debug_assert!(interval <= u32::MAX as u64);
        CNTP_TVAL_EL0.set(interval);
    } else {
        CNTP_TVAL_EL0.set(0);
    }
}
/// Return the timer frequency in Hz.
#[inline]
pub fn freq() -> u64 {
    CNTFRQ_EL0.get()
}
/// Initialize conversion ratios using the current timer frequency.
pub fn early_init() {
    let freq = CNTFRQ_EL0.get();
    unsafe {
        CNTPCT_TO_NANOS_RATIO = Ratio::new(kplat::timer::NS_SEC as u32, freq as u32);
        NANOS_TO_CNTPCT_RATIO = CNTPCT_TO_NANOS_RATIO.inverse();
    }
}
/// Enable the local timer interrupt and unmask its IRQ line.
pub fn enable_local(timer_interrupt_id: usize) {
    use aarch64_cpu::registers::CNTP_CTL_EL0;
    CNTP_CTL_EL0.write(CNTP_CTL_EL0::ENABLE::SET);
    CNTP_TVAL_EL0.set(0);
    kplat::interrupts::enable(timer_interrupt_id, true);
}
/// Implement `kplat::timer::GlobalTimer` for this backend.
#[macro_export]
#[allow(clippy::crate_in_macro_def)]
macro_rules! time_if_impl {
    ($name:ident) => {
        struct $name;
        #[impl_dev_interface]
        impl kplat::timer::GlobalTimer for $name {
            fn now_ticks() -> u64 {
                $crate::generic_timer::now_ticks()
            }

            fn t2ns(ticks: u64) -> u64 {
                $crate::generic_timer::t2ns(ticks)
            }

            fn ns2t(nanos: u64) -> u64 {
                $crate::generic_timer::ns2t(nanos)
            }

            fn offset_ns() -> u64 {
                $crate::pl031::offset_ns()
            }

            fn freq() -> u64 {
                $crate::generic_timer::freq()
            }

            fn interrupt_id() -> usize {
                crate::config::devices::TIMER_IRQ
            }

            fn arm_timer(deadline_ns: u64) {
                $crate::generic_timer::arm_timer(deadline_ns)
            }
        }
    };
}
