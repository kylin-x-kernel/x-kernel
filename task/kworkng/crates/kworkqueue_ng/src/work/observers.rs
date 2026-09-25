// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Prepared, single-use completion observations without subject back-pointers.
//!
//! Prepare and register wakeups outside management locks. Attach and take require
//! exclusive access under the subject's protection. After resource cleanup and
//! final subject access, complete the detached chain outside management locks:
//! completion may invoke arbitrary waiter wakeups and release observer storage.

use alloc::sync::Arc;
use core::task::Waker;

use intrusive_collections::{LinkedList, intrusive_adapter, linked_list::AtomicLink};
use kpoll::{Completion, PollRegisterError, PollRegistration};

/// A permanent completion receipt; it never retains Work, Queue or slot storage.
pub(crate) struct CompletionObserver {
    link: AtomicLink,
    completion: Completion,
}

/// Unique right to attach a newly prepared observer to exactly one subject.
///
/// Unlike the receipt, this token is not cloneable. Consuming it on attachment
/// prevents safe callers from inserting the same intrusive link twice.
pub(crate) struct PreparedObserver(Arc<CompletionObserver>);

impl CompletionObserver {
    /// Allocates a fresh receipt and its single-use attachment token.
    pub(crate) fn prepare() -> (Arc<Self>, PreparedObserver) {
        let observer = Arc::new(Self {
            link: AtomicLink::new(),
            completion: Completion::new(),
        });
        let prepared = PreparedObserver(observer.clone());
        (observer, prepared)
    }

    /// Checks permanent completion without resetting or consuming it.
    pub(crate) fn is_completed(&self) -> bool {
        self.completion.is_completed()
    }

    /// Registers a wakeup before attachment; allocation is permitted here.
    ///
    /// Keep the registration while waiting and recheck completion after
    /// registration to cover completion racing with waiter preparation.
    ///
    /// # Errors
    /// Returns the poll provider's storage or identity-exhaustion error.
    pub(crate) fn register(&self, waker: &Waker) -> Result<PollRegistration, PollRegisterError> {
        self.completion.register_owned(waker)
    }
}

// The adapter retains Arc ownership of each stable node; AtomicLink supports
// sharing the immutable receipt while the exclusively accessed list owns links.
intrusive_adapter!(ObserverAdapter = Arc<CompletionObserver>: CompletionObserver {
    link => AtomicLink
});

/// A subject's observer chain, transferable without walking or reallocating nodes.
pub(crate) struct CompletionObservers(LinkedList<ObserverAdapter>);

impl CompletionObservers {
    pub(crate) fn new() -> Self {
        Self(LinkedList::new(ObserverAdapter::new()))
    }

    /// Transfers the unique prepared node into this subject without invoking wakes.
    pub(crate) fn attach(&mut self, observer: PreparedObserver) {
        self.0.push_back(observer.0);
    }

    /// Moves the entire chain in constant time, leaving this subject empty.
    pub(crate) fn take(&mut self) -> Self {
        Self(self.0.take())
    }

    /// Publishes terminal completion after cleanup and all borrowed subject access.
    ///
    /// Must run outside management locks. Dropping a chain instead does not
    /// report success: only this explicit terminal action completes receipts.
    pub(crate) fn complete(mut self) {
        while let Some(observer) = self.0.pop_front() {
            observer.completion.complete_all();
        }
    }
}

#[cfg(unittest)]
mod tests;
