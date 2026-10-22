// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared ownership whose final payload destruction runs on a prepared executor.
//!
//! [`DeferredArc`] moves final payload destruction to normal sleepable workers.
//!
//! [`DeferredArc::new`] requires a ready provider and sleepable task context.
//! Clone, downgrade and upgrade use standard Arc counting. Last strong Drop
//! transfers a prepared obligation without sleeping or allocating a carrier;
//! Weak immediately stops upgrading, independently of worker progress.
//!
//! Prepared Drop supports task/BH/hard IRQ and initialized, supported NMI contexts. It is not
//! wait-free and does not make arbitrary caller-held locks safe. Payload Drop
//! must not form a dependency cycle with the shared worker pool. Use these types
//! only when destruction needs a different context. The shared
//! finalizer serializes reclamation; Drop must not wait for another object's
//! finalization on that same executor, even indirectly. Only payload
//! destruction always runs on workers; metadata frees inline outside NMI and
//! through ordinary IRQ after NMI. There is no public drain, unwrap or mutable-access API.
//!
//! # Opt-in use and costs
//!
//! Prefer ordinary Arc when the complete destructor chain is legal wherever
//! its last owner can be released. This is not a general Arc replacement or a
//! performance optimization: use it only for an identified destruction-context
//! mismatch, after reviewing object population, churn and worker dependencies.
//!
//! The current kernel provider prepares three allocations per object (identity,
//! payload with a raw-owner header, and carrier), including for zero-sized T.
//! Ordinary Arc normally needs one. Handles also retain allocator state; on
//! 64-bit targets DeferredArc and DeferredWeak are each 24 bytes, versus 8 for
//! ordinary global-allocator Arc/Weak. Payload access adds pointer indirection.
//! Final release adds queue synchronization/notification and worker scheduling;
//! NMI release additionally requires an ordinary IRQ handoff. These costs are
//! implementation/configuration dependent, not a stable layout or latency ABI.
//!
//! The shared finalizer is serial: a sleeping destructor delays later objects.
//! Queued payloads retain their resources until execution, without a latency
//! bound. Do not wait, directly or indirectly, for another finalization on this
//! executor. Dropping an owner is not a completion or shutdown barrier; callers
//! must separately finish uncounted RCU/device accesses and required cleanup.
//!
//! # Execution contexts
//!
//! Construction requires a sleepable task and installed provider. Final strong
//! release may run in task, softirq or hardirq context, provided caller-held
//! locks permit queue notification and metadata deallocation. NMI final strong
//! and Weak release require the kernel provider and initialized local irq_work.
//! Both are detected before ordinary locks/free: payload responsibility transfers
//! to ordinary IRQ then worker; metadata transfers separately to ordinary IRQ.
//! Unsupported/unready NMI delivery is a fatal contract violation, never inline
//! destruction. An upgraded weak owner can become the final strong owner too.
//! Merely borrowing T in NMI is a separate module protocol: an owner outside
//! NMI must keep T alive throughout the borrow, and all accessed operations
//! must themselves be NMI-safe. This type does not establish that protocol.
//! The unsafe integration-only constructor additionally permits audited boot/task
//! preparation, including before current Task exists and under NoPreempt, after
//! provider installation and local allocator/per-CPU/IRQ-lock prerequisites hold.
//! It never permits hardirq, softirq or NMI construction.
//!
//! ```no_run
//! use kfinalize::DeferredArc;
//! let owner = DeferredArc::new(42usize).unwrap();
//! let weak = DeferredArc::downgrade(&owner);
//! let other = owner.clone();
//! assert_eq!(*other, 42);
//! drop(other);
//! drop(owner);
//! assert!(weak.upgrade().is_none());
//! ```

#![no_std]
#![feature(allocator_api)]

extern crate alloc;

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};
use core::{
    mem::{ManuallyDrop, MaybeUninit},
    ops::Deref,
    ptr::NonNull,
};

#[doc(hidden)]
pub mod service;

/// Recoverable preparation failure. Allocation failure follows kernel policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreateError {
    /// Construction requires sleepable task context.
    InvalidContext,
    /// The selected permanent execution service has not been installed.
    NotReady,
}

struct Identity<T: Send + Sync + 'static> {
    value: NonNull<Payload<T>>,
    prepared: Option<Arc<dyn service::Prepared>>,
}

struct RawIdentity<T: Send + Sync + 'static> {
    pointer: *const Identity<T>,
    allocator: service::IdentityAllocator,
}

#[repr(C)]
struct Payload<T: Send + Sync + 'static> {
    identity: MaybeUninit<RawIdentity<T>>,
    value: T,
}

