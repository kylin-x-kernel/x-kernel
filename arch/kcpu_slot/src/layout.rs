// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::ptr;

const CACHE_LINE: usize = 64;

#[cfg(target_arch = "x86_64")]
#[used]
#[unsafe(link_section = ".cpu_slot.template")]
pub(crate) static CPU_SLOT_SELF_PTR: usize = 0;

#[cfg(not(test))]
unsafe extern "C" {
    static _cpu_slot_template_lma_start: u8;
    static _cpu_slot_template_size: u8;
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

/// Initializes one CPU's slot area from the linker template and selects it.
/// `base` must point to at least `stride()` writable bytes.
///
/// # Safety
/// `base` must be unique for this CPU and valid for copying the complete
/// template. No slot may be accessed until this function returns.
pub unsafe fn initialize_cpu(base: *mut u8) {
    let size = template_size();
    // SAFETY: Preconditions guarantee both regions are valid and non-overlapping.
    unsafe { ptr::copy_nonoverlapping(template_start() as *const u8, base, size) };
    #[cfg(not(test))]
    // SAFETY: The caller supplied an exclusive, initialized CPU area.
    unsafe {
        crate::arch::set_current_base(base as usize)
    };
}

/// Number of bytes required for one CPU's slot area.
pub const fn stride(template_size: usize) -> usize {
    template_size.saturating_add(CACHE_LINE - 1) & !(CACHE_LINE - 1)
}

/// Number of bytes required for one CPU's area in the linked image.
pub fn area_size() -> usize {
    stride(template_size())
}
