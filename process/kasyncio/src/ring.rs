// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux `io_uring` shared-ring memory (single-mmap subset).
//!
//! The kernel allocates one physically contiguous block (2 MiB up to 16384
//! entries, 4 MiB up to Linux `IORING_MAX_ENTRIES`) and keeps a direct-map
//! pointer for all ring accesses. Userspace obtains the same pages through
//! `mmap(fd, IORING_OFF_SQ_RING)` and `mmap(fd, IORING_OFF_SQES)`, and the VMA
//! keeps the block alive after the descriptor closes.
//!
//! Layout inside the block:
//!
//! ```text
//! 0x0000  SQ head/tail/mask/entries/flags/dropped
//! 0x0040  u32 SQ index array (sq_entries entries)
//! align   CQ head/tail/mask/entries/overflow
//! align   struct io_uring_cqe array (cq_entries entries)
//! align   struct io_uring_sqe array (sq_entries entries)
//! ```
//!
//! Index storage is shared memory and therefore hostile: the kernel keeps its
//! own `sq_head`/`cq_tail`, masks every user-published index, and validates
//! SQEs before queueing them.

use alloc::sync::Arc;
use core::{
    ptr,
    sync::atomic::{Ordering, fence},
};

use kalloc::{UsageKind, global_allocator};
use kerrno::{KError, KResult};
use khal::mem::v2p;
use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr, align_up_4k};

use crate::{
    abi::{
        IO_URING_CQE_SIZE, IO_URING_SQE_SIZE, IORING_FEAT_SINGLE_MMAP, IoUringCqOffsets,
        IoUringCqe, IoUringSqOffsets, IoUringSqe,
    },
    context::{AsyncContext, CompletedOp, PreparedOp},
};

/// Largest backing block: enough for 32768 SQEs plus the CQ ring, matching
/// Linux `IORING_MAX_ENTRIES`.
pub(crate) const RING_MAX_MEM: usize = 4 * 1024 * 1024;
/// Maximum supported SQ/CQ depth, matching Linux `IORING_MAX_ENTRIES`.
pub(crate) const MAX_ENTRIES: u32 = 32_768;

const SQ_HEAD: usize = 0x00;
const SQ_TAIL: usize = 0x04;
const SQ_MASK: usize = 0x08;
const SQ_ENTRIES: usize = 0x0c;
const SQ_FLAGS: usize = 0x10;
const SQ_DROPPED: usize = 0x14;
const SQ_ARRAY: usize = 0x40;

const CQ_HEADER_SIZE: usize = 0x40;

/// Owns the physically contiguous backing block of one ring.
///
/// A user mapping keeps an `Arc` of this object through the VMA's owning
/// linear runtime, so closing the descriptor before `munmap` cannot free
/// pages that user PTEs still reference.
pub(crate) struct RingPages {
    base: *mut u8,
    base_paddr: PhysAddr,
    len_bytes: usize,
    num_pages: usize,
}

// SAFETY: the block is one exclusive kernel allocation; the direct-map pointer
// is stable until Drop, and ring access is synchronized through the shared
// head/tail words.
unsafe impl Send for RingPages {}
// SAFETY: see `Send`.
unsafe impl Sync for RingPages {}

impl RingPages {
    fn new(len_bytes: usize) -> KResult<Self> {
        // Round the block up to a page multiple so the raw zero-fill below is
        // covered by exactly the pages the allocator returns.
        let len_bytes = align_up_4k(len_bytes);
        if len_bytes == 0 {
            return Err(KError::InvalidInput);
        }
        let num_pages = len_bytes / PAGE_SIZE_4K;
        if num_pages * PAGE_SIZE_4K < len_bytes {
            return Err(KError::InvalidInput);
        }
        let va = global_allocator()
            .alloc_pages(num_pages, PAGE_SIZE_4K, UsageKind::VirtMem)
            .map_err(|_| KError::NoMemory)?;
        let base_paddr = v2p(VirtAddr::from(va));
        // Charge the ring to the caller's memory cgroup, like every PageRef
        // allocation; a raw page allocation would otherwise bypass memcg.
        if memacct::charge_frame(base_paddr, len_bytes, UsageKind::VirtMem).is_err() {
            global_allocator().dealloc_pages(va, num_pages, UsageKind::VirtMem);
            return Err(KError::NoMemory);
        }
        let base = va as *mut u8;
        // SAFETY: `va` is a fresh allocation of `num_pages * PAGE_SIZE_4K`,
        // which the check above proves is at least `len_bytes`; the zero-fill
        // stays inside the allocation.
        unsafe { ptr::write_bytes(base, 0, len_bytes) };
        Ok(Self {
            base,
            base_paddr,
            len_bytes,
            num_pages,
        })
    }