// SAFETY: Identity exclusively owns the payload Box token until its last Arc drops.
// All public access is shared and T is Send + Sync; only the final Drop transfers
// exclusive destruction to the prepared provider, after all public borrows end.
unsafe impl<T: Send + Sync + 'static> Send for Identity<T> {}
// SAFETY: Identity exposes only shared T references while public strong owners
// exist. Its pointer and prepared owner are mutated only by exclusive final Drop.
unsafe impl<T: Send + Sync + 'static> Sync for Identity<T> {}

impl<T: Send + Sync + 'static> Drop for Identity<T> {
    fn drop(&mut self) {
        if let Some(prepared) = self.prepared.take() {
            let finalization = service::Finalization {
                context: self.value.cast(),
                destroy: destroy_value::<Payload<T>>,
            };
            // SAFETY: this identity owns the sole preparation, submits once, and
            // has lost every public strong reference before Arc invokes Drop.
            unsafe { prepared.submit(finalization) };
        }
    }
}

/// # Safety
/// context is the sole Box<T> token, after all public strong references ended.
unsafe fn destroy_value<T>(context: NonNull<()>) {
    // SAFETY: new_prepared produced this concrete Box pointer; the non-cloneable
    // finalization transfers its only destruction right to this one invocation.
    drop(unsafe { Box::from_raw(context.cast::<T>().as_ptr()) });
}

/// An Arc-like shared owner with worker-context payload destruction.
///
/// Opt in only for a demonstrated destruction-context mismatch. This is not a
/// drop-in performance replacement for Arc; see the crate's costs and context
/// restrictions before adopting it for a new object type.
pub struct DeferredArc<T: Send + Sync + 'static>(Arc<Identity<T>, service::IdentityAllocator>);

/// A non-owning identity; queued destruction never keeps upgrade possible.
///
/// Its final release frees metadata inline outside NMI. The kernel provider
/// transfers NMI metadata release to initialized local ordinary irq_work.
pub struct DeferredWeak<T: Send + Sync + 'static>(Weak<Identity<T>, service::IdentityAllocator>);

