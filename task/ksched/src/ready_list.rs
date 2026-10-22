// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared intrusive FIFO linkage and checked constant-time removal.
//!
//! A task records the address of the [`ReadyList`] that linked it. Removal
//! checks that address before `cursor_mut_from_ptr`, so a task queued on
//! another CPU or another priority is left alone. `is_linked` is not enough.

use core::{
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use intrusive_collections::{Adapter, DefaultLinkOps, LinkedList, LinkedListAtomicLink};

use crate::owner::{OwnerPointerOps, SharedOwner};

pub(crate) struct ReadyLinks {
    link: LinkedListAtomicLink,
    /// Address of the [`ReadyList`] that linked this task, or `0` if detached.
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
///
/// `OFFSET` must be the byte offset of the [`ReadyLinks`] returned by
/// [`Self::ready_links`]. That field must stay private. Only [`ReadyList`]
/// may read or write the link and the owner word.
pub(crate) unsafe trait ReadyItem: Sized {
    /// Offset of the [`ReadyLinks`] field inside `Self`.
    const OFFSET: usize;

    /// The linkage [`ReadyList`] uses for this entity.
    fn ready_links(&self) -> &ReadyLinks;
}

struct ReadyAdapter<T: ReadyItem, Owner: SharedOwner<T>> {
    link_ops: <LinkedListAtomicLink as DefaultLinkOps>::Ops,
    pointer_ops: OwnerPointerOps<T, Owner>,
}

impl<T: ReadyItem, Owner: SharedOwner<T>> ReadyAdapter<T, Owner> {
    const NEW: Self = Self {
        link_ops: <LinkedListAtomicLink as DefaultLinkOps>::NEW,
        pointer_ops: OwnerPointerOps::new(),
    };
    const OFFSET: usize = T::OFFSET + core::mem::offset_of!(ReadyLinks, link);
}

// SAFETY: `ReadyItem::OFFSET` names the private `ReadyLinks` field.
// `get_link` and `get_value` invert that same field offset inside the live
// entity allocation. `OwnerPointerOps` owns each list's reference; the adapter
// does not change the strong count.
unsafe impl<T: ReadyItem, Owner: SharedOwner<T>> Adapter for ReadyAdapter<T, Owner> {
    type LinkOps = <LinkedListAtomicLink as DefaultLinkOps>::Ops;
    type PointerOps = OwnerPointerOps<T, Owner>;

    /// # Safety
    ///
    /// `link` must have been produced by this adapter's `get_link` on a
    /// still-live `T`.
    unsafe fn get_value(&self, link: NonNull<LinkedListAtomicLink>) -> *const T {
        // SAFETY: Adapter requires a link previously projected by this
        // adapter. Subtracting the same field offset recovers the live
        // owning allocation. The link is aligned because it is a real field.
        unsafe { link.as_ptr().cast::<u8>().sub(Self::OFFSET).cast() }
    }

    /// # Safety
    ///
    /// `value` must point to a live, initialized `T`.
    unsafe fn get_link(&self, value: *const T) -> NonNull<LinkedListAtomicLink> {
        // SAFETY: Adapter supplies a live entity. OFFSET selects its
        // aligned, initialized `LinkedListAtomicLink` without creating an
        // exclusive entity reference.
        let link = unsafe { value.cast::<u8>().add(Self::OFFSET).cast_mut().cast() };
        // SAFETY: A field inside the caller's live allocation cannot be null.
        unsafe { NonNull::new_unchecked(link) }
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

pub(crate) struct ReadyList<T: ReadyItem, Owner: SharedOwner<T>> {
    list: LinkedList<ReadyAdapter<T, Owner>>,
}

impl<T: ReadyItem, Owner: SharedOwner<T>> ReadyList<T, Owner> {
    pub(crate) const fn new() -> Self {
        Self {
            list: LinkedList::new(ReadyAdapter::NEW),
        }
    }

    /// Address of this list. Stable while the owning `Scheduler` stays put,
    /// which is true once a run queue has published it.
    fn identity(&self) -> usize {
        core::ptr::from_ref(self) as usize
    }

    /// Link `task` at the back, or the front when `front` is set.
    ///
    /// Returns false when `task` is already on a list. Its owner stays with
    /// that list.
    fn push(&mut self, task: Owner, front: bool) -> bool {
        if task.ready_links().link.is_linked() {
            return false;
        }
        task.ready_links()
            .owner
            .store(self.identity(), Ordering::Relaxed);
        if front {
            self.list.push_front(task);
        } else {
            self.list.push_back(task);
        }
        true
    }

    pub(crate) fn push_back(&mut self, task: Owner) -> bool {
        self.push(task, false)
    }

    pub(crate) fn push_front(&mut self, task: Owner) -> bool {
        self.push(task, true)
    }

    pub(crate) fn pop_front(&mut self) -> Option<Owner> {
        let task = self.list.pop_front()?;
        task.ready_links().owner.store(0, Ordering::Relaxed);
        Some(task)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub(crate) fn remove(&mut self, task: &Owner) -> Option<Owner> {
        if task.ready_links().owner.load(Ordering::Relaxed) != self.identity() {
            return None;
        }
        task.ready_links().owner.store(0, Ordering::Relaxed);
        // SAFETY: `cursor_mut_from_ptr` requires `task` to be linked in this
        // list. `owner` is this list's address, stored only when the link was
        // free, and cleared before the link is released.
        // Exclusive `&mut self` is the only mutator of this list. The owner's
        // original pointer preserves allocation provenance for raw recovery;
        // a narrowed &T pointer is insufficient for an owner with a prefix.
        unsafe { self.list.cursor_mut_from_ptr(Owner::as_ptr(task)).remove() }
    }

    pub(crate) fn steal(&mut self, mut want: impl FnMut(&T) -> bool) -> Option<Owner> {
        let mut cursor = self.list.front_mut();
        while let Some(task) = cursor.get() {
            if want(task) {
                task.ready_links().owner.store(0, Ordering::Relaxed);
                return cursor.remove();
            }
            cursor.move_next();
        }
        None
    }
}

impl<T: ReadyItem, Owner: SharedOwner<T>> Drop for ReadyList<T, Owner> {
    fn drop(&mut self) {
        // Surviving external Arcs must be reusable after the queue is dropped.
        while self.pop_front().is_some() {}
    }
}
