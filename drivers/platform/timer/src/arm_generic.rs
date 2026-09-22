// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    convert::TryInto,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

#[cfg(feature = "vmm")]
use aarch64_cpu::registers::CNTHP_CTL_EL2;
use aarch64_cpu::registers::{
    CNTFRQ_EL0, CNTPCT_EL0, CNTV_CTL_EL0, CNTV_TVAL_EL0, CNTVCT_EL0, Readable, Writeable,
};
#[cfg(not(feature = "vmm"))]
use aarch64_cpu::registers::{CNTP_CTL_EL0, CNTP_TVAL_EL0};
use int_ratio::Ratio;
#[cfg(feature = "arm-timer-resume-fixup")]
use kcpu_slot::cpu_slot_cell;
use klazy::Once;
#[cfg(feature = "arm-timer-resume-fixup")]
use kspin::NoPreemptIrqSave;
use ktime_types::Frequency;
#[cfg(feature = "arm-timer-resume-fixup")]
use log::info;

use crate::TimerSource;

static TIMER_IRQ: AtomicUsize = AtomicUsize::new(0);
static TIMER_FREQ_HZ: AtomicU64 = AtomicU64::new(0);
static TIMER_MODE: AtomicUsize = AtomicUsize::new(TimerMode::Physical as usize);
static TIMER_INIT_TICKS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "arm-timer-resume-fixup")]
static TICK_RESUME_OFFSET: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "arm-timer-resume-fixup")]
cpu_slot_cell! {
    static LAST_LOGICAL_TICKS: u64 = 0;
}
#[cfg(feature = "arm-timer-resume-fixup")]
static IPI_FIXUP_PENDING: AtomicUsize = AtomicUsize::new(0);
static CNTPCT_TO_NANOS_RATIO: Once<Ratio> = Once::new();
static NANOS_TO_CNTPCT_RATIO: Once<Ratio> = Once::new();

// CNTP_TVAL_EL0 and CNTV_TVAL_EL0 interpret their low 32 bits as a signed
// countdown value. Values above i32::MAX therefore describe an expired timer.
const MAX_TIMER_INTERVAL_TICKS: u64 = i32::MAX as u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum TimerMode {
    Physical = 0,
    Virtual  = 1,
}

impl TimerMode {
    const fn as_usize(self) -> usize {
        self as usize
    }

    fn from_usize(value: usize) -> Self {
        match value {
            x if x == Self::Virtual.as_usize() => Self::Virtual,
            _ => Self::Physical,
        }
    }

    fn from_kconfig() -> Self {
        match kbuild_config::ARM_GENERIC_TIMER_MODE {
            "virtual" => Self::Virtual,
            _ => Self::Physical,
        }
    }

