// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Controlled owning consumer of the task-reader and synchronous-wait protocols.
//! Not exported: complete context integration is required before safe admission.

use alloc::boxed::Box;
use core::{
    marker::PhantomData,
    sync::atomic::{AtomicPtr, Ordering},
};

use kerrno::KError;
use krcu_core::ReadGuard;

use super::synchronize::PreparedGracePeriodWait;

/// Owns the currently published allocation; replacements privately retain old ones.
struct Rcu<T> {
    value: AtomicPtr<T>,
    _owner: PhantomData<Box<T>>,
}

// SAFETY: shared readers only receive &T, hence Sync. A replacement can return
// an allocation published by another task, hence Send even for shared access.
// Atomic exchange uniquely assigns each old owner; no mutable borrow is exposed.
unsafe impl<T: Send + Sync> Sync for Rcu<T> {}

impl<T> Rcu<T> {
    fn new(value: Box<T>) -> Self {
        Self {
            value: AtomicPtr::new(Box::into_raw(value)),
            _owner: PhantomData,
        }
    }

    /// Borrows initialized data against both the container and active read scope.
    ///
    /// # Safety
    /// The token must belong to the current task's reader in watched kernel
    /// execution: either a task scope or a strictly nested interrupt scope that
    /// cannot schedule and finishes before handler return. NMI requires an
    /// audited entry that establishes watching and retains initialized current;
    /// early architecture entry windows and standalone readers are excluded.
    /// Keep the boot-owned RCU service and participating CPUs available. No protected
    /// reference may be used after an abnormal exit abandons this task's scope.
    unsafe fn read<'a>(&'a self, _token: &'a ReadGuard<'_>) -> &'a T {
        let value = self.value.load(Ordering::Acquire);
        // SAFETY: this container always owns a non-null initialized allocation.
        // A removed allocation stays raw-owned until its GP completes. The
        // task scope retains CPU/enrollment obligations; an interrupt borrow ends
        // before its CPU can report a later quiescent boundary. The lifetime also
        // prevents final container destruction while this reference is borrowed.
        unsafe { &*value }
    }

    /// Returns the old allocation only after its protected readers have exited.
    /// On preparation failure returns the unused new allocation without exchange.
    /// The caller must hold no lock needed by a reader or the progress service.
    fn replace_sync(&self, new_value: Box<T>) -> Result<Box<T>, (KError, Box<T>)> {
        let wait = match PreparedGracePeriodWait::prepare() {
            Ok(wait) => wait,
            Err(error) => return Err((error, new_value)),
        };
        let old = self.value.swap(Box::into_raw(new_value), Ordering::AcqRel);
        // Do not reconstruct a Box yet: old readers may still hold shared
        // borrows. A nonreturning wait leaks this raw owner instead of freeing
        // it early, including if unwinding is introduced in a host environment.
        wait.submit_and_wait();
        // SAFETY: the exchange gave this operation unique ownership, and its
        // subsequent GP ended all protected borrows of this removed allocation.
        // The container exposes no independent publication or raw ownership API.
        Ok(unsafe { Box::from_raw(old) })
    }
}

impl<T> Drop for Rcu<T> {
    fn drop(&mut self) {
        // SAFETY: exclusive final ownership excludes read references and pending
        // replacements, both of which borrow self. The current pointer still
        // represents exactly one Box owner, distinct from every returned value.
        drop(unsafe { Box::from_raw(*self.value.get_mut()) });
    }
}

unittest::path_mod_test!(tests, "rcu_owned.rs");
