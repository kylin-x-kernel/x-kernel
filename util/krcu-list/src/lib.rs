// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Forward traversal concurrent with serialized insertion and removal.
//!
//! Prepare an [`Entry`] outside writer exclusion, append it with
//! [`RcuList::push_back`], and iterate using an existing [`krcu::ReadScope`].
//! [`RcuList::remove_first`] withdraws one entry without waiting for readers;
//! its embedded callback releases it after a grace period. An iterator is not a
//! snapshot: concurrent additions/removals may or may not be observed.
//!
//! Mutation originates in task context. Readers may use task scopes or already
//! watched interrupt/NMI scopes. Removal requires a ready RCU service and a
//! payload destructor valid in its nonblocking callback context. `Send + Sync`
//! alone does not establish that destructor contract.
//!
//! Store `Arc<Action>` when a consumer must retain an action independently of
//! list membership. Removing it does not synchronously stop its execution.
//! List destruction releases still-linked entries in the dropping context;
//! previously removed entries remain owned by their independent callbacks.
//!
//! ```no_run
//! let actions = krcu_list::RcuList::new();
//! let entry = krcu_list::Entry::new(7_usize);
//! // SAFETY: initialized task context, no reentrant writer operation.
//! unsafe { actions.push_back(entry) };
//! krcu::with_read(|scope| {
//!     for action in actions.iter(scope) {
//!         assert_eq!(*action, 7);
//!     }
//! });
//! // SAFETY: sleepable watched task outside scopes/locks; primitive payload
//! // and the kernel allocator can be released without sleeping or unwinding.
//! let removed = unsafe { actions.remove_first(|action| *action == 7) };
//! assert_eq!(removed, Ok(true));
//! ```

#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use core::{
    marker::PhantomData,
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use kerrno::KResult;
use krcu::ReadScope;
use kspin::SpinNoPreempt;

mod node;
use node::Node;

/// A uniquely owned, unpublished entry. Preparation allocates once.
///
/// The allocation contains the payload, forward link and retirement callback.
/// Dropping an unused entry releases it immediately in the dropping context.
/// Publication consumes this handle; it cannot be inserted twice.
///
/// ```compile_fail,E0382
/// let list = krcu_list::RcuList::new();
/// let entry = krcu_list::Entry::new(1_usize);
/// // SAFETY: assume initialized non-reentrant task context.
/// unsafe { list.push_back(entry) };
/// // SAFETY: context cannot restore the consumed ownership.
/// unsafe { list.push_back(entry) };
/// ```
pub struct Entry<T: Send + Sync + 'static>(Box<Node<T>>);

impl<T: Send + Sync + 'static> Entry<T> {
    /// Allocates unpublished storage using the system's allocator policy.
    pub fn new(value: T) -> Self {
        Self(Box::new(Node::new(value)))
    }
}

struct Writers<T: Send + Sync + 'static> {
    tail: *mut Node<T>,
}

// SAFETY: only the list's writer guard accesses tail. It points into an owned
// allocation whose T is transferable and shareable, not into CPU-local storage.
unsafe impl<T: Send + Sync + 'static> Send for Writers<T> {}

/// An ordered collection whose readers never acquire the writer lock.
///
/// Each published entry owns one allocation. No mutable payload or detached
/// ownership is exposed while readers may still borrow it.
pub struct RcuList<T: Send + Sync + 'static> {
    head: AtomicPtr<Node<T>>,
    writers: SpinNoPreempt<Writers<T>>,
    ownership: PhantomData<Box<Node<T>>>,
}

