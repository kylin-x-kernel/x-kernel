// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User and pin lifetime capabilities for process address spaces.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use ksync::Mutex;

use crate::{InvalidateHandle, MmSpace};

pub(crate) struct MmUserLifetime {
    state: AtomicUsize,
}

impl MmUserLifetime {
    const TORN_DOWN: usize = 1usize << (usize::BITS as usize - 1);
    const USER_MASK: usize = !Self::TORN_DOWN;

    pub(crate) fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
        }
    }

    fn acquire_user(&self) -> bool {
        self.state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                if state & Self::TORN_DOWN != 0 || state == Self::USER_MASK {
                    None
                } else {
                    Some(state + 1)
                }
            })
            .is_ok()
    }

    fn release_user(&self) -> bool {
        let old = self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |users| {
                let count = users & Self::USER_MASK;
                debug_assert!(count > 0, "mm user count underflow");
                if count == 0 {
                    None
                } else if count == 1 {
                    Some(Self::TORN_DOWN)
                } else {
                    Some((users & Self::TORN_DOWN) | (count - 1))
                }
            })
            .expect("mm user release must have a live user");

        old & Self::USER_MASK == 1
    }

    fn is_user_alive(&self) -> bool {
        self.state.load(Ordering::Acquire) & Self::TORN_DOWN == 0
    }
}

/// Active user capability for a process address space.
///
/// This is the X-Kernel counterpart of Linux `mm_users`: process runtimes that
/// actively use the user mappings hold an `MmUserHandle`. Ordinary
/// `Arc<Mutex<MmSpace>>` observers do not keep mappings alive.
///
/// Dropping the last handle synchronously clears the address space's user
/// mappings. Callers must only release this capability from sleepable task
/// context, without holding spinlocks or running with IRQs/preemption disabled.
/// Releasing or dropping the last user must also not happen while the current
/// task already holds this handle's `MmSpace` mutex, because final release
/// synchronously locks the same mutex to tear mappings down.
#[doc(hidden)]
pub struct MmUserHandle {
    address_space: Arc<Mutex<MmSpace>>,
    lifetime: Option<Arc<MmUserLifetime>>,
}

impl MmUserHandle {
    pub(crate) fn acquire(address_space: Arc<Mutex<MmSpace>>) -> Option<Self> {
        let lifetime = address_space.lock().user_lifetime().clone();
        if !lifetime.acquire_user() {
            return None;
        }

        Some(Self {
            address_space,
            lifetime: Some(lifetime),
        })
    }

    /// Returns the virtual address space guarded by this active user.
    pub fn address_space(&self) -> &Arc<Mutex<MmSpace>> {
        &self.address_space
    }

    /// Returns the immutable address-space identity.
    pub fn mm_id(&self) -> u64 {
        self.address_space.lock().mm_id()
    }

    /// Creates another active user if the address space has not been torn down.
    ///
    /// This is valid only in sleepable task context. The returned handle has
    /// the same drop contract as [`MmUserHandle`].
    pub fn clone_user_unless_zero(&self) -> Option<Self> {
        let lifetime = self.lifetime.as_ref()?;
        if !lifetime.acquire_user() {
            return None;
        }

        Some(Self {
            address_space: self.address_space.clone(),
            lifetime: Some(lifetime.clone()),
        })
    }

    /// Creates an object pin that does not keep user mappings alive.
    pub fn pin(&self) -> MmPin {
        MmPin {
            address_space: self.address_space.clone(),
            lifetime: self
                .lifetime
                .as_ref()
                .expect("live mm user must carry lifetime state")
                .clone(),
        }
    }

    /// Releases this user and clears mappings if it was the last active user.
    ///
    /// This may take the `MmSpace` mutex and run VMA/backend teardown. It must
    /// not be called while holding a spinlock or from IRQ/preempt-disabled
    /// context, or while the current task already holds this handle's
    /// `MmSpace` mutex.
    pub fn release_and_clear_if_last(mut self) -> bool {
        let Some(lifetime) = self.lifetime.take() else {
            return false;
        };
        if !lifetime.release_user() {
            return false;
        }

        self.address_space.assert_not_owned_by_current(
            "MmUserHandle final release would relock the current task's MmSpace mutex",
        );
        self.address_space.lock().clear();
        true
    }
}

impl Drop for MmUserHandle {
    fn drop(&mut self) {
        let Some(lifetime) = self.lifetime.take() else {
            return;
        };
        if lifetime.release_user() {
            self.address_space.assert_not_owned_by_current(
                "MmUserHandle drop would relock the current task's MmSpace mutex",
            );
            self.address_space.lock().clear();
        }
    }
}

/// Object pin for an address space whose user mappings may already be torn down.
///
/// This reserves the Linux `mm_count` role for observers that need stable mm
/// identity without extending the lifetime of user mappings.
#[doc(hidden)]
pub struct MmPin {
    address_space: Arc<Mutex<MmSpace>>,
    lifetime: Arc<MmUserLifetime>,
}

impl MmPin {
    /// Attempts to upgrade this object pin into an active user.
    pub fn try_upgrade_user(&self) -> Option<MmUserHandle> {
        if !self.lifetime.acquire_user() {
            return None;
        }

        Some(MmUserHandle {
            address_space: self.address_space.clone(),
            lifetime: Some(self.lifetime.clone()),
        })
    }

    /// Returns whether active users may still be acquired.
    pub fn is_user_alive(&self) -> bool {
        self.lifetime.is_user_alive()
    }

    /// Returns the pinned address-space object.
    pub fn address_space(&self) -> &Arc<Mutex<MmSpace>> {
        &self.address_space
    }
}

/// Observer capability for an address-space object.
///
/// This keeps the `MmSpace` object observable for mapping invalidation and
/// runtime callbacks, but it does not keep user mappings alive.
#[derive(Clone)]
pub struct MmObserver {
    address_space: Arc<Mutex<MmSpace>>,
}

impl MmObserver {
    /// Creates an observer from a known live or pinned address-space owner.
    pub fn new(address_space: &Arc<Mutex<MmSpace>>) -> Self {
        Self {
            address_space: address_space.clone(),
        }
    }

    /// Creates an invalidation handle for this observer.
    ///
    /// This takes the observed `MmSpace` mutex. Callers must not invoke it
    /// while already holding that same lock; paths that already hold the
    /// address-space lock should create the handle from the locked owner
    /// instead.
    pub fn invalidate_handle(&self) -> InvalidateHandle {
        self.address_space
            .lock()
            .invalidate_handle(&self.address_space)
    }
}
