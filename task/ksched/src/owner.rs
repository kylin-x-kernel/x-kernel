// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stable shared ownership for scheduler queues, independent of destruction policy.

use alloc::sync::Arc;
use core::{marker::PhantomData, ops::Deref};

use intrusive_collections::PointerOps;
use kfinalize::DeferredArc;

/// A stable shared owner accepted by scheduler queues.
///
/// # Safety
/// Cloning retains the same initialized value at a stable address. into_raw
/// transfers one count without releasing storage; from_raw recovers that count
/// exactly once and must not panic for a valid pointer. Raw pointers must retain
/// provenance through intrusive link-field projections and their inverses.
/// as_ptr must preserve the same allocation provenance as into_raw so it can
/// locate a linked node and recover the collection's separately retained count.
/// Dropping owners must be legal under the caller's run-queue lock and context.
pub unsafe trait SharedOwner<T>: Clone + Deref<Target = T> {
    /// Borrows the original allocation-provenance address without transferring
    /// an owner. A separate retained raw count is required for recovery.
    fn as_ptr(owner: &Self) -> *const T;

    /// Transfers one owner to a stable raw pointer without changing its count.
    fn into_raw(owner: Self) -> *const T;

    /// Recovers one transferred owner.
    ///
    /// # Safety
    /// pointer must preserve the provenance and outstanding ownership from
    /// into_raw. Each transferred owner may be recovered only once.
    unsafe fn from_raw(pointer: *const T) -> Self;

    /// Compares the identity of two owners.
    fn ptr_eq(left: &Self, right: &Self) -> bool {
        core::ptr::eq(Self::as_ptr(left), Self::as_ptr(right))
    }
}

// SAFETY: standard Arc provides stable shared allocation and count-preserving
// raw transfer. Its final destructor's execution context remains caller-owned.
unsafe impl<T> SharedOwner<T> for Arc<T> {
    fn as_ptr(owner: &Self) -> *const T {
        Arc::as_ptr(owner)
    }

    fn into_raw(owner: Self) -> *const T {
        Arc::into_raw(owner)
    }

    unsafe fn from_raw(pointer: *const T) -> Self {
        // SAFETY: SharedOwner's caller transfers a live original Arc raw owner.
        unsafe { Arc::from_raw(pointer) }
    }
}

// SAFETY: DeferredArc's raw interface preserves allocation provenance and one
// standard Arc count. Its payload stays stable until worker finalization. The
// caller must satisfy its documented release-context and lock requirements.
unsafe impl<T: Send + Sync + 'static> SharedOwner<T> for DeferredArc<T> {
    fn as_ptr(owner: &Self) -> *const T {
        DeferredArc::as_ptr(owner)
    }

    fn into_raw(owner: Self) -> *const T {
        DeferredArc::into_raw(owner)
    }

    unsafe fn from_raw(pointer: *const T) -> Self {
        // SAFETY: SharedOwner's caller retains the original payload provenance
        // and transfers the unconsumed count returned by DeferredArc::into_raw.
        unsafe { DeferredArc::from_raw(pointer) }
    }
}

pub(crate) struct OwnerPointerOps<T, Owner: SharedOwner<T>> {
    marker: PhantomData<(T, Owner)>,
}

impl<T, Owner: SharedOwner<T>> OwnerPointerOps<T, Owner> {
    pub(crate) const fn new() -> Self {
        Self {
            marker: PhantomData,
        }
    }
}

// SAFETY: SharedOwner establishes stable values and single-count raw transfer;
// the adapter preserves those pointers when projecting and reversing links.
unsafe impl<T, Owner: SharedOwner<T>> PointerOps for OwnerPointerOps<T, Owner> {
    type Pointer = Owner;
    type Value = T;

    unsafe fn from_raw(&self, pointer: *const T) -> Owner {
        // SAFETY: PointerOps requires a live pointer transferred by into_raw;
        // the intrusive collection returns that one count exactly once.
        unsafe { Owner::from_raw(pointer) }
    }

    fn into_raw(&self, owner: Owner) -> *const T {
        Owner::into_raw(owner)
    }
}
