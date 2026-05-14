// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    convert::TryInto,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use aarch64_cpu::registers::{
    CNTFRQ_EL0, CNTP_CTL_EL0, CNTP_TVAL_EL0, CNTPCT_EL0, CNTV_CTL_EL0, CNTV_TVAL_EL0, CNTVCT_EL0,
    Readable, Writeable,
};
use int_ratio::Ratio;
#[cfg(feature = "crosvm")]
use log::info;

use crate::TimerSource;

static TIMER_IRQ: AtomicUsize = AtomicUsize::new(0);
static TIMER_FREQ_HZ: AtomicU64 = AtomicU64::new(0);
static TIMER_MODE: AtomicUsize = AtomicUsize::new(TimerMode::Physical as usize);
static TIMER_INIT_TICKS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "crosvm")]
static TICK_RESUME_OFFSET: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "crosvm")]
#[percpu::def_percpu]
static LAST_LOGICAL_TICKS: u64 = 0;
#[cfg(feature = "crosvm")]
static IPI_FIXUP_PENDING: AtomicUsize = AtomicUsize::new(0);
static mut CNTPCT_TO_NANOS_RATIO: Ratio = Ratio::zero();
static mut NANOS_TO_CNTPCT_RATIO: Ratio = Ratio::zero();

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
            Self::Physical => &["phys", "virt", "sec-phys", "hyp-phys", "hyp-virt"],
            Self::Virtual => &["virt", "phys", "sec-phys", "hyp-phys", "hyp-virt"],
        }
    }

    fn fallback_interrupt_indices(self) -> &'static [usize] {
        match self {
            Self::Physical => &[1, 0, 2, 3, 4],
            Self::Virtual => &[2, 1, 0, 3, 4],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerConfig {
    pub irq: usize,
    pub frequency_hz: Option<u64>,
    pub source: TimerSource,
    pub mode: TimerMode,
}

impl TimerConfig {
    pub const fn platform_static(irq: usize) -> Self {
        Self {
            irq,
            frequency_hz: None,
            source: TimerSource::PlatformStatic,
            mode: TimerMode::Physical,
        }
    }
}

struct ArmGenericMonotonicTimer;

#[kplat::impl_dev_interface]
impl khal::time::MonotonicTimerIf for ArmGenericMonotonicTimer {
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

    #[cfg(feature = "crosvm")]
    fn handle_idle_return(previous_ticks: u64) -> bool {
        handle_idle_return(previous_ticks)
    }
}

pub fn init(config: TimerConfig) {
    assert!(config.irq != 0, "ARM generic timer IRQ must be non-zero");
    TIMER_IRQ.store(config.irq, Ordering::Relaxed);
    TIMER_MODE.store(config.mode.as_usize(), Ordering::Relaxed);
    let init_ticks = match config.mode {
        TimerMode::Physical => 0,
        TimerMode::Virtual => raw_now_ticks(),
    };
    TIMER_INIT_TICKS.store(init_ticks, Ordering::Relaxed);
    #[cfg(feature = "crosvm")]
    TICK_RESUME_OFFSET.store(0, Ordering::Relaxed);

    let freq = config.frequency_hz.unwrap_or_else(|| CNTFRQ_EL0.get());
    assert!(freq != 0, "ARM generic timer frequency must be non-zero");
    assert!(
        u32::try_from(freq).is_ok(),
        "ARM generic timer frequency must fit in u32"
    );
    TIMER_FREQ_HZ.store(freq, Ordering::Relaxed);
    unsafe {
        CNTPCT_TO_NANOS_RATIO = Ratio::new(1_000_000_000u32, freq as u32);
        NANOS_TO_CNTPCT_RATIO = CNTPCT_TO_NANOS_RATIO.inverse();
    }
}

pub fn init_percpu() {
    #[cfg(feature = "crosvm")]
    unsafe {
        LAST_LOGICAL_TICKS.write_current_raw(logical_ticks(
            raw_now_ticks(),
            TICK_RESUME_OFFSET.load(Ordering::Relaxed),
        ));
    }
    rearm_local_timer_irq();
}

fn rearm_local_timer_irq() {
    let irq = interrupt_id();
    match mode() {
        TimerMode::Physical => {
            CNTP_CTL_EL0.write(CNTP_CTL_EL0::ENABLE::SET);
            CNTP_TVAL_EL0.set(0);
        }
        TimerMode::Virtual => {
            CNTV_CTL_EL0.write(CNTV_CTL_EL0::ENABLE::SET);
            CNTV_TVAL_EL0.set(0);
        }
    }
    khal::irq::enable(irq, true);
}

#[inline]
pub fn now_ticks() -> u64 {
    #[cfg(feature = "crosvm")]
    {
        track_logical_ticks(logical_ticks(
            raw_now_ticks(),
            TICK_RESUME_OFFSET.load(Ordering::Relaxed),
        ))
    }

    #[cfg(not(feature = "crosvm"))]
    {
        raw_now_ticks().saturating_sub(TIMER_INIT_TICKS.load(Ordering::Relaxed))
    }
}

#[cfg(feature = "crosvm")]
pub fn handle_idle_return(previous_ticks: u64) -> bool {
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
            t2ns(repair_ticks),
            raw,
            old_offset,
            new_offset
        );
        rearm_local_timer_irq();
        request_remote_timer_fixup();
    }
    repaired
}

