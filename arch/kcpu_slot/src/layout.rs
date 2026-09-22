// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    fmt,
    ptr::{self, NonNull},
    sync::atomic::{AtomicU8, Ordering},
};

use crate::{BootCpuToken, CpuId};

/// Alignment of every CPU slot area; shared with the linker script so the
/// Rust stride and the linker's `ALIGN` expressions cannot drift. Keeping the
/// stride and every area base a multiple of this value guarantees that adjacent
/// CPU areas never share a cache line.
pub(crate) const AREA_ALIGN: usize = kaddr_layout::CPU_SLOT_AREA_ALIGN;
const AREA_UNINITIALIZED: u8 = 0;
const AREA_INITIALIZING: u8 = 1;
const AREA_PUBLISHED: u8 = 2;

static CPU_AREA_STATES: [AtomicU8; kbuild_config::NR_CPUS] =
    [const { AtomicU8::new(AREA_UNINITIALIZED) }; kbuild_config::NR_CPUS];

/// An initialized and published per-CPU slot area.
#[derive(Clone, Copy)]
pub struct CpuArea {
    cpu: CpuId,
    base: NonNull<u8>,
}

impl CpuArea {
    /// Returns the logical CPU that owns this area.
    pub fn cpu(self) -> CpuId {
        self.cpu
    }

    pub(crate) fn base(self) -> usize {
        self.base.as_ptr() as usize
    }
}

/// Failure to begin initialization of a per-CPU slot area.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuInitError {
    /// The logical CPU exceeds the image's configured CPU capacity.
    OutOfRange,
    /// Another context is currently initializing this CPU's area.
    AlreadyInitializing,
    /// This CPU's area has already been published.
    AlreadyInitialized,
}

impl fmt::Display for CpuInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::OutOfRange => "logical CPU exceeds NR_CPUS",
            Self::AlreadyInitializing => "CPU slot area is already being initialized",
            Self::AlreadyInitialized => "CPU slot area is already initialized",
        })
    }
}

// `#[used]` keeps the self-pointer in the section even before the
// register-addressed path is instantiated. `.cpu_slot.template` owns VMA 0
// after the migration removed the old `.percpu` section.
#[cfg(target_arch = "x86_64")]
#[used]
#[unsafe(link_section = ".cpu_slot.template")]
pub(crate) static CPU_SLOT_SELF_PTR: usize = 0;

#[cfg(not(test))]
unsafe extern "C" {
    static _cpu_slot_template_lma_start: u8;
    static _cpu_slot_template_size: u8;
    static _cpu_slot_area_start: u8;
}

#[cfg(test)]
#[unsafe(no_mangle)]
static _cpu_slot_template_lma_start: u8 = 0;
#[cfg(test)]
#[unsafe(no_mangle)]
static _cpu_slot_template_size: u8 = 0;

pub(crate) fn template_start() -> usize {
    &raw const _cpu_slot_template_lma_start as usize
}

/// Returns the linker-computed template size.
#[cfg(not(test))]
pub fn template_size() -> usize {
    crate::__cpu_slot_symbol_offset!(_cpu_slot_template_size)
}

#[cfg(test)]
/// Returns zero in host-side unit tests without the kernel linker script.
pub fn template_size() -> usize {
    0
}

/// Returns the slot area base address for `cpu` in the linker-reserved
/// per-CPU region.
///
/// The region spans `NR_CPUS * stride(template_size())` bytes starting at the
/// `_cpu_slot_area_start` linker symbol, so callers can initialize every CPU
/// without a heap. `cpu` must be below the NR_CPUS the image was linked for.
#[cfg(not(test))]
pub fn cpu_area_base(cpu: usize) -> usize {
    // Keep this calculation in sync with the linker's ALIGN expression. In
    // particular, do not take the address of its absolute stride symbol:
    // AArch64 would emit a bounded PC-relative relocation for a numeric value.
    let area_stride = stride(template_size());
    &raw const _cpu_slot_area_start as usize + cpu * area_stride
}

#[cfg(test)]
/// Returns zero in host-side unit tests without the kernel linker script.
pub fn cpu_area_base(_cpu: usize) -> usize {
    0
}