    fn base(&self) -> *mut u8 {
        self.base
    }

    fn base_paddr(&self) -> PhysAddr {
        self.base_paddr
    }
}

impl Drop for RingPages {
    fn drop(&mut self) {
        memacct::uncharge_frame(self.base_paddr, self.len_bytes);
        global_allocator().dealloc_pages(self.base as usize, self.num_pages, UsageKind::VirtMem);
    }
}

/// Kernel-side state and layout of one io_uring ring.
pub(crate) struct Ring {
    pages: Arc<RingPages>,
    base: *mut u8,
    base_paddr: PhysAddr,
    sq_entries: u32,
    cq_entries: u32,
    sq_array_off: usize,
    cq_head_off: usize,
    cqes_off: usize,
    sqes_off: usize,
    ring_len: usize,
    /// Kernel-consumed SQ index (free-running).
    sq_head: u32,
    /// Kernel-published CQ index (free-running).
    cq_tail: u32,
}

// SAFETY: the direct-map pointer is stable for the lifetime of `pages`; all
// accesses are volatile and synchronized through the shared head/tail words.
unsafe impl Send for Ring {}
// SAFETY: see `Send`; shared accesses are volatile and index-masked.
unsafe impl Sync for Ring {}

impl Ring {
    /// Allocates and initializes ring memory for `entries` SQEs.
    ///
    /// Requires sleepable kernel context with the direct map available.
    pub(crate) fn create(entries: u32) -> KResult<Self> {
        if entries == 0 || entries > MAX_ENTRIES {
            return Err(KError::InvalidInput);
        }
        // Linux rounds the requested depth up to a power of two.
        let entries = entries.next_power_of_two();
        let cq_entries = entries;
        let sq_array_off = SQ_ARRAY;
        let sq_array_end = sq_array_off + entries as usize * 4;
        let cq_head_off = align_up_4k(sq_array_end);
        let cqes_off = align_up_4k(cq_head_off + CQ_HEADER_SIZE);
        let sqes_off = align_up_4k(cqes_off + cq_entries as usize * IO_URING_CQE_SIZE);
        let used = sqes_off + entries as usize * IO_URING_SQE_SIZE;
        // Size the block to the layout (rounded to a buddy-order size) instead
        // of reserving a fixed multi-MiB region for tiny rings.
        let ring_len = align_up_4k(used).next_power_of_two();
        if ring_len > RING_MAX_MEM || used > ring_len {
            return Err(KError::InvalidInput);
        }

        let pages = Arc::new(RingPages::new(ring_len)?);
        let base_paddr = pages.base_paddr();
        let base = pages.base();

        let ring = Self {
            pages,
            base,
            base_paddr,
            sq_entries: entries,
            cq_entries,
            sq_array_off,
            cq_head_off,
            cqes_off,
            sqes_off,
            ring_len,
            sq_head: 0,
            cq_tail: 0,
        };
        ring.write_u32(SQ_HEAD, 0);
        ring.write_u32(SQ_TAIL, 0);
        ring.write_u32(SQ_MASK, entries - 1);
        ring.write_u32(SQ_ENTRIES, entries);
        ring.write_u32(SQ_FLAGS, 0);
        ring.write_u32(SQ_DROPPED, 0);
        ring.write_u32(cq_head_off, 0);
        ring.write_u32(cq_head_off + 4, 0);
        ring.write_u32(cq_head_off + 8, cq_entries - 1);
        ring.write_u32(cq_head_off + 12, cq_entries);
        ring.write_u32(cq_head_off + 16, 0);
        // `cq_flags` is a distinct word after the overflow counter; report its
        // real offset instead of aliasing the SQ head at offset zero.
        ring.write_u32(cq_head_off + 20, 0);
        Ok(ring)
    }

