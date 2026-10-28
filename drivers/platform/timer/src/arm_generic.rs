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
use klazy::Once;
#[cfg(feature = "arm-timer-resume-fixup")]
use kspin::{NoPreemptIrqSave, SpinNoIrq};
use ktime_types::Frequency;
#[cfg(feature = "arm-timer-resume-fixup")]
use log::info;

use crate::TimerSource;

static TIMER_IRQ: AtomicUsize = AtomicUsize::new(0);
static TIMER_FREQ_HZ: AtomicU64 = AtomicU64::new(0);
static TIMER_MODE: AtomicUsize = AtomicUsize::new(TimerMode::Physical as usize);
static TIMER_INIT_TICKS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "arm-timer-resume-fixup")]
#[path = "resume_counter.rs"]
mod resume_counter;

#[cfg(any(feature = "arm-timer-resume-fixup", unittest))]
#[path = "resume_notifications.rs"]
mod resume_notifications;

// Counter sampling and epoch updates must be one transaction across CPUs.
// The clock is also read from timer IRQs, so local IRQs must be masked.
#[cfg(feature = "arm-timer-resume-fixup")]
kspin::static_lock! {
    static RESUME_COUNTER: SpinNoIrq<resume_counter::ResumeCounter> =
        SpinNoIrq::new(resume_counter::ResumeCounter::new());
}
#[cfg(feature = "arm-timer-resume-fixup")]
static IPI_FIXUP_PENDING: resume_notifications::ResumeNotifications<{ kbuild_config::NR_CPUS }> =
    resume_notifications::ResumeNotifications::new();
#[cfg(feature = "arm-timer-resume-fixup")]
static REMOTE_FIXUP_REQUESTED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
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
impl khal::time::CounterSourceIf {
    fn now_ticks() -> khal::time::TimerTicks {
        now_ticks()
    }

    fn resume_ticks() -> khal::time::TimerTicks {
        // Observe platform counter restoration before establishing the anchor.
        aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
        #[cfg(feature = "arm-timer-resume-fixup")]
        {
            let mut counter = RESUME_COUNTER.lock();
            let ticks = counter.resume(raw_now_ticks());
            REMOTE_FIXUP_REQUESTED.store(false, Ordering::Release);
            IPI_FIXUP_PENDING.clear_quiesced();
            khal::time::TimerTicks::from_raw(ticks)
        }
        #[cfg(not(feature = "arm-timer-resume-fixup"))]
        {
            // Only a formal resume may move the origin. Ordinary reads retain
            // their saturating behavior for an unexpected counter regression.
            rebase_counter_origin(&TIMER_INIT_TICKS, raw_now_ticks())
        }
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
        // Do not reuse init_percpu: its zero countdown requests an event.
        disarm_timer();
        kirq::enable(interrupt_id(), true);
    }

    fn disarm_timer() {
        disarm_timer()
    }
}

#[kiface::provide]
impl kiface_time_api::ClockResumeIf {
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
    RESUME_COUNTER
        .lock()
        .initialize(raw_now_ticks(), TIMER_INIT_TICKS.load(Ordering::Relaxed));

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
    rearm_local_timer_irq();
}

fn rearm_local_timer_irq() {
    rearm_local_timer_registers();
    kirq::enable(interrupt_id(), true);
}

fn rearm_local_timer_registers() {
    match mode() {
        TimerMode::Physical => {
            write_physical_timer_tval(0);
            write_physical_timer_ctl(1);
        }
        TimerMode::Virtual => {
            CNTV_TVAL_EL0.set(0);
            CNTV_CTL_EL0.write(CNTV_CTL_EL0::ENABLE::SET);
        }
    }
}

#[inline]
fn now_ticks() -> khal::time::TimerTicks {
    khal::time::TimerTicks::from_raw(now_ticks_raw())
}

#[inline]
fn now_ticks_raw() -> u64 {
    #[cfg(feature = "arm-timer-resume-fixup")]
    {
        sample_resume_counter().ticks
    }

    #[cfg(not(feature = "arm-timer-resume-fixup"))]
    {
        ticks_since_origin(raw_now_ticks(), TIMER_INIT_TICKS.load(Ordering::Relaxed))
    }
}

#[cfg(any(not(feature = "arm-timer-resume-fixup"), unittest))]
fn rebase_counter_origin(origin: &AtomicU64, restored_ticks: u64) -> khal::time::TimerTicks {
    origin.store(restored_ticks, Ordering::Relaxed);
    khal::time::TimerTicks::from_raw(0)
}

#[cfg(any(not(feature = "arm-timer-resume-fixup"), unittest))]
fn ticks_since_origin(raw_ticks: u64, origin_ticks: u64) -> u64 {
    raw_ticks.saturating_sub(origin_ticks)
}

