// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared FIFO/RR linkage and checked constant-time removal.

use alloc::sync::Arc;
use core::{
    mem::ManuallyDrop,
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use intrusive_collections::{
    Adapter, DefaultLinkOps, DefaultPointerOps, LinkedList, LinkedListAtomicLink,
};

static NEXT_QUEUE_ID: AtomicUsize = AtomicUsize::new(1);

pub(crate) struct ReadyLinks {
    link: LinkedListAtomicLink,
    owner: AtomicUsize,
}

impl ReadyLinks {
    pub(crate) const fn new() -> Self {
        Self {
            link: LinkedListAtomicLink::new(),
            owner: AtomicUsize::new(0),
        }
    }
}

/// Identifies the one private linkage field in an entity.
///
/// # Safety
/// OFFSET must be the offset of the ReadyLinks returned by ready_links in Self.
/// That field must not be exposed or mutated outside this module's queue API.
pub(crate) unsafe trait ReadyItem: Sized {
    const OFFSET: usize;
    fn ready_links(&self) -> &ReadyLinks;
}

struct ReadyAdapter<T: ReadyItem> {
    link_ops: <LinkedListAtomicLink as DefaultLinkOps>::Ops,
    pointer_ops: DefaultPointerOps<Arc<T>>,
}

impl<T: ReadyItem> ReadyAdapter<T> {
    const NEW: Self = Self {
        link_ops: <LinkedListAtomicLink as DefaultLinkOps>::NEW,
        pointer_ops: DefaultPointerOps::new(),
    };
    const OFFSET: usize = T::OFFSET + core::mem::offset_of!(ReadyLinks, link);
}

// SAFETY: ReadyItem supplies the exact embedded field offset. Both projections
// are inverses within the same live Arc allocation; the library owns Arc changes.
unsafe impl<T: ReadyItem> Adapter for ReadyAdapter<T> {
    type LinkOps = <LinkedListAtomicLink as DefaultLinkOps>::Ops;
    type PointerOps = DefaultPointerOps<Arc<T>>;

    /// # Safety
    /// link must be a live link previously projected by this adapter.
    unsafe fn get_value(&self, link: NonNull<LinkedListAtomicLink>) -> *const T {
        // SAFETY: the adapter contract provides our field in a live T allocation.
        unsafe { link.as_ptr().cast::<u8>().sub(Self::OFFSET).cast() }
    }

    /// # Safety
    /// value must point to a live initialized T.
    unsafe fn get_link(&self, value: *const T) -> NonNull<LinkedListAtomicLink> {
        // SAFETY: ReadyItem guarantees OFFSET names the initialized aligned link.
        unsafe { NonNull::new_unchecked(value.cast::<u8>().add(Self::OFFSET).cast_mut().cast()) }
    }

    fn link_ops(&self) -> &Self::LinkOps {
        &self.link_ops
    }

    fn link_ops_mut(&mut self) -> &mut Self::LinkOps {
        &mut self.link_ops
    }

    fn pointer_ops(&self) -> &Self::PointerOps {
        &self.pointer_ops
    }
}

pub(crate) struct ReadyList<T: ReadyItem> {
    list: LinkedList<ReadyAdapter<T>>,
    id: usize,
}

impl<T: ReadyItem> ReadyList<T> {
    pub(crate) const fn new() -> Self {
        Self {
            list: LinkedList::new(ReadyAdapter::NEW),
            id: 0,
        }
    }

    fn ensure_identity(&mut self) {
        if self.id == 0 {
            self.id = NEXT_QUEUE_ID
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .expect("ready queue identity exhausted");
        }
    }

    pub(crate) fn push_back(&mut self, task: Arc<T>) {
        self.ensure_identity();
        self.list.push_back(task);
        // AtomicLink acquires exclusive linkage before publication. Assign only
        // after success, so a rejected duplicate cannot overwrite its owner.
        self.list
            .back()
            .get()
            .unwrap()
            .ready_links()
            .owner
            .store(self.id, Ordering::Relaxed);
    }

    pub(crate) fn push_front(&mut self, task: Arc<T>) {
        self.ensure_identity();
        self.list.push_front(task);
        self.list
            .front()
            .get()
            .unwrap()
            .ready_links()
            .owner
            .store(self.id, Ordering::Relaxed);
    }

    pub(crate) fn pop_front(&mut self) -> Option<Arc<T>> {
        // Clear before the library releases its atomic link: a concurrent
        // insertion may acquire that link immediately after removal.
        self.list
            .front()
            .get()?
            .ready_links()
            .owner
            .store(0, Ordering::Relaxed);
        self.list.pop_front()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub(crate) fn remove(&mut self, task: &Arc<T>) -> Option<Arc<T>> {
        if self.id == 0 || task.ready_links().owner.load(Ordering::Relaxed) != self.id {
            return None;
        }
        task.ready_links().owner.store(0, Ordering::Relaxed);
        // SAFETY: the checked nonzero never-reused identity establishes membership in this
        // list, whose Arc keeps task alive. Exclusive self prevents concurrent
        // unlink; private links and AtomicLink prevent foreign-list mutation.
        unsafe { self.list.cursor_mut_from_ptr(Arc::as_ptr(task)).remove() }
    }

    pub(crate) fn steal(&mut self, mut want: impl FnMut(&Arc<T>) -> bool) -> Option<Arc<T>> {
        let mut cursor = self.list.front_mut();
        while let Some(pointer) = cursor.as_cursor().get_ptr() {
            // SAFETY: this cursor's list owns an Arc converted by DefaultPointerOps
            // into this exact data pointer. The exclusive cursor keeps it alive
            // throughout want; want receives only &Arc and cannot consume it.
            // ManuallyDrop borrows that strong count without releasing it, even
            // on panic. The view is not used after cursor.remove transfers the Arc.
            let task = ManuallyDrop::new(unsafe { Arc::from_raw(pointer.as_ptr()) });
            if want(&task) {
                task.ready_links().owner.store(0, Ordering::Relaxed);
                return cursor.remove();
            }
            cursor.move_next();
        }
        None
    }
}

impl<T: ReadyItem> Drop for ReadyList<T> {
    fn drop(&mut self) {
        // Surviving external Arcs must be reusable after the queue is dropped.
        while self.pop_front().is_some() {}
    }
}