impl<T: Send + Sync + 'static> RcuList<T> {
    /// Creates an empty list without allocation or a running RCU service.
    pub const fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
            writers: SpinNoPreempt::new(Writers {
                tail: ptr::null_mut(),
            }),
            ownership: PhantomData,
        }
    }

    /// Appends a prepared entry without allocating or waiting for a grace period.
    ///
    /// No service is required until removal; boot-time construction is allowed
    /// once the task/preemption facilities needed by the writer lock are ready.
    ///
    /// # Safety
    /// Call from task context, never IRQ, softirq or NMI. Do not reenter the same
    /// list's writer operations. Publication does not certify a later destructor
    /// context: removal must independently meet its callback contract.
    pub unsafe fn push_back(&self, entry: Entry<T>) {
        let mut writers = self.writers.lock();
        let raw = Box::into_raw(entry.0);
        let predecessor = if writers.tail.is_null() {
            &self.head
        } else {
            // SAFETY: writer exclusion keeps the linked tail allocation owned
            // by this list. Only the atomic link is accessed through this borrow.
            unsafe { &(*writers.tail).next }
        };
        predecessor.store(raw, Ordering::Release);
        writers.tail = raw;
    }

    /// Borrows a forward iterator for at most the list and read scope lifetimes.
    ///
    /// Reads allocate nothing and do not clone payload owners.
    ///
    /// ```compile_fail
    /// fn escape(list: &krcu_list::RcuList<usize>) -> &usize {
    ///     krcu::with_read(|scope| list.iter(scope).next().unwrap())
    /// }
    /// ```
    ///
    /// The list cannot be destroyed while a borrowed item remains in use:
    ///
    /// ```compile_fail,E0505
    /// let list = krcu_list::RcuList::<usize>::new();
    /// krcu::with_read(|scope| {
    ///     let item = list.iter(scope).next();
    ///     drop(list);
    ///     assert!(item.is_none());
    /// });
    /// ```
    pub fn iter<'a>(&'a self, _scope: &'a ReadScope<'_>) -> Iter<'a, T> {
        Iter {
            next: self.head.load(Ordering::Acquire),
            lifetime: PhantomData,
        }
    }

    /// Withdraws the first matching entry and schedules its deferred destruction.
    ///
    /// Returns false for no match. Success means unlinked, not reclaimed or no
    /// longer executing. The predicate runs under the list's writer lock.
    /// Sleepable watched task context outside read scopes and atomic exclusion
    /// is checked before mutation. A sleepable consumer management mutex may
    /// serialize the surrounding transaction.
    ///
    /// # Errors
    /// RCU admission errors are returned before invoking the predicate or changing
    /// the list. An impossible post-withdrawal rejection retains storage and
    /// diagnoses the violated service/context invariant, never frees it early.
    ///
    /// # Panics
    /// Panics if submission is rejected after successful admission and unlink,
    /// indicating a violated permanent-service or execution-context invariant.
    ///
    /// # Safety
    /// Hold no scheduler/RCU bookkeeping lock. The predicate must not block,
    /// allocate, reenter this list's writers, or leave execution context changed.
    /// The removed T and its entire release chain
    /// must be nonblocking, nonunwinding and safe in RCU callback context.
    pub unsafe fn remove_first(&self, mut predicate_fn: impl FnMut(&T) -> bool) -> KResult<bool> {
        krcu::preflight_callback_submission()?;
        let raw = self.withdraw(&mut predicate_fn);
        if raw.is_null() {
            return Ok(false);
        }
        // SAFETY: withdrawal transferred this allocation's sole retirement
        // right; preflight succeeded and the writer lock is released. The caller
        // certifies the callback-safe release chain and predicate context.
        unsafe { Node::retire(raw) };
        Ok(true)
    }

    // Acquire this list's exclusion here, so callers cannot supply a guard for
    // another list. The returned retirement right never retains the guard.
    fn withdraw(&self, predicate_fn: &mut impl FnMut(&T) -> bool) -> *mut Node<T> {
        let mut writers = self.writers.lock();
        let mut previous: *mut Node<T> = ptr::null_mut();
        let mut link = &self.head;
        let mut raw = link.load(Ordering::Acquire);
        while !raw.is_null() {
            // SAFETY: the writer guard prevents unlink of this currently linked
            // node; the list retains its allocation. Readers use shared borrows.
            let node = unsafe { &*raw };
            let next = node.next.load(Ordering::Acquire);
            if predicate_fn(&node.value) {
                link.store(next, Ordering::Release);
                if writers.tail == raw {
                    writers.tail = previous;
                }
                // Old readers may still follow this node, including after its
                // successor is also removed. Do not reset node.next here.
                return raw;
            }
            previous = raw;
            link = &node.next;
            raw = next;
        }
        ptr::null_mut()
    }
}

impl<T: Send + Sync + 'static> Default for RcuList<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + Sync + 'static> Drop for RcuList<T> {
    fn drop(&mut self) {
        let mut raw = *self.head.get_mut();
        while !raw.is_null() {
            // SAFETY: exclusive list destruction excludes outstanding iterator
            // borrows and writers. Linked nodes have not been submitted to RCU.
            let node = unsafe { Box::from_raw(raw) };
            raw = node.next.load(Ordering::Relaxed);
            drop(node);
        }
    }
}

/// Forward iterator tied to a list and a protected scope; not transferable.
pub struct Iter<'a, T: Send + Sync + 'static> {
    next: *mut Node<T>,
    lifetime: PhantomData<&'a RcuList<T>>,
}

impl<'a, T: Send + Sync + 'static> Iterator for Iter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next.is_null() {
            return None;
        }
        // SAFETY: iter ties this cursor to a read scope and list borrow. Unlink
        // preserves next; retirement cannot free any reachable old node until
        // this scope ends. Acquire publication exposes initialized payloads.
        let node = unsafe { &*self.next };
        self.next = node.next.load(Ordering::Acquire);
        Some(&node.value)
    }
}

#[cfg(test)]
#[path = "../tests/host/list.rs"]
mod tests;

unittest::path_mod_test!(tests_kernel, "../ut-tests/lifecycle.rs");
