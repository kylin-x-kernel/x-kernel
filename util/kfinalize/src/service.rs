// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel integration contracts, not a user-selectable executor API.

use alloc::{alloc::Global, sync::Arc};
use core::{
    alloc::{AllocError, Allocator, Layout},
    ptr::NonNull,
    sync::atomic::{AtomicPtr, Ordering},
};

use klazy::Once;

use crate::{CreateError, DeferredArc};

/// Permanent storage policy for the standard Arc identity allocation.
/// Installation precedes exposing any production preparation provider.
pub struct IdentityMemory {
    /// Allocates the exact requested Arc region, with provider-owned bookkeeping.
    pub allocate: fn(Layout) -> Result<IdentityAllocation, AllocError>,
    /// Takes the original token once; must not touch Arc's destroyed contents.
    pub deallocate: unsafe fn(NonNull<()>, Layout),
}

/// The requested region and an independent original-provenance release token.
pub struct IdentityAllocation {
    /// Only this region is exposed to Arc's storage operations.
    pub region: NonNull<[u8]>,
    /// Retained separately from Arc pointers; never derived back from them.
    pub token: NonNull<()>,
}

static IDENTITY_MEMORY: Once<&'static IdentityMemory> = Once::new();

static GLOBAL_IDENTITY_MEMORY: IdentityMemory = IdentityMemory {
    allocate: |layout| {
        Global.allocate(layout).map(|region| IdentityAllocation {
            token: region.cast(),
            region,
        })
    },
    deallocate: release_global_identity,
};

/// # Safety
/// The allocation must have been returned by Global for this layout.
unsafe fn release_global_identity(pointer: NonNull<()>, layout: Layout) {
    // SAFETY: the allocator contract transfers a matching live allocation once.
    unsafe { Global.deallocate(pointer.cast(), layout) };
}

/// A one-allocation policy/token pair. Clone is permitted only after allocation;
/// copies share the same original token and reject further allocations.
/// Dropping an allocator value does not release storage.
pub(crate) struct IdentityAllocator {
    memory: &'static IdentityMemory,
    token: AtomicPtr<()>,
}

impl Clone for IdentityAllocator {
    fn clone(&self) -> Self {
        let token = self.token.load(Ordering::Acquire);
        assert!(
            !token.is_null(),
            "identity allocator clones require an allocated identity"
        );
        Self {
            memory: self.memory,
            token: AtomicPtr::new(token),
        }
    }
}

// SAFETY: installation certifies the allocator pair. Every handle retains the
// same permanent policy, so later installation cannot change an allocation's
// deallocator. Arc itself controls the final deallocation and its exact layout.
unsafe impl Allocator for IdentityAllocator {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if !self.token.load(Ordering::Acquire).is_null() {
            return Err(AllocError);
        }
        let allocation = (self.memory.allocate)(layout)?;
        if self
            .token
            .compare_exchange(
                core::ptr::null_mut(),
                allocation.token.as_ptr(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            // SAFETY: this unpublished allocation was rejected by the one-slot
            // allocator; its original token and exact layout remain exclusively ours.
            unsafe { (self.memory.deallocate)(allocation.token, layout) };
            return Err(AllocError);
        }
        Ok(allocation.region)
    }

    unsafe fn deallocate(&self, _pointer: NonNull<u8>, layout: Layout) {
        // SAFETY: successful allocation published a non-null original token;
        // Arc copied this allocator only after that publication completed.
        let token = unsafe { NonNull::new_unchecked(self.token.load(Ordering::Acquire)) };
        // SAFETY: Arc transfers its allocation according to Allocator's contract.
        unsafe { (self.memory.deallocate)(token, layout) };
    }
}

pub(crate) fn identity_allocator() -> IdentityAllocator {
    IdentityAllocator {
        memory: IDENTITY_MEMORY
            .get()
            .copied()
            .unwrap_or(&GLOBAL_IDENTITY_MEMORY),
        token: AtomicPtr::new(core::ptr::null_mut()),
    }
}

/// Installs identity storage before installing production preparation providers.
///
/// # Safety
/// The permanent pair must implement Allocator's layout/lifetime contract.
/// allocate must return an aligned region covering the requested layout and a
/// token with authority to free the entire backing allocation independently of
/// any references subsequently created within that region.
/// Release must support every context allowed by the associated provider,
/// including NMI when advertised, without inline payload destruction. Installation
/// is serialized at startup; the first policy wins. Pre-installation allocations
/// retain Global and must never be released from NMI.
pub unsafe fn install_identity_memory(memory: &'static IdentityMemory) {
    IDENTITY_MEMORY.call_once(|| memory);
}

