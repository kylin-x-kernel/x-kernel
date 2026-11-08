// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Direct-map access to boot-owned firmware storage.
//!
//! This is the allocation-free table path: it must remain callable from early
//! boot (including the pre-allocation x86 handoff) and is also used by later
//! queries. It assumes the boot firmware handed over usable physical addresses
//! and forms byte slices through the kernel direct map. It
//! applies no memory-category policy; the interpreter path validates every
//! access through its OSL.

use core::slice;

use kaddr_layout::PAGE_OFFSET;

/// Forms a byte slice over a validated SDT range.
pub(super) fn sdt_bytes(addr: usize, length: u32) -> &'static [u8] {
    // SAFETY: Callers pass validated ACPI table addresses and lengths obtained
    // from firmware headers, so the resulting byte slice stays in-bounds.
    unsafe { slice::from_raw_parts(addr_to_ptr::<u8>(addr), length as usize) }
}

/// Reads boot-owned storage, including headers not yet protocol-validated.
pub(super) fn bytes_from_addr(addr: usize, length: usize) -> &'static [u8] {
    // SAFETY: The boot contract supplies readable, resident, immutable firmware
    // storage. The caller checks address/slice arithmetic before this read.
    // Checksums do not establish physical accessibility; boot firmware does.
    unsafe { slice::from_raw_parts(addr_to_ptr::<u8>(addr), length) }
}

/// Translates a firmware physical address into a direct-map pointer.
pub(super) fn addr_to_ptr<T>(addr: usize) -> *const T {
    let mapped = if addr >= PAGE_OFFSET {
        addr
    } else {
        addr + PAGE_OFFSET
    };
    mapped as *const T
}
