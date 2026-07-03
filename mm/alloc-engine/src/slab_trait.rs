// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Slab allocator traits and size-class definitions.
//!
//! These trait definitions are compatible with the buddy-slab-allocator
//! crate's API surface used by x-kernel.

use core::{alloc::Layout, ptr::NonNull};

use crate::AllocResult;

// ---------------------------------------------------------------------------
// SizeClass
// ---------------------------------------------------------------------------

/// Number of distinct size classes.
pub const SIZE_CLASS_COUNT: usize = 9;

/// Maximum object size handled by the slab.
pub const SLAB_MAX_SIZE: usize = 2048;

/// Ordered table of object sizes for all classes.
const CLASS_SIZES: [usize; SIZE_CLASS_COUNT] = [8, 16, 32, 64, 128, 256, 512, 1024, 2048];

/// Fixed set of object sizes served by the slab allocator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SizeClass {
    /// 8-byte objects.
    Bytes8    = 0,
    /// 16-byte objects.
    Bytes16   = 1,
    /// 32-byte objects.
    Bytes32   = 2,
    /// 64-byte objects.
    Bytes64   = 3,
    /// 128-byte objects.
    Bytes128  = 4,
    /// 256-byte objects.
    Bytes256  = 5,
    /// 512-byte objects.
    Bytes512  = 6,
    /// 1024-byte objects.
    Bytes1024 = 7,
    /// 2048-byte objects.
    Bytes2048 = 8,
}

impl SizeClass {
    /// All size classes in ascending order.
    pub const ALL: [SizeClass; SIZE_CLASS_COUNT] = [
        SizeClass::Bytes8,
        SizeClass::Bytes16,
        SizeClass::Bytes32,
        SizeClass::Bytes64,
        SizeClass::Bytes128,
        SizeClass::Bytes256,
        SizeClass::Bytes512,
        SizeClass::Bytes1024,
        SizeClass::Bytes2048,
    ];
    /// Number of distinct size classes.
    pub const COUNT: usize = SIZE_CLASS_COUNT;

    /// Select the smallest size class that can satisfy `layout`.
    ///
    /// Returns `None` if the requested size or alignment exceeds
    /// [`SLAB_MAX_SIZE`].
    pub fn from_layout(layout: Layout) -> Option<SizeClass> {
        let size = layout.size().max(layout.align());
        if size > SLAB_MAX_SIZE {
            return None;
        }
        for (i, &class_size) in CLASS_SIZES.iter().enumerate() {
            if size <= class_size {
                return Some(SizeClass::ALL[i]);
            }
        }
        None
    }

    /// Object size in bytes.
    pub const fn size(self) -> usize {
        CLASS_SIZES[self as usize]
    }

    /// Array index (0-based).
    pub const fn index(self) -> usize {
        self as usize
    }

    /// How many pages are needed for a single slab of this class.
    ///
    /// Smaller classes use 1 page; larger classes may use more to
    /// amortise the per-page header overhead.
    pub const fn slab_pages(self, page_size: usize) -> usize {
        let obj_size = self.size();
        if obj_size <= 256 {
            1
        } else if obj_size <= 1024 {
            2
        } else {
            let v = 16 * page_size / (obj_size * 8);
            if v < 4 { v } else { 4 }
        }
    }
}

// ---------------------------------------------------------------------------
// Allocation / deallocation result enums
// ---------------------------------------------------------------------------

/// Result of a slab allocation attempt.
pub enum SlabAllocResult {
    /// Object successfully allocated.
    Allocated(NonNull<u8>),
    /// The slab cache for this size class has no free objects.
    /// The caller should allocate `pages` pages, call
    /// [`SlabTrait::add_slab`], and retry.
    NeedsSlab {
        /// The size class that needs more pages.
        size_class: SizeClass,
        /// Number of pages to allocate.
        pages: usize,
    },
}

/// Result of a slab deallocation (local path).
pub enum SlabDeallocResult {
    /// Object freed, nothing else to do.
    Done,
    /// The slab page at `base` became empty and should be returned
    /// to the buddy allocator.
    FreeSlab {
        /// Virtual address of the empty slab page(s).
        base: usize,
        /// Number of pages.
        pages: usize,
    },
}

/// Result of a pool-mediated slab deallocation.
pub enum SlabPoolDeallocResult {
    /// Object freed on the local CPU path.
    Done,
    /// Object was queued onto the owner's remote-free list.
    RemoteQueued,
    /// The slab page at `base` became empty and should be returned
    /// to the buddy allocator.
    FreeSlab {
        /// Virtual address of the empty slab page(s).
        base: usize,
        /// Number of pages.
        pages: usize,
    },
}

// ---------------------------------------------------------------------------
// SlabTrait / SlabPoolTrait
// ---------------------------------------------------------------------------

/// Object-safe slab interface.
///
/// Each CPU (or logical slab context) provides one implementation.
pub trait SlabTrait: Sync {
    /// Logical CPU id this slab belongs to.
    fn cpu_id(&self) -> usize;

    /// Page size used by this slab.
    fn page_size(&self) -> usize;

    /// Allocate one object.
    fn alloc(&self, layout: Layout) -> AllocResult<SlabAllocResult>;

    /// Register a freshly allocated slab page.
    fn add_slab(&self, size_class: SizeClass, base: usize, bytes: usize);

    /// Free an object on the owner CPU path.
    fn dealloc_local(&self, ptr: NonNull<u8>, layout: Layout) -> SlabDeallocResult;
}

/// Object-safe slab-pool interface.
pub trait SlabPoolTrait: Sync {
    /// Return the slab belonging to the current CPU.
    fn current_slab(&self) -> &dyn SlabTrait;

    /// Return the owner slab for the given CPU.
    fn owner_slab(&self, cpu_idx: usize) -> &dyn SlabTrait;

    /// Logical CPU id of the current CPU.
    fn current_cpu_id(&self) -> usize {
        self.current_slab().cpu_id()
    }

    /// Allocate one object from the current CPU's slab.
    fn alloc(&self, layout: Layout) -> AllocResult<SlabAllocResult> {
        self.current_slab().alloc(layout)
    }

    /// Register a freshly allocated slab page in the current CPU's slab.
    fn add_slab(&self, size_class: SizeClass, base: usize, bytes: usize) {
        self.current_slab().add_slab(size_class, base, bytes)
    }

    /// Free an object, routing to local or remote slab as needed.
    fn dealloc(&self, ptr: NonNull<u8>, layout: Layout, owner_cpu: usize) -> SlabPoolDeallocResult {
        if owner_cpu == self.current_cpu_id() {
            match self.current_slab().dealloc_local(ptr, layout) {
                SlabDeallocResult::Done => SlabPoolDeallocResult::Done,
                SlabDeallocResult::FreeSlab { base, pages } => {
                    SlabPoolDeallocResult::FreeSlab { base, pages }
                }
            }
        } else {
            // Remote-free path: not supported in this lightweight version.
            // This matches x-kernel's current stub implementation.
            let _ = ptr;
            let _ = layout;
            let _ = owner_cpu;
            SlabPoolDeallocResult::RemoteQueued
        }
    }
}
