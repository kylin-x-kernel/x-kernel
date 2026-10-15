// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Caller-owned callback nodes; linkage alone never grants permission to reclaim.

use intrusive_collections::{LinkedListAtomicLink, UnsafeRef, intrusive_adapter};

/// Intrusive callback state embedded in caller-owned storage.
///
/// The core neither allocates nor frees this node. Its callback decides whether
/// to release the enclosing object or retain it. No payload layout is imposed.
pub struct CallbackNode {
    link: LinkedListAtomicLink,
    invoke_fn: unsafe fn(*mut CallbackNode),
}

intrusive_adapter!(pub(super) CallbackAdapter = UnsafeRef<CallbackNode>: CallbackNode {
    link => LinkedListAtomicLink
});

impl CallbackNode {
    /// Prepares an unlinked node without allocating storage.
    pub const fn new(invoke_fn: unsafe fn(*mut Self)) -> Self {
        Self {
            link: LinkedListAtomicLink::new(),
            invoke_fn,
        }
    }

    /// Borrows a node for private queue bookkeeping without allocating.
    ///
    /// Dropping the returned handle does not invoke or free the node. This is
    /// not a submission API and does not establish grace-period eligibility.
    ///
    /// # Safety
    ///
    /// `ptr` must point to an initialized node at a stable address. It must not
    /// already be queued, detached for execution or awaiting invocation. Retain
    /// its storage, callback code and referenced resources through invocation;
    /// do not move, free or exclusively borrow storage while a handle exists.
    /// Preserve one logical invocation owner, even if list inspection borrows
    /// the node temporarily. The callback must match the containing object's
    /// layout, provenance and ownership, including any cross-CPU transfer.
    /// If it recovers the enclosing object, derive `ptr` from that object's raw
    /// pointer, not from a shared reference restricted to the node field.
    pub(super) unsafe fn from_raw(ptr: *mut Self) -> UnsafeRef<Self> {
        debug_assert!(!ptr.is_null(), "RCU callback node is null");
        // SAFETY: the caller provides initialized, stable storage and retains
        // it until the unique invocation consumes all list/inspection handles.
        let node = unsafe { UnsafeRef::from_raw(ptr) };
        debug_assert!(!node.link.is_linked(), "RCU callback is already linked");
        node
    }

    /// Invokes a detached node without assuming how its storage is owned.
    ///
    /// # Safety
    ///
    /// The argument must be the sole remaining handle from Self::from_raw and
    /// be removed from every list. Its required grace period and independent
    /// retirement obligations must have completed. No queue/node/coordinator
    /// lock may be held. Invocation and resource cleanup must not block, unwind
    /// or wait for this RCU engine, and the context must permit them. The
    /// callback may free its entire containing object; do not access the node
    /// afterward without a separate proof that its storage remains valid.
    pub(super) unsafe fn invoke(node: UnsafeRef<Self>) {
        debug_assert!(!node.link.is_linked(), "RCU callback is still linked");
        let invoke_fn = node.invoke_fn;
        let ptr = UnsafeRef::into_raw(node);
        // SAFETY: the caller certifies unique detached ownership, eligibility
        // and the callback's storage/context contract. Consuming the handle
        // preserves its provenance; no node access follows the invocation.
        unsafe { invoke_fn(ptr) };
    }
}

#[cfg(any(test, unittest))]
#[path = "../ut-tests/callback.rs"]
mod tests;
