// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Optional KArc ownership conversion for existing intrusive collections.

use core::marker::PhantomData;

use intrusive_collections::PointerOps;

use crate::KArc;

/// Zero-sized pointer operations for an intrusive adapter owning `KArc<T>`.
///
/// Available with the `intrusive-collections` feature. Set the concrete
/// adapter's PointerOps to this type; the adapter still defines the link-field
/// projections. Each inserted owner transfers one strong reference, recovered
/// by the collection on removal or destruction. Conversion neither allocates
/// nor releases a reference. Dropping a recovered owner may destroy T and free
/// storage, so callers must obey KArc's release-context requirements.
///
/// This does not implement a queue, checked cancellation, generation tracking,
/// synchronization or deferred destruction. In particular, is_linked alone does
/// not establish membership in the list passed to cursor_mut_from_ptr. Those
/// proofs belong to the concrete subsystem, not pointer conversion.
/// Library policies are unchanged: duplicate insertion can panic and fast_clear
/// intentionally abandons ownership. This adapter does not guarantee complete
/// list cleanup if a payload destructor unwinds.
pub struct KArcPointerOps<T>(PhantomData<KArc<T>>);

impl<T> Copy for KArcPointerOps<T> {}

impl<T> Clone for KArcPointerOps<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> KArcPointerOps<T> {
    /// Creates pointer operations without allocating or acquiring a lock.
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T> Default for KArcPointerOps<T> {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: KArc transfers a stable allocation pointer and exactly one strong
// reference. Its matching recovery preserves allocator authority and cannot
// panic for a valid, unrecovered pointer supplied by PointerOps's contract.
unsafe impl<T> PointerOps for KArcPointerOps<T> {
    type Pointer = KArc<T>;
    type Value = T;

    unsafe fn from_raw(&self, pointer: *const T) -> KArc<T> {
        // SAFETY: PointerOps requires the original, unrecovered into_raw result.
        unsafe { KArc::from_raw(pointer) }
    }

    fn into_raw(&self, owner: KArc<T>) -> *const T {
        KArc::into_raw(owner)
    }
}