#[cfg(feature = "arm-timer-resume-fixup")]
fn handle_idle_return(_previous_ticks: u64) -> bool {
    sample_resume_counter().is_repaired
}

// The outer guard keeps repair side effects on the sampling CPU. Register
// programming happens after releasing the shared lock. Logging and IPI
// delivery are deferred to the timer handler.
#[cfg(feature = "arm-timer-resume-fixup")]
fn sample_resume_counter() -> resume_counter::CounterSample {
    let _guard = NoPreemptIrqSave::new();
    let sample = {
        let mut counter = RESUME_COUNTER.lock();
        let sample = counter.sample(raw_now_ticks());
        if sample.is_repaired {
            counter.queue_repair(khal::percpu::this_cpu_id().as_usize(), &sample);
            // Publication and consumption are serialized with the report.
            REMOTE_FIXUP_REQUESTED.store(true, Ordering::Release);
        }
        sample
    };
    if sample.is_repaired {
        // A clock read may hold IRQ-controller or console locks. Only touch
        // CPU-local registers here; notify peers from the timer handler.
        rearm_local_timer_registers();
    }
    sample
}

/// Sends deferred resume notifications from the timer handler, where no
/// caller's clock-read locks remain held. Never runs under the counter lock.
/// Requires initialized interrupt routing and a timer-handler context with no
/// caller-owned clock-read locks. Does not sleep or reconstruct lost sleep time.
#[cfg(feature = "arm-timer-resume-fixup")]
pub fn dispatch_resume_fixup() {
    if !REMOTE_FIXUP_REQUESTED.load(Ordering::Acquire) {
        return;
    }
    let _guard = NoPreemptIrqSave::new();
    let report = {
        let mut counter = RESUME_COUNTER.lock();
        let report = counter.take_repair();
        REMOTE_FIXUP_REQUESTED.store(false, Ordering::Release);
        report
    };
    if let Some(report) = report {
        // Any CPU can dispatch. Rearm it too: AllButSelf excludes this CPU,
        // which need not be the CPU that originally detected the reset.
        rearm_local_timer_registers();
        request_remote_timer_fixup();
        info!(
            "[PM-DBG] counter recovery notification: detector_cpu={}, dispatcher_cpu={}, \
             raw_ticks={}, logical_ticks={}",
            report.cpu,
            khal::percpu::this_cpu_id().as_usize(),
            report.raw_ticks,
            report.ticks
        );
    }
}

#[cfg(feature = "arm-timer-resume-fixup")]
/// Re-applies the per-CPU timer programming after an IPI-driven CPU
/// hotplug/cresume fixup (feature `arm-timer-resume-fixup`).
pub fn handle_ipi_fixup() {
    let cpu_id = khal::percpu::this_cpu_id();
    if !IPI_FIXUP_PENDING.take(cpu_id.as_usize()) {
        return;
    }

    let fixed_ticks = now_ticks_raw();
    rearm_local_timer_registers();
    info!(
        "[PM-DBG] cpu{} applied remote timer fixup via IPI: logical_ticks={}",
        cpu_id.as_usize(),
        fixed_ticks
    );
}

#[inline]
fn ticks_to_nanos(ticks: u64) -> u64 {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Generic timer initialization installs \
                  conversion ratios before publishing clock and timer operations"
    )]
    CNTPCT_TO_NANOS_RATIO
        .get()
        .expect("ARM generic timer conversion ratio is not initialized")
        .mul_trunc(ticks)
}

#[inline]
fn nanos_to_ticks(nanos: u64) -> u64 {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Generic timer initialization installs \
                  conversion ratios before publishing clock and timer operations"
    )]
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

fn arm_timer_after(interval: ktime_types::TimeSpan) {
    let interval =
        deadline_to_interval_ticks(interval.as_nanos_u64_saturating(), 0, frequency().as_hz());
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
fn request_remote_timer_fixup() {
    if kcpu_id_map::nr_cpus() <= 1 {
        return;
    }

    let current_cpu = khal::percpu::this_cpu_id();
    // AArch64 load_from_fdt filters enabled nodes and valid reg values before
    // load_from_raw_cpu_ids enumerates them. Present logical IDs are therefore
    // 0..nr_cpus(), even with disabled nodes or sparse hardware MPIDRs. The
    // normalized MPIDR cannot equal the map's usize::MAX invalid sentinel.
    if !IPI_FIXUP_PENDING.mark_peers(kcpu_id_map::nr_cpus(), current_cpu.as_usize()) {
        return;
    }

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
    // Serialize raw sampling only for the optional recovery path.
    #[cfg(feature = "arm-timer-resume-fixup")]
    aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
    match mode() {
        TimerMode::Physical => CNTPCT_EL0.get(),
        TimerMode::Virtual => CNTVCT_EL0.get(),
    }
}

unittest::path_mod_test!(tests, "../ut-tests/arm_generic.rs");
