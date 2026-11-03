// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Initialization of allocator-owned normal memory, not arbitrary C buffers.

/// Initializes an exclusively owned normal-memory allocation without widening
/// stores beyond its requested bounds.
///
/// # Safety
///
/// `dst` must be valid for exclusive writes of `len` bytes in one live
/// allocation mapped as normal, cacheable RAM. It may be uninitialized, but
/// must not be MMIO, device memory, or concurrently accessed by DMA/readers.
pub(super) unsafe fn zero_owned_memory(dst: *mut u8, len: usize) {
    #[cfg(target_arch = "aarch64")]
    if len >= 256 {
        // Pin the PE whose DCZID supplies the block size for the entire loop.
        let _preempt_guard = kspin::NoPreempt::new();
        let dczid: u64;
        // SAFETY: DCZID_EL0 is a readable architectural capability register;
        // this instruction has no memory side effects or stack accesses.
        unsafe {
            core::arch::asm!("mrs {value}, dczid_el0", value = out(reg) dczid, options(nomem, nostack, preserves_flags))
        };
        if dczid & (1 << 4) == 0 {
            let block_bytes = 4usize << (dczid & 15);
            let prefix_bytes = dst.addr().wrapping_neg() & (block_bytes - 1);
            if prefix_bytes <= len && len - prefix_bytes >= block_bytes {
                // SAFETY: the prefix is inside the caller-owned allocation.
                // Raw byte stores initialize it without forming references.
                unsafe { core::ptr::write_bytes(dst, 0, prefix_bytes) };
                let bulk_bytes = (len - prefix_bytes) & !(block_bytes - 1);
                let bulk_end = prefix_bytes + bulk_bytes;
                for offset in (prefix_bytes..bulk_end).step_by(block_bytes) {
                    // SAFETY: each address is aligned to DCZID's block size,
                    // and its entire zero block lies within the exclusive
                    // normal-memory allocation. The asm is a memory write.
                    let address = dst.wrapping_add(offset);
                    // SAFETY: the aligned address points at a complete zero
                    // block inside the exclusive normal-memory allocation.
                    unsafe {
                        core::arch::asm!("dc zva, {address}", address = in(reg) address, options(nostack, preserves_flags))
                    };
                }
                // SAFETY: the remaining tail is in bounds and exclusive;
                // unlike DC ZVA these byte stores cannot touch a neighbour.
                let tail = dst.wrapping_add(bulk_end);
                // SAFETY: the tail lies in bounds and is exclusively owned.
                unsafe { core::ptr::write_bytes(tail, 0, len - bulk_end) };
                return;
            }
        }
    }
    // SAFETY: the caller owns this writable byte region, including on CPUs
    // where DC ZVA is prohibited or no complete aligned block fits.
    unsafe { core::ptr::write_bytes(dst, 0, len) };
}

unittest::path_mod_test!(tests, "../ut-tests/zero.rs");