impl<T: Send + Sync + 'static> DeferredArc<T> {
    /// Prepares all storage and the one-shot normal-worker carrier.
    ///
    /// Requires a ready provider and sleepable task context. Preparation pays
    /// per-object allocation costs; cloning an existing handle does not prepare
    /// another carrier. Final payload destruction is asynchronous, not bounded
    /// by the lifetime of this call's task or a later handle Drop.
    ///
    /// # Errors
    /// Returns the error and unchanged input on invalid context or unready service.
    /// The caller remains responsible for handling/dropping that input.
    ///
    /// # Panics
    /// Allocation and provider invariant failures follow kernel policy.
    pub fn new(value: T) -> Result<Self, (CreateError, T)> {
        Self::new_prepared(value, service::prepare_normal)
    }

    fn new_prepared(
        value: T,
        prepare: impl FnOnce() -> Result<Arc<dyn service::Prepared>, CreateError>,
    ) -> Result<Self, (CreateError, T)> {
        let prepared = match prepare() {
            Ok(prepared) => prepared,
            Err(error) => return Err((error, value)),
        };
        Ok(Self::from_carrier(value, prepared))
    }

    fn from_carrier(value: T, prepared: Arc<dyn service::Prepared>) -> Self {
        let value = NonNull::from(Box::leak(Box::new(Payload {
            identity: MaybeUninit::uninit(),
            value,
        })));
        let identity = Arc::new_in(
            Identity {
                value,
                prepared: Some(prepared),
            },
            service::identity_allocator(),
        );
        let raw_identity = RawIdentity {
            pointer: Arc::as_ptr(&identity),
            allocator: Arc::allocator(&identity).clone(),
        };
        // SAFETY: the new payload is exclusively owned and unpublished. Only its
        // header is initialized here; it remains immutable until worker disposal.
        let header = unsafe { core::ptr::addr_of_mut!((*value.as_ptr()).identity) };
        // SAFETY: header selects the exclusively owned uninitialized field;
        // initializing it does not create a reference to the payload value.
        unsafe { header.write(MaybeUninit::new(raw_identity)) };
        Self(identity)
    }

    /// Creates a weak identity without extending the public strong lifetime.
    pub fn downgrade(owner: &Self) -> DeferredWeak<T> {
        DeferredWeak(Arc::downgrade(&owner.0))
    }

    /// Borrows the stable payload address without transferring a strong owner.
    /// The pointer retains the payload allocation's provenance; do not replace
    /// it with a pointer derived from a dereferenced `&T` for raw-owner recovery.
    pub fn as_ptr(owner: &Self) -> *const T {
        // SAFETY: a strong owner retains the initialized payload allocation.
        // Raw field projection preserves the provenance needed by from_raw.
        unsafe { core::ptr::addr_of!((*owner.0.value.as_ptr()).value) }
    }

    /// Tests shared identity, independently of the payload's value.
    pub fn ptr_eq(left: &Self, right: &Self) -> bool {
        Arc::ptr_eq(&left.0, &right.0)
    }

    /// Returns an instantaneous count, including outstanding raw owners.
    /// This observation does not establish exclusive access or synchronization.
    pub fn strong_count(owner: &Self) -> usize {
        Arc::strong_count(&owner.0)
    }

    /// Transfers one strong owner to a stable payload pointer without allocation.
    /// The count is unchanged. Recover it exactly once with [`Self::from_raw`];
    /// abandoning it leaks that owner and prevents finalization.
    pub fn into_raw(owner: Self) -> *const T {
        let pointer = Self::as_ptr(&owner);
        let _ = Arc::into_raw_with_allocator(owner.0);
        pointer
    }

    /// Recovers one outstanding raw strong owner without changing its count.
    ///
    /// # Safety
    /// `pointer` must originate from `into_raw` for this exact T, with its
    /// allocation provenance preserved, and represent an unconsumed strong owner.
    /// Release each transferred count once. Intrusive adapters may temporarily
    /// reconstruct a ManuallyDrop view while retaining the real raw owner;
    /// that view must obey [`Self::borrow_raw`]'s lifetime and non-consumption rules.
    /// A pointer made from `&T`, `as_ptr`
    /// alone, or an expired identity does not grant ownership. Do not recover an
    /// owner while borrows that would outlive that owner remain outstanding.
    /// Releasing the result must obey DeferredArc's execution-context contract.
    pub unsafe fn from_raw(pointer: *const T) -> Self {
        // SAFETY: the caller transfers the unique outstanding raw count to the
        // reconstructed handle rather than retaining it as a borrowed view.
        unsafe { Self::raw_handle(pointer) }
    }

    /// Borrows an outstanding raw owner without incrementing or consuming it.
    /// This supports current-task views; cloning the borrowed handle creates
    /// an independent counted owner.
    ///
    /// # Safety
    /// `pointer` must retain the original allocation provenance from into_raw.
    /// Its outstanding strong owner must remain alive throughout every use of
    /// this view and any payload borrows derived from it. Never explicitly drop
    /// or extract the contained handle: this view does not own that count.
    pub unsafe fn borrow_raw(pointer: *const T) -> ManuallyDrop<Self> {
        // SAFETY: the caller retains an independent raw owner for the entire
        // view lifetime; ManuallyDrop prevents this borrowed view consuming it.
        ManuallyDrop::new(unsafe { Self::raw_handle(pointer) })
    }

    /// # Safety
    /// The pointer has into_raw's original provenance and a retained strong
    /// count. Either consume that count once or suppress this handle's Drop
    /// and keep the real count alive for all borrowed uses.
    unsafe fn raw_handle(pointer: *const T) -> Self {
        // SAFETY: into_raw preserves the enclosing allocation's provenance;
        // subtracting the repr(C) field offset recovers its initialized header.
        let payload = unsafe {
            pointer
                .cast::<u8>()
                .sub(core::mem::offset_of!(Payload<T>, value))
                .cast::<Payload<T>>()
        };
        // SAFETY: the outstanding raw owner keeps the immutable initialized
        // header and its identity alive. No reference to T is used for recovery.
        let header = unsafe { &(*payload).identity };
        // SAFETY: from_carrier initializes this header before publishing an owner.
        let identity = unsafe { header.assume_init_ref() };
        // SAFETY: the header holds this identity's original Arc pointer and a
        // compatible allocator. The caller either transfers its retained count
        // or suppresses Drop for a view bounded by that count's lifetime.
        Self(unsafe { Arc::from_raw_in(identity.pointer, identity.allocator.clone()) })
    }
}

impl<T: Send + Sync + 'static> Clone for DeferredArc<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: Send + Sync + 'static> Deref for DeferredArc<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: this public strong owner prevents Identity::drop and therefore
        // finalization. Only the initialized T field is borrowed, not its header.
        unsafe { &*Self::as_ptr(self) }
    }
}

impl<T: Send + Sync + 'static> AsRef<T> for DeferredArc<T> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T: Send + Sync + 'static> DeferredWeak<T> {
    /// Observes the public strong count; zero does not imply destruction has finished.
    pub fn strong_count(&self) -> usize {
        self.0.strong_count()
    }

    /// Returns whether both weak handles identify the same allocation, even after retirement.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.0, &other.0)
    }

    /// Acquires a public strong owner, or None once the last strong owner ended.
    ///
    /// The returned owner may become the last strong reference. Its eventual
    /// release must obey the same context and local irq_work readiness contract.
    pub fn upgrade(&self) -> Option<DeferredArc<T>> {
        self.0.upgrade().map(DeferredArc)
    }
}

impl<T: Send + Sync + 'static> Clone for DeferredWeak<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

unittest::path_mod_test!(ownership_unittest, "../ut-tests/ownership.rs");
