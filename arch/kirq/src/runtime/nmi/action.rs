// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! One immutable owner with CPU-local, quiescent publication.

use core::{
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use kalloc_types::KBox;

use crate::platform::Handler;

pub(in crate::runtime) struct NmiAction {
    owner: AtomicPtr<Handler>,
    local: [AtomicPtr<Handler>; kbuild_config::NR_CPUS],
}

impl NmiAction {
    pub(in crate::runtime) const fn new() -> Self {
        Self {
            owner: AtomicPtr::new(ptr::null_mut()),
            local: [const { AtomicPtr::new(ptr::null_mut()) }; kbuild_config::NR_CPUS],
        }
    }

    pub(in crate::runtime) fn try_publish(
        &self,
        owner: KBox<Handler>,
    ) -> Result<(), KBox<Handler>> {
        let raw = KBox::into_raw(owner);
        match self.owner.compare_exchange(
            ptr::null_mut(),
            raw,
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(()),
            // SAFETY: failed publication never transferred this allocation.
            Err(_) => Err(unsafe { KBox::from_raw(raw) }),
        }
    }

    #[cfg(unittest)]
    pub(in crate::runtime) fn has_owner(&self) -> bool {
        !self.owner.load(Ordering::Acquire).is_null()
    }

    /// # Safety
    /// Hold this Line's lock with IRQs/migration excluded, outside NMI. Local
    /// controller preparation has succeeded and the action remains installed.
    pub(in crate::runtime) unsafe fn publish_current(&self) -> bool {
        let raw = self.owner.load(Ordering::Acquire);
        if raw.is_null() {
            return false;
        }
        let cpu = kiface_cpu_api::CurrentCpuIf::current_cpu().as_usize();
        self.local[cpu].store(raw, Ordering::Release);
        true
    }

    /// # Safety
    /// Hold this Line's lock with IRQs/migration excluded, outside NMI. Local
    /// teardown succeeded; no local dispatch/borrow or original claim remains.
    /// Never revoke a remote CPU's publication.
    pub(in crate::runtime) unsafe fn clear_current(&self) {
        let cpu = kiface_cpu_api::CurrentCpuIf::current_cpu().as_usize();
        self.local[cpu].store(ptr::null_mut(), Ordering::Release);
    }

    /// Keeps CPU exclusion through the complete operation; no borrow escapes.
    pub(in crate::runtime) fn with_current<R>(
        &self,
        operation: impl FnOnce(Option<&Handler>) -> R,
    ) -> R {
        let _pin = kspin::NoPreemptIrqSave::new();
        let cpu = kiface_cpu_api::CurrentCpuIf::current_cpu().as_usize();
        let raw = self.local[cpu].load(Ordering::Acquire);
        // SAFETY: only this CPU can revoke its pointer, outside NMI and without
        // a live dispatch. IRQ exclusion prevents ordinary local revocation;
        // nested NMI returns before the interrupted writer resumes. Retirement
        // refuses any published slot, and the closure cannot retain this borrow.
        operation(unsafe { raw.as_ref() })
    }

    /// # Safety
    /// Hold this Line's lock and management exclusion; prevent concurrent
    /// publication. Closing and source shutdown must precede final withdrawal.
    /// Release the returned owner only after dropping those locks.
    pub(in crate::runtime) unsafe fn take(&self) -> Option<KBox<Handler>> {
        if self
            .local
            .iter()
            .any(|slot| !slot.load(Ordering::Acquire).is_null())
        {
            return None;
        }
        let raw = self.owner.swap(ptr::null_mut(), Ordering::AcqRel);
        if raw.is_null() {
            None
        } else {
            // SAFETY: all local revocations completed before their Line unlock;
            // no local borrow remains. Management excludes a new publisher.
            Some(unsafe { KBox::from_raw(raw) })
        }
    }
}

impl Drop for NmiAction {
    fn drop(&mut self) {
        let raw = *self.owner.get_mut();
        if !raw.is_null() {
            // SAFETY: exclusive destruction excludes accessors borrowing self.
            drop(unsafe { KBox::from_raw(raw) });
        }
    }
}