/// Returns a checked handle to an initialized CPU area.
///
/// The acquire load pairs with [`BootCpuToken::publish`], so initialization
/// writes performed before publication are visible through the returned area.
pub fn area(cpu: CpuId) -> Option<CpuArea> {
    let cpu_index = cpu.as_usize();
    let state = CPU_AREA_STATES.get(cpu_index)?;
    if state.load(Ordering::Acquire) != AREA_PUBLISHED {
        return None;
    }
    let base = NonNull::new(cpu_area_base(cpu_index) as *mut u8)?;
    Some(CpuArea { cpu, base })
}

/// Returns the checked, published area of the current CPU without a CPU id.
///
/// Derives the slot index from the architecture base register against the
/// linker-reserved region. Self-hosting code (guards, CPU identity, active
/// trapframe) has no logical CPU id and no guard yet, so it uses this to reach
/// the checked [`CpuArea`] accessors. Returns `None` when the base does not
/// name a published area.
#[cfg(not(test))]
pub fn current_area() -> Option<CpuArea> {
    let base = crate::arch::current_base();
    // SAFETY: reading the address of a linker-defined symbol is always sound.
    let area_start = &raw const _cpu_slot_area_start as usize;
    let area_stride = stride(template_size());
    if area_stride == 0 || base < area_start {
        return None;
    }
    let offset = base - area_start;
    if !offset.is_multiple_of(area_stride) {
        return None;
    }
    let cpu = offset / area_stride;
    if cpu >= kbuild_config::NR_CPUS {
        return None;
    }
    area(CpuId::new(cpu))
}

/// Returns `None` in host-side unit tests without the kernel linker script.
#[cfg(test)]
pub fn current_area() -> Option<CpuArea> {
    None
}

/// Starts exclusive initialization of one CPU's slot area.
///
/// The template is copied and the architecture base register is installed
/// before the token is returned. Callers may then initialize CPU-specific
/// slot values through the token and finally publish it.
///
/// # Safety
/// This must run on the CPU identified by `cpu` during its exclusive bring-up
/// phase. No task, IRQ, NMI, or remote observer may access that CPU's slots
/// until the returned token is published.
///
/// # Errors
/// Returns [`CpuInitError::OutOfRange`] when `cpu` is at or above `NR_CPUS`,
/// [`CpuInitError::AlreadyInitializing`] when another context is bringing this
/// CPU up, or [`CpuInitError::AlreadyInitialized`] once it is published.
///
/// # Panics
/// Panics if the linker reserved a null slot area for `cpu`.
pub unsafe fn begin_cpu_init(cpu: CpuId) -> Result<BootCpuToken, CpuInitError> {
    let cpu_index = cpu.as_usize();
    let state = CPU_AREA_STATES
        .get(cpu_index)
        .ok_or(CpuInitError::OutOfRange)?;
    if let Err(observed) = state.compare_exchange(
        AREA_UNINITIALIZED,
        AREA_INITIALIZING,
        Ordering::Acquire,
        Ordering::Acquire,
    ) {
        return Err(if observed == AREA_INITIALIZING {
            CpuInitError::AlreadyInitializing
        } else {
            CpuInitError::AlreadyInitialized
        });
    }

    let base = NonNull::new(cpu_area_base(cpu_index) as *mut u8)
        .expect("linker supplied a null CPU slot area");
    let size = template_size();
    // SAFETY: The caller owns the bounded linker-reserved area exclusively;
    // the linker template is readable, equally sized, and non-overlapping.
    unsafe { ptr::copy_nonoverlapping(template_start() as *const u8, base.as_ptr(), size) };
    #[cfg(not(test))]
    // SAFETY: The copied area is initialized, belongs to the calling CPU, and
    // remains reserved for the lifetime of the kernel image.
    unsafe {
        crate::arch::set_current_base(base.as_ptr() as usize)
    };
    Ok(BootCpuToken::new(cpu, base))
}

pub(crate) fn publish_cpu_area(token: &mut BootCpuToken) -> CpuArea {
    CPU_AREA_STATES[token.cpu.as_usize()].store(AREA_PUBLISHED, Ordering::Release);
    token.is_published = true;
    CpuArea {
        cpu: token.cpu,
        base: token.base,
    }
}

/// Number of bytes required for one CPU's slot area.
pub const fn stride(template_size: usize) -> usize {
    template_size.saturating_add(AREA_ALIGN - 1) & !(AREA_ALIGN - 1)
}

/// Number of bytes required for one CPU's area in the linked image.
pub fn area_size() -> usize {
    stride(template_size())
}
