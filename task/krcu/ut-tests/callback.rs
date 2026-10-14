// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicUsize, Ordering};

use intrusive_collections::{LinkedList, container_of};
#[cfg(not(test))]
use unittest::{assert, assert_eq};

use super::*;

// A nonzero node offset and over-alignment exercise enclosing-object recovery,
// rather than relying on a callback header being the allocation's first field.
#[repr(C, align(128))]
struct OwnedObject {
    value: usize,
    callback: CallbackNode,
    invoked: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

impl OwnedObject {
    fn new(invoked: Arc<AtomicUsize>, dropped: Arc<AtomicUsize>) -> Box<Self> {
        Box::new(Self {
            value: 7,
            callback: CallbackNode::new(Self::release),
            invoked,
            dropped,
        })
    }

    fn into_node(self: Box<Self>) -> UnsafeRef<CallbackNode> {
        let object = Box::into_raw(self);
        // SAFETY: object is an initialized Box transferred to this fixture's
        // retirement path. Raw field projection preserves allocation provenance.
        let node = unsafe { &raw mut (*object).callback };
        // SAFETY: only this handle owns invocation; the Box remains allocated
        // and fixed until release reconstructs it. Its captures are Send.
        unsafe { CallbackNode::from_raw(node) }
    }

    /// # Safety
    /// `node` must be this object's unique detached callback from into_node;
    /// no object references may survive this terminal invocation.
    unsafe fn release(node: *mut CallbackNode) {
        // SAFETY: into_node preserved the enclosing Box provenance; the actual
        // field offset restores its address and original over-aligned layout.
        let object = unsafe { container_of!(node, Self, callback) }.cast_mut();
        // SAFETY: this is the original Box pointer, uniquely transferred by
        // into_node; no references survive the detached callback invocation.
        let object = unsafe { Box::from_raw(object) };
        object.invoked.fetch_add(object.value, Ordering::Relaxed);
        drop(object);
    }
}

impl Drop for OwnedObject {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn unqueued_embedded_node_does_not_invoke_on_owner_drop() {
    let invoked = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    drop(OwnedObject::new(invoked.clone(), dropped.clone()));
    assert_eq!(invoked.load(Ordering::Relaxed), 0);
    assert_eq!(dropped.load(Ordering::Relaxed), 1);
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn embedded_callback_releases_its_original_object() {
    let invoked = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let object = OwnedObject::new(invoked.clone(), dropped.clone());
    let address = core::ptr::from_ref(&object.callback);
    let mut queue = LinkedList::new(CallbackAdapter::NEW);
    queue.push_back(object.into_node());
    let retained = invoked.load(Ordering::Relaxed) == 0 && dropped.load(Ordering::Relaxed) == 0;
    let node = queue.pop_front();
    let same_address = node
        .as_ref()
        .is_some_and(|node| core::ptr::eq(core::ptr::from_ref(&**node), address));
    if let Some(node) = node {
        // SAFETY: unique detached fixture, no published data needs a GP.
        // release only updates atomics and frees its own object; no locks held.
        unsafe { CallbackNode::invoke(node) };
    }
    assert!(retained && same_address);
    assert!(queue.is_empty());
    assert_eq!(invoked.load(Ordering::Relaxed), 7);
    assert_eq!(dropped.load(Ordering::Relaxed), 1);
}

#[repr(C)]
struct BorrowedRequest<'a> {
    callback: CallbackNode,
    invoked: &'a AtomicUsize,
}

impl BorrowedRequest<'_> {
    /// # Safety
    /// `node` must be the detached callback of a live BorrowedRequest; the
    /// request and its counter must remain valid throughout this invocation.
    unsafe fn notify(node: *mut CallbackNode) {
        // SAFETY: the fixture retains stack storage and its counter through the
        // call; only the atomic counter is modified, and storage is not freed.
        let request = unsafe { container_of!(node, Self, callback) };
        // SAFETY: the containing request remains live and shared access changes
        // only its atomic counter, not its node or borrowed counter reference.
        let request = unsafe { &*request };
        let _ = request
            .invoked
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value * 10)
            });
    }
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn one_list_supports_owned_and_stack_backed_nodes() {
    let invoked = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let object = OwnedObject::new(invoked.clone(), dropped.clone());
    let mut request = BorrowedRequest {
        callback: CallbackNode::new(BorrowedRequest::notify),
        invoked: &invoked,
    };
    let request_ptr = &raw mut request;
    // SAFETY: request_ptr names initialized stack storage; field projection
    // preserves the original pointer for notify's enclosing-object access.
    let node_ptr = unsafe { &raw mut (*request_ptr).callback };
    // SAFETY: request is neither moved nor borrowed exclusively until the queue
    // is drained and notify returns. There is exactly one invocation handle.
    let borrowed = unsafe { CallbackNode::from_raw(node_ptr) };
    let mut queue = LinkedList::new(CallbackAdapter::NEW);
    queue.push_back(object.into_node());
    queue.push_back(borrowed);
    let mut count = 0;
    while let Some(node) = queue.pop_front() {
        // SAFETY: unique detached fixture with no published data or held locks.
        // Each callback obeys its own storage policy; both are nonblocking.
        unsafe { CallbackNode::invoke(node) };
        count += 1;
    }
    assert_eq!(count, 2);
    assert_eq!(invoked.load(Ordering::Relaxed), 70);
    assert_eq!(dropped.load(Ordering::Relaxed), 1);
    // This proves only the synchronous fixture's borrowed lifetime, not a
    // production waiter's notification-versus-return protocol.
    assert!(!request.callback.link.is_linked());
}