#[cfg(feature = "crosvm")]
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
pub fn t2ns(ticks: u64) -> u64 {
    unsafe { CNTPCT_TO_NANOS_RATIO.mul_trunc(ticks) }
}

#[inline]
pub fn ns2t(nanos: u64) -> u64 {
    unsafe { NANOS_TO_CNTPCT_RATIO.mul_trunc(nanos) }
}

#[inline]
pub fn freq() -> u64 {
    TIMER_FREQ_HZ.load(Ordering::Relaxed)
}

#[inline]
pub fn interrupt_id() -> usize {
    let irq = TIMER_IRQ.load(Ordering::Relaxed);
    assert!(irq != 0, "ARM generic timer not initialized");
    irq
}

pub fn arm_timer(deadline_ns: u64) {
    let current_ticks = now_ticks();
    let deadline_ticks = ns2t(deadline_ns);
    if current_ticks < deadline_ticks {
        let interval = deadline_ticks - current_ticks;
        debug_assert!(interval <= u32::MAX as u64);
        match mode() {
            TimerMode::Physical => CNTP_TVAL_EL0.set(interval),
            TimerMode::Virtual => CNTV_TVAL_EL0.set(interval),
        }
    } else {
        match mode() {
            TimerMode::Physical => CNTP_TVAL_EL0.set(0),
            TimerMode::Virtual => CNTV_TVAL_EL0.set(0),
        }
    }
}

pub fn config_from_device_tree() -> Option<TimerConfig> {
    let timer_mode = TimerMode::from_kconfig();
    let node = of::find_compatible("arm,armv8-timer")
        .or_else(|| of::find_compatible("arm,armv7-timer"))?;
    Some(TimerConfig {
        irq: timer_irq_from_device_tree(node, timer_mode)?,
        frequency_hz: timer_frequency_from_device_tree(node),
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

fn timer_frequency_from_device_tree(node: of::FdtNode<'static, 'static>) -> Option<u64> {
    let value = node.property("clock-frequency")?.value;
    match value.len() {
        4 => Some(u32::from_be_bytes(value.try_into().ok()?) as u64),
        8 => Some(u64::from_be_bytes(value.try_into().ok()?)),
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

#[cfg(feature = "crosvm")]
#[inline]
fn logical_ticks(raw_ticks: u64, resume_offset: u64) -> u64 {
    raw_ticks
        .wrapping_add(resume_offset)
        .saturating_sub(TIMER_INIT_TICKS.load(Ordering::Relaxed))
}

#[cfg(feature = "crosvm")]
#[inline]
fn track_logical_ticks(ticks: u64) -> u64 {
    // Safety: this only accesses the current CPU's percpu timer state.
    let last = unsafe { LAST_LOGICAL_TICKS.read_current_raw() };
    if ticks > last {
        unsafe { LAST_LOGICAL_TICKS.write_current_raw(ticks) };
        ticks
    } else {
        last
    }
}

#[cfg(feature = "crosvm")]
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

#[cfg(feature = "crosvm")]
fn request_remote_timer_fixup() {
    if kbuild_config::CPU_NUM <= 1 {
        return;
    }

    let current_cpu = khal::percpu::this_cpu_id();
    let all_cpus_mask = if kbuild_config::CPU_NUM >= usize::BITS as usize {
        usize::MAX
    } else {
        (1usize << kbuild_config::CPU_NUM) - 1
    };
    let remote_mask = all_cpus_mask & !(1usize << current_cpu.as_usize());
    if remote_mask == 0 {
        return;
    }

    IPI_FIXUP_PENDING.fetch_or(remote_mask, Ordering::AcqRel);
    khal::irq::notify_cpu(
        kbuild_config::IPI_IRQ,
        khal::irq::TargetCpu::AllButSelf {
            me: current_cpu.into(),
            total: kbuild_config::CPU_NUM,
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