/// Prepares an identity through the installed kernel-integration provider.
///
/// # Safety
/// Allocator, local per-CPU and IRQ-safe lock support must be ready. Allocation
/// and metadata deallocation must be legal under caller-held locks. Only audited
/// boot/task preparation paths may use this entry, never hardirq, softirq or NMI.
/// A current task, enabled preemption and bound workers are not required.
/// Concurrent callers on independently initialized CPUs are permitted.
///
/// # Errors
/// Returns NotReady and the unchanged input before provider installation.
/// Other preparation errors also return the unchanged input.
///
/// # Panics
/// Allocation failure follows kernel policy.
pub unsafe fn new_integration<T: Send + Sync + 'static>(
    value: T,
) -> Result<DeferredArc<T>, (CreateError, T)> {
    DeferredArc::new_prepared(value, || {
        let prepare = INTEGRATION.get().ok_or(CreateError::NotReady)?;
        // SAFETY: the caller establishes this CPU's allocation/locking
        // prerequisites; installation certifies the permanent provider.
        unsafe { prepare() }
    })
}

/// A non-cloneable responsibility to destroy one payload and release its storage.
/// Dropping this token without finish deliberately leaks rather than destroying
/// potentially sleepable payload in the wrong context, including panic paths.
pub struct Finalization {
    pub(crate) context: NonNull<()>,
    pub(crate) destroy: unsafe fn(NonNull<()>),
}

// SAFETY: only final Identity::drop creates a token after all public access has
// ended. The erased allocation contains Send + Sync + 'static data, and the token
// uniquely owns its destruction. There is no shared payload-access operation.
unsafe impl Send for Finalization {}

impl Finalization {
    /// Consumes the unique obligation, executing payload Drop and freeing storage.
    ///
    /// # Safety
    /// Run only in sleepable normal-worker context, outside queue/management
    /// locks. Never invoke inline from submit. All payload references must have
    /// ended beforehand.
    ///
    /// # Panics
    /// Propagates payload destructor panics; never retry a failed destructor.
    pub unsafe fn finish(self) {
        // SAFETY: construction pairs the concrete Box token with its destructor;
        // consuming self makes this the only call, in the caller-certified context.
        unsafe { (self.destroy)(self.context) };
    }
}

/// A prepared one-shot carrier, independent of public identity and payload storage.
///
/// # Safety
/// Submission must transfer the obligation without allocation, sleeping, or
/// inline finalization. Retain it until exactly one finish call on the executor
/// selected at preparation; never infer or switch policy from submit context.
/// Execution cannot be canceled. Keep carrier storage live across publication
/// even when execution wins the race. Carrier Drop must never destroy payload.
/// Unexpected admission failure must retain/leak the obligation before failing
/// fatally, not free it or retry an ambiguously accepted submission.
pub unsafe trait Prepared: Send + Sync {
    /// Hands off a single finalization; there is no caller-visible failure path.
    ///
    /// # Safety
    /// Invoke at most once on this carrier, in task/BH/hard IRQ context or an
    /// audited startup path with allocator/per-CPU/IRQ-lock support ready. NMI
    /// requires a provider certifying NMI transfer and initialized local irq_work.
    /// The payload has no remaining public borrows or uncounted accesses.
    ///
    /// # Panics
    /// May fail fatally on violated provider/admission invariants.
    unsafe fn submit(self: Arc<Self>, finalization: Finalization);
}

/// Preparation validates context/readiness and returns a fresh unused carrier.
pub type Prepare = fn() -> Result<Arc<dyn Prepared>, CreateError>;

static NORMAL: Once<Prepare> = Once::new();

/// Preparation requiring the caller-certified new_integration context.
pub type IntegrationPrepare = unsafe fn() -> Result<Arc<dyn Prepared>, CreateError>;

static INTEGRATION: Once<IntegrationPrepare> = Once::new();

/// Publishes permanent preparation independently of worker readiness.
///
/// # Safety
/// Install during serialized kernel startup before exposing integration users.
/// The provider must support concurrent calls under new_integration's contract,
/// return a fresh carrier per success, and retain unbound obligations permanently.
/// It must not create tasks, wait for workers, or execute payload destruction.
/// Install the matching identity-memory policy first. Exposed carriers and that
/// policy must support NMI handoff when local irq_work is ready.
pub unsafe fn install_integration(prepare: IntegrationPrepare) {
    INTEGRATION.call_once(|| prepare);
}

/// Installs the permanent normal preparation entry; the first installation wins.
///
/// # Safety
/// The provider must validate sleepable construction context and service readiness,
/// return a fresh carrier for each success, and satisfy Prepared for kernel life.
/// Every carrier must finish on normal sleepable workers, never BH execution.
/// Install the matching identity-memory policy first. Exposed carriers and that
/// policy must support NMI handoff when local irq_work is ready.
/// Install during serialized boot before exposing users; no teardown is supported.
pub unsafe fn install_normal(prepare: Prepare) {
    NORMAL.call_once(|| prepare);
}

pub(crate) fn prepare_normal() -> Result<Arc<dyn Prepared>, CreateError> {
    NORMAL.get().ok_or(CreateError::NotReady)?()
}
