// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[repr(C)]
pub(super) struct SelfReposting {
    pub(super) node: CallbackNode,
    pub(super) queues: Arc<[CallbackQueue; 1]>,
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) drops: Arc<AtomicUsize>,
    pub(super) remaining: usize,
}

impl SelfReposting {
    pub(super) fn enqueue(self: Box<Self>) {
        let queues = self.queues.clone();
        let object = Box::into_raw(self);
        // SAFETY: raw field projection preserves the original Box provenance.
        let node = unsafe { &raw mut (*object).node };
        // SAFETY: this fixture owns the unqueued/detached node exclusively;
        // its original allocation survives until the next invocation.
        let node = unsafe { CallbackNode::from_raw(node) };
        // SAFETY: unpublished fixture, no locks held. The test drives every GP
        // and drains all reposts before releasing its coordinator and queues.
        unsafe { queues[0].submit(node) };
    }

    /// # Safety
    /// `node` is the unique detached projection transferred by enqueue. Recover
    /// its original Box once; requeue or destroy it without subsequent access.
    pub(super) unsafe fn invoke(node: *mut CallbackNode) {
        // SAFETY: enqueue preserves allocation provenance; the actual field
        // offset restores the original Box base without a layout assumption.
        let object = unsafe { node.byte_sub(core::mem::offset_of!(Self, node)) }.cast::<Self>();
        // SAFETY: the detached callback exclusively owns the original allocation.
        let mut object = unsafe { Box::from_raw(object) };
        object.calls.fetch_add(1, Ordering::Relaxed);
        if object.remaining != 0 {
            object.remaining -= 1;
            object.enqueue();
        }
    }
}

impl Drop for SelfReposting {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}