    fn preferred_interrupt_names(self) -> &'static [&'static str] {
        match self {
            // VHE: EL2 accesses to CNTP_*_EL0 are redirected to CNTHP_*_EL2
            // (the EL2 physical timer), which fires on INTID 26 ("hyp-phys").
            #[cfg(feature = "vmm")]
            Self::Physical => &["hyp-phys", "phys", "virt", "sec-phys", "hyp-virt"],
            #[cfg(not(feature = "vmm"))]
            Self::Physical => &["phys", "virt", "sec-phys", "hyp-phys", "hyp-virt"],
            Self::Virtual => &["virt", "phys", "sec-phys", "hyp-phys", "hyp-virt"],
        }
    }

    fn fallback_interrupt_indices(self) -> &'static [usize] {
        match self {
            // VHE: prefer hyp-phys (index 3) over phys (index 1).
            #[cfg(feature = "vmm")]
            Self::Physical => &[3, 1, 0, 2, 4],
            #[cfg(not(feature = "vmm"))]
            Self::Physical => &[1, 0, 2, 3, 4],
            Self::Virtual => &[2, 1, 0, 3, 4],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerConfig {
    pub irq: usize,
    pub frequency: Option<Frequency>,
    pub source: TimerSource,
    pub mode: TimerMode,
}

impl TimerConfig {
    /// Builds the platform-static timer descriptor for one non-zero IRQ
    /// line.
    pub const fn platform_static(irq: usize) -> Self {
        Self {
            irq,
            frequency: None,
            source: TimerSource::PlatformStatic,
            mode: TimerMode::Physical,
        }
    }
}

#[kiface::provide]
impl khal::time::ClockSourceIf {
    fn now_ticks() -> khal::time::TimerTicks {
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

    fn arm_timer(deadline: ktime_types::MonotonicInstant) {
        arm_timer(deadline)
    }

    fn disarm_timer() {
        disarm_timer()
    }

    #[cfg(feature = "arm-timer-resume-fixup")]
    fn handle_idle_return(previous_ticks: khal::time::TimerTicks) -> bool {
        handle_idle_return(previous_ticks.as_raw())
    }

    #[cfg(not(feature = "arm-timer-resume-fixup"))]
    fn handle_idle_return(_previous_ticks: khal::time::TimerTicks) -> bool {
        false
    }
}

/// Initializes the ARM generic timer from a platform-provided
/// configuration: validates the IRQ and frequency, records the counter
/// epoch (physical mode starts at 0; virtual mode snapshots the current
/// count), and installs the count-to-nanoseconds ratio.
///
/// # Panics
///
/// Panics for a zero IRQ, a zero frequency, or a frequency that does
/// not fit in `u32` — all platform configuration errors.
pub fn init(config: TimerConfig) {
    assert!(config.irq != 0, "ARM generic timer IRQ must be non-zero");
    TIMER_IRQ.store(config.irq, Ordering::Relaxed);
    TIMER_MODE.store(config.mode.as_usize(), Ordering::Relaxed);
    let init_ticks = match config.mode {
        TimerMode::Physical => 0,
        TimerMode::Virtual => raw_now_ticks(),
    };
    TIMER_INIT_TICKS.store(init_ticks, Ordering::Relaxed);
    #[cfg(feature = "arm-timer-resume-fixup")]
    TICK_RESUME_OFFSET.store(0, Ordering::Relaxed);

    let frequency = config
        .frequency
        .unwrap_or_else(|| Frequency::from_hz(CNTFRQ_EL0.get()));
    assert!(
        !frequency.is_zero(),
        "ARM generic timer frequency must be non-zero"
    );
    assert!(
        u32::try_from(frequency.as_hz()).is_ok(),
        "ARM generic timer frequency must fit in u32"
    );
    TIMER_FREQ_HZ.store(frequency.as_hz(), Ordering::Relaxed);
    let ratio = CNTPCT_TO_NANOS_RATIO
        .call_once(|| Ratio::new(ktime_types::NANOS_PER_SEC as u32, frequency.as_hz() as u32));
    NANOS_TO_CNTPCT_RATIO.call_once(|| ratio.inverse());
}

/// Enables the timer interrupt and arms the per-CPU countdown on the
/// current CPU.
pub fn init_percpu() {
    #[cfg(feature = "arm-timer-resume-fixup")]
    {
        let guard = NoPreemptIrqSave::new();
        LAST_LOGICAL_TICKS.write(
            &guard,
            logical_ticks(raw_now_ticks(), TICK_RESUME_OFFSET.load(Ordering::Relaxed)),
        );
    }
    rearm_local_timer_irq();
}

fn rearm_local_timer_irq() {
    let irq = interrupt_id();
    match mode() {
        TimerMode::Physical => {
            write_physical_timer_ctl(1);
            write_physical_timer_tval(0);
        }
        TimerMode::Virtual => {
            CNTV_CTL_EL0.write(CNTV_CTL_EL0::ENABLE::SET);
            CNTV_TVAL_EL0.set(0);
        }
    }
    kirq::enable(irq, true);
}

#[inline]
fn now_ticks() -> khal::time::TimerTicks {
    khal::time::TimerTicks::from_raw(now_ticks_raw())
}

#[inline]
fn now_ticks_raw() -> u64 {
    #[cfg(feature = "arm-timer-resume-fixup")]
    {
        track_logical_ticks(logical_ticks(
            raw_now_ticks(),
            TICK_RESUME_OFFSET.load(Ordering::Relaxed),
        ))
    }

    #[cfg(not(feature = "arm-timer-resume-fixup"))]
    {
        raw_now_ticks().saturating_sub(TIMER_INIT_TICKS.load(Ordering::Relaxed))
    }
}

#[cfg(feature = "arm-timer-resume-fixup")]
fn handle_idle_return(previous_ticks: u64) -> bool {
    let raw = raw_now_ticks();
    let cpu_id = khal::percpu::this_cpu_id();
    let old_offset = TICK_RESUME_OFFSET.load(Ordering::Relaxed);
    let current_ticks = logical_ticks(raw, old_offset);
    let repaired = if current_ticks < previous_ticks {
        install_resume_offset(raw, previous_ticks)
    } else {
        false
    };

    let new_offset = TICK_RESUME_OFFSET.load(Ordering::Relaxed);
    let fixed_ticks = track_logical_ticks(logical_ticks(raw_now_ticks(), new_offset));
    if repaired {
        let repair_ticks = fixed_ticks.saturating_sub(current_ticks);
        info!(
            "[PM-DBG] cpu{} repaired timer regression: before={}, current={}, after={}, \
             repair_ticks={}, repair_ns={}, raw_ticks={}, offset={}=>{}",
            cpu_id.as_usize(),
            previous_ticks,
            current_ticks,
            fixed_ticks,
            repair_ticks,
            ticks_to_nanos(repair_ticks),
            raw,
            old_offset,
            new_offset
        );
        rearm_local_timer_irq();
        request_remote_timer_fixup();
    }
    repaired
}

#[cfg(feature = "arm-timer-resume-fixup")]
/// Re-applies the per-CPU timer programming after an IPI-driven CPU
/// hotplug/cresume fixup (feature `arm-timer-resume-fixup`).
pub fn handle_ipi_fixup() {
    let cpu_id = khal::percpu::this_cpu_id();
    let bit = 1usize << cpu_id.as_usize();
    let pending = IPI_FIXUP_PENDING.fetch_and(!bit, Ordering::AcqRel);
    if pending & bit == 0 {
        return;
    }

    let offset = TICK_RESUME_OFFSET.load(Ordering::Relaxed);
    let fixed_ticks = track_logical_ticks(logical_ticks(raw_now_ticks(), offset));
    rearm_local_timer_irq();
    info!(
        "[PM-DBG] cpu{} applied remote timer fixup via IPI: logical_ticks={}, offset={}",
        cpu_id.as_usize(),
        fixed_ticks,
        offset
    );
}

#[inline]
fn ticks_to_nanos(ticks: u64) -> u64 {
    CNTPCT_TO_NANOS_RATIO
        .get()
        .expect("ARM generic timer conversion ratio is not initialized")
        .mul_trunc(ticks)
}

#[inline]
fn nanos_to_ticks(nanos: u64) -> u64 {
    NANOS_TO_CNTPCT_RATIO
        .get()
        .expect("ARM generic timer inverse conversion ratio is not initialized")
        .mul_trunc(nanos)
}

#[inline]
fn frequency() -> Frequency {
    Frequency::from_hz(TIMER_FREQ_HZ.load(Ordering::Relaxed))
}

#[inline]
fn interrupt_id() -> usize {
    let irq = TIMER_IRQ.load(Ordering::Relaxed);
    assert!(irq != 0, "ARM generic timer not initialized");
    irq
}

fn deadline_to_interval_ticks(deadline_ns: u64, now_ns: u64, frequency_hz: u64) -> u64 {
    let remaining_ns = deadline_ns.saturating_sub(now_ns);
    // Saturation is exact for this bounded result: i32::MAX * 1e9 fits
    // in u64, so any overflowing product already exceeds the TVAL limit.
    // Round up to avoid turning a positive sub-tick interval into zero.
    remaining_ns
        .saturating_mul(frequency_hz)
        .div_ceil(ktime_types::NANOS_PER_SEC)
        .min(MAX_TIMER_INTERVAL_TICKS)
}

fn arm_timer(deadline: ktime_types::MonotonicInstant) {
    // Sample the same logical clock used by software deadlines, including
    // the virtual-counter origin and optional resume offset. Converting an
    // absolute deadline to ticks can wrap before a later interval clamp.
    let now_ns = ticks_to_nanos(now_ticks_raw());
    let interval = deadline_to_interval_ticks(
        deadline.as_nanos_u64_saturating(),
        now_ns,
        frequency().as_hz(),
    );
    match mode() {
        TimerMode::Physical => {
            // TVAL then ENABLE: after `disarm_timer` the leftover TVAL may be 0.
            // Enabling first would assert ISTATUS immediately. Linux
            // `arch_timer` `set_next_event` writes TVAL, then CTRL.ENABLE.
            // Physical writes go through the VMM-aware helpers (EL2 CNTHP_*).
            write_physical_timer_tval(interval);
            write_physical_timer_ctl(1);
        }
        TimerMode::Virtual => {
            CNTV_TVAL_EL0.set(interval);
            CNTV_CTL_EL0.write(CNTV_CTL_EL0::ENABLE::SET);
        }
    }
}

fn disarm_timer() {
    match mode() {
        TimerMode::Physical => write_physical_timer_ctl(0),
        TimerMode::Virtual => CNTV_CTL_EL0.write(CNTV_CTL_EL0::ENABLE::CLEAR),
    }
}

#[cfg(feature = "vmm")]
#[inline]
fn write_physical_timer_ctl(value: u64) {
    // Under the aarch64 VMM configuration the host runs at EL2 with VHE
    // enabled, and IRQ 26 is the EL2 physical timer interrupt.
    CNTHP_CTL_EL2.set(value);
}

#[cfg(not(feature = "vmm"))]
#[inline]
fn write_physical_timer_ctl(value: u64) {
    if value & 1 != 0 {
        CNTP_CTL_EL0.write(CNTP_CTL_EL0::ENABLE::SET);
    } else {
        CNTP_CTL_EL0.write(CNTP_CTL_EL0::ENABLE::CLEAR);
    }
}

#[cfg(feature = "vmm")]
#[inline]
fn write_physical_timer_tval(value: u64) {
    // SAFETY: see `write_physical_timer_ctl`; TVAL is a 32-bit signed timer
    // value architecturally, and callers clamp positive intervals to u32.
    unsafe { core::arch::asm!("msr CNTHP_TVAL_EL2, {}", in(reg) value) };
}

#[cfg(not(feature = "vmm"))]
#[inline]
fn write_physical_timer_tval(value: u64) {
    CNTP_TVAL_EL0.set(value);
}

/// Discovers the ARM generic timer from the device tree (the `timer`
/// node's frequency and interrupt); returns `None` when absent.
pub fn config_from_device_tree() -> Option<TimerConfig> {
    let timer_mode = TimerMode::from_kconfig();
    let node = of::find_compatible("arm,armv8-timer")
        .or_else(|| of::find_compatible("arm,armv7-timer"))?;
    Some(TimerConfig {
        irq: timer_irq_from_device_tree(node, timer_mode)?,
        frequency: timer_frequency_from_device_tree(node),
        source: TimerSource::DeviceTree,
        mode: timer_mode,
    })
}

fn timer_irq_from_device_tree(
    node: of::FdtNode<'static, 'static>,
    mode: TimerMode,
) -> Option<usize> {
    let interrupts = node.property("interrupts")?.value;
    let names = node.property("interrupt-names").map(|prop| prop.value);

    preferred_timer_interrupt_index(names, mode)
        .and_then(|index| interrupt_spec_at(interrupts, index))
        .or_else(|| {
            mode.fallback_interrupt_indices()
                .iter()
                .find_map(|&index| interrupt_spec_at(interrupts, index))
        })
}

fn preferred_timer_interrupt_index(names: Option<&[u8]>, mode: TimerMode) -> Option<usize> {
    let names = names?;
    for preferred in mode.preferred_interrupt_names() {
        if let Some(index) = interrupt_names(names).position(|name| name == *preferred) {
            return Some(index);
        }
    }
    None
}

fn interrupt_names(names: &[u8]) -> impl Iterator<Item = &str> {
    names.split(|byte| *byte == 0).filter_map(|name| {
        if name.is_empty() {
            None
        } else {
            core::str::from_utf8(name).ok()
        }
    })
}

fn interrupt_spec_at(specs: &[u8], index: usize) -> Option<usize> {
    let spec = specs.chunks_exact(12).nth(index)?;
    let irq_type = read_be_u32(spec, 0)?;
    let irq_num = read_be_u32(spec, 1)?;
    let intid = match irq_type {
        0 => 32 + irq_num,
        1 => 16 + irq_num,
        _ => return None,
    };
    Some(intid as usize)
}

fn timer_frequency_from_device_tree(node: of::FdtNode<'static, 'static>) -> Option<Frequency> {
    let value = node.property("clock-frequency")?.value;
    match value.len() {
        4 => Some(Frequency::from_hz(u64::from(u32::from_be_bytes(
            value.try_into().ok()?,
        )))),
        8 => Some(Frequency::from_hz(u64::from_be_bytes(
            value.try_into().ok()?,
        ))),
        _ => None,
    }
}

fn read_be_u32(spec: &[u8], index: usize) -> Option<u32> {
    let start = index.checked_mul(4)?;
    let bytes: [u8; 4] = spec.get(start..start + 4)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

#[inline]
fn mode() -> TimerMode {
    TimerMode::from_usize(TIMER_MODE.load(Ordering::Relaxed))
}

#[cfg(feature = "arm-timer-resume-fixup")]
#[inline]
fn logical_ticks(raw_ticks: u64, resume_offset: u64) -> u64 {
    raw_ticks
        .wrapping_add(resume_offset)
        .saturating_sub(TIMER_INIT_TICKS.load(Ordering::Relaxed))
}

#[cfg(feature = "arm-timer-resume-fixup")]
#[inline]
fn track_logical_ticks(ticks: u64) -> u64 {
    let guard = NoPreemptIrqSave::new();
    let last = LAST_LOGICAL_TICKS.read(&guard);
    if ticks > last {
        LAST_LOGICAL_TICKS.write(&guard, ticks);
        ticks
    } else {
        last
    }
}

#[cfg(feature = "arm-timer-resume-fixup")]
fn install_resume_offset(raw_ticks: u64, target_ticks: u64) -> bool {
    loop {
        let current_offset = TICK_RESUME_OFFSET.load(Ordering::Relaxed);
        let desired_offset = target_ticks
            .saturating_add(TIMER_INIT_TICKS.load(Ordering::Relaxed))
            .saturating_sub(raw_ticks);
        if desired_offset <= current_offset {
            return false;
        }
        match TICK_RESUME_OFFSET.compare_exchange(
            current_offset,
            desired_offset,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(_) => continue,
        }
    }
}

#[cfg(feature = "arm-timer-resume-fixup")]
fn request_remote_timer_fixup() {
    if kcpu_id_map::nr_cpus() <= 1 {
        return;
    }

    let current_cpu = khal::percpu::this_cpu_id();
    let all_cpus_mask = if kcpu_id_map::nr_cpus() >= usize::BITS as usize {
        usize::MAX
    } else {
        (1usize << kcpu_id_map::nr_cpus()) - 1
    };
    let remote_mask = all_cpus_mask & !(1usize << current_cpu.as_usize());
    if remote_mask == 0 {
        return;
    }

    IPI_FIXUP_PENDING.fetch_or(remote_mask, Ordering::AcqRel);
    kirq::notify_cpu(
        kbuild_config::IPI_IRQ,
        kirq::TargetCpu::AllButSelf {
            me: current_cpu.into(),
            total: kcpu_id_map::nr_cpus(),
        },
    );
}

#[inline]
fn raw_now_ticks() -> u64 {
    match mode() {
        TimerMode::Physical => CNTPCT_EL0.get(),
        TimerMode::Virtual => CNTVCT_EL0.get(),
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::{MAX_TIMER_INTERVAL_TICKS, deadline_to_interval_ticks};

    #[def_test]
    fn expired_deadline_uses_zero_countdown() {
        assert_eq!(deadline_to_interval_ticks(0, 0, 1), 0);
        assert_eq!(deadline_to_interval_ticks(99, 100, 4_000_000_000), 0);
        assert_eq!(deadline_to_interval_ticks(100, 100, 1_000_000_000), 0);
    }

    #[def_test]
    fn countdown_rounds_positive_subtick_up() {
        assert_eq!(deadline_to_interval_ticks(101, 100, 1), 1);
        assert_eq!(deadline_to_interval_ticks(101, 100, 62_500_000), 1);
        assert_eq!(deadline_to_interval_ticks(116, 100, 62_500_000), 1);
        assert_eq!(deadline_to_interval_ticks(117, 100, 62_500_000), 2);
        assert_eq!(deadline_to_interval_ticks(101, 100, 4_000_000_000), 4);
    }

    #[def_test]
    fn far_deadline_clamps_before_conversion_can_wrap() {
        // The old absolute conversion at 4 GHz truncated 2^64 ticks to zero.
        assert_eq!(
            deadline_to_interval_ticks(1 << 62, 1, 4_000_000_000),
            MAX_TIMER_INTERVAL_TICKS
        );
        assert_eq!(
            deadline_to_interval_ticks(u64::MAX, 0, u32::MAX as u64),
            MAX_TIMER_INTERVAL_TICKS
        );
        assert_eq!(
            deadline_to_interval_ticks(u64::MAX, u64::MAX - 1, 4_000_000_000),
            4
        );
        assert_eq!(
            deadline_to_interval_ticks(MAX_TIMER_INTERVAL_TICKS, 0, 1_000_000_000),
            MAX_TIMER_INTERVAL_TICKS
        );
    }

    #[def_test]
    fn bounded_countdown_matches_wide_integer_reference() {
        for frequency in [
            1,
            32_768,
            19_200_000,
            62_500_000,
            1_000_000_000,
            u32::MAX as u64,
        ] {
            for now in [0, 1, 1_000_000, 1 << 62, u64::MAX - 1] {
                for deadline in [0, 1, 17, 1_000_000_001, 1 << 62, u64::MAX - 1, u64::MAX] {
                    let remaining = u128::from(deadline.saturating_sub(now));
                    let expected = (remaining * u128::from(frequency))
                        .div_ceil(u128::from(ktime_types::NANOS_PER_SEC))
                        .min(u128::from(MAX_TIMER_INTERVAL_TICKS))
                        as u64;
                    assert_eq!(
                        deadline_to_interval_ticks(deadline, now, frequency),
                        expected
                    );
                }
            }
        }
    }
}