    /// Returns the physical base address of the ring block.
    pub(crate) fn base_paddr(&self) -> PhysAddr {
        self.base_paddr
    }

    /// Returns the backing pages so a user mapping can keep them alive after
    /// the ring file is closed.
    pub(crate) fn pages(&self) -> Arc<RingPages> {
        self.pages.clone()
    }

    /// Returns the complete mapped block length.
    pub(crate) fn ring_len(&self) -> usize {
        self.ring_len
    }

    /// Returns the offset of the SQE array inside the block.
    pub(crate) fn sqes_offset(&self) -> usize {
        self.sqes_off
    }

    /// Returns the SQ depth.
    pub(crate) fn sq_entries(&self) -> u32 {
        self.sq_entries
    }

    /// Returns the CQ depth.
    pub(crate) fn cq_entries(&self) -> u32 {
        self.cq_entries
    }

    /// Returns the `io_sqring_offsets` reported in `io_uring_params`.
    pub(crate) fn sq_offsets(&self) -> IoUringSqOffsets {
        IoUringSqOffsets {
            head: SQ_HEAD as u32,
            tail: SQ_TAIL as u32,
            ring_mask: SQ_MASK as u32,
            ring_entries: SQ_ENTRIES as u32,
            flags: SQ_FLAGS as u32,
            dropped: SQ_DROPPED as u32,
            array: self.sq_array_off as u32,
            resv1: 0,
            user_addr: 0,
        }
    }

    /// Returns the `io_cqring_offsets` reported in `io_uring_params`.
    pub(crate) fn cq_offsets(&self) -> IoUringCqOffsets {
        IoUringCqOffsets {
            head: self.cq_head_off as u32,
            tail: (self.cq_head_off + 4) as u32,
            ring_mask: (self.cq_head_off + 8) as u32,
            ring_entries: (self.cq_head_off + 12) as u32,
            overflow: (self.cq_head_off + 16) as u32,
            cqes: self.cqes_off as u32,
            flags: (self.cq_head_off + 20) as u32,
            resv1: 0,
            user_addr: 0,
        }
    }

    /// Returns the feature bits this ring implements.
    pub(crate) fn features(&self) -> u32 {
        IORING_FEAT_SINGLE_MMAP
    }

    /// Consumes up to `to_submit` SQEs from the shared SQ ring.
    ///
    /// Every consumed entry advances the shared SQ head, so an invalid or
    /// unsupported SQE cannot wedge the ring: it is completed with `-EINVAL`
    /// and the following entries still run, as in Linux.
    pub(crate) fn submit(&mut self, ctx: &AsyncContext, to_submit: u32) -> KResult<u32> {
        let tail = self.read_u32(SQ_TAIL);
        // Linux clamps a short submission to the published entries instead of
        // failing it.
        let available = tail.wrapping_sub(self.sq_head).min(self.sq_entries);
        let to_submit = to_submit.min(available);
        // Pairs with the user's Release publication of SQ_TAIL: observing the
        // tail makes every SQE field written before it visible here.
        fence(Ordering::Acquire);

        let mask = self.sq_entries - 1;
        // `submitted` is the return count; `consumed` advances the shared head.
        let mut submitted = 0;
        let mut consumed = 0;
        for index in 0..to_submit {
            let sq_index = (self.sq_head.wrapping_add(index) & mask) as usize;
            let array_off = self.sq_array_off + sq_index * 4;
            let sqe_index = self.read_u32(array_off);
            if sqe_index >= self.sq_entries {
                // Linux's `io_get_sqe` drops an invalid array entry: consume
                // it, bump SQ_DROPPED, and stop the batch without a CQE.
                let dropped = self.read_u32(SQ_DROPPED);
                self.write_u32(SQ_DROPPED, dropped.wrapping_add(1));
                consumed += 1;
                break;
            }
            let sqe = self.read_sqe(sqe_index);
            if sqe.flags != 0 || !sqe.is_supported_opcode() {
                // Unsupported flags/opcodes complete with -EINVAL and are
                // consumed so later SQEs are not blocked. Bound the staged
                // backlog: if the completion queue is already at its limit,
                // leave this entry at the head for the next enter.
                if ctx.completion_len() >= self.cq_entries as usize {
                    break;
                }
                ctx.push_completion(CompletedOp {
                    user_data: sqe.user_data,
                    res: -kerrno::LinuxError::EINVAL.into_raw(),
                });
                submitted += 1;
                consumed += 1;
                continue;
            }
            match ctx.push_ready(PreparedOp {
                user_data: sqe.user_data,
            }) {
                Ok(()) => {
                    submitted += 1;
                    consumed += 1;
                }
                Err(KError::WouldBlock) => break,
                Err(error) => {
                    if consumed == 0 {
                        return Err(error);
                    }
                    break;
                }
            }
        }
        self.sq_head = self.sq_head.wrapping_add(consumed);
        self.write_u32(SQ_HEAD, self.sq_head);
        Ok(submitted)
    }

    /// Publishes staged completions into the shared CQ ring.
    pub(crate) fn flush_completions(&mut self, ctx: &AsyncContext) -> KResult<u32> {
        let mut published = 0;
        loop {
            let cq_head = self.read_u32(self.cq_head_off);
            if self.cq_tail.wrapping_sub(cq_head) >= self.cq_entries {
                break;
            }
            let Some(completion) = ctx.peek_completion() else {
                break;
            };
            let slot = (self.cq_tail & (self.cq_entries - 1)) as usize;
            let cqe = IoUringCqe {
                user_data: completion.user_data,
                res: completion.res,
                flags: 0,
            };
            self.write_cqe(slot, cqe);
            let _ = ctx.pop_completion();
            // Pairs with the user's Acquire read of the CQ tail: the CQE store
            // above must be visible before the new tail is published.
            fence(Ordering::Release);
            self.cq_tail = self.cq_tail.wrapping_add(1);
            self.write_u32(self.cq_head_off + 4, self.cq_tail);
            published += 1;
        }
        Ok(published)
    }

    /// Returns the number of CQEs published into the shared ring that the
    /// user has not consumed yet.
    pub(crate) fn pending_cqes(&self) -> u32 {
        let cq_head = self.read_u32(self.cq_head_off);
        self.cq_tail.wrapping_sub(cq_head)
    }

    fn read_u32(&self, offset: usize) -> u32 {
        let ptr = self.base.wrapping_add(offset).cast::<u32>();
        // SAFETY: every caller passes an offset produced by `Ring::create`'s
        // layout arithmetic: the fixed SQ_HEAD..SQ_DROPPED words,
        // `cq_head_off + {0,4,8,12,16,20}`, or a masked array slot
        // (`sq_index < sq_entries`). `Ring::create` rejects a layout whose
        // `used` end exceeds the allocated `ring_len`, so these offsets lie
        // inside the block, are 4-byte aligned on the directly mapped block,
        // and `Ring::create` zero-initializes every header word before the
        // block becomes user-visible.
        unsafe { ptr::read_volatile(ptr) }
    }

    fn write_u32(&self, offset: usize, value: u32) {
        let ptr = self.base.wrapping_add(offset).cast::<u32>();
        // SAFETY: see `read_u32`; the header field is kernel-owned.
        unsafe { ptr::write_volatile(ptr, value) }
    }

    fn read_sqe(&self, index: u32) -> IoUringSqe {
        let offset = self.sqes_off + index as usize * IO_URING_SQE_SIZE;
        let ptr = self.base.wrapping_add(offset).cast::<IoUringSqe>();
        // SAFETY: the SQE array is inside the ring block and `index` is
        // bounded by `sq_entries`; the source is aligned user-shared memory.
        unsafe { ptr::read_volatile(ptr) }
    }

    fn write_cqe(&self, slot: usize, cqe: IoUringCqe) {
        let offset = self.cqes_off + slot * IO_URING_CQE_SIZE;
        let ptr = self.base.wrapping_add(offset).cast::<IoUringCqe>();
        // SAFETY: the CQE array is inside the ring block and `slot` is masked
        // to `cq_entries`; the destination is aligned user-shared memory.
        unsafe { ptr::write_volatile(ptr, cqe) }
    }
}
