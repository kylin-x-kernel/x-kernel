// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-domain IRQ reverse maps and lock-free data-plane resolution.
//!
//! Ordinary domains own an immutable `hwirq -> virq` snapshot that is
//! published atomically by the control plane and read without any lock by the
//! dispatch data path (hardirq / irqson NMI). Snapshots are never freed and
//! never mutated after publication, which is what makes the lock-free read
//! sound without RCU or seqlocks. MSI uses permanent per-vector slots pointing
//! to immutable retained records, so removal needs no snapshot allocation.

use alloc::vec::Vec;
use core::{
    marker::PhantomData,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

use kalloc_types::{AllocFlags, KBox};

use crate::desc::{Hwirq, IrqDescError, IrqDomainId, Virq};

/// A frozen `hwirq -> virq` reverse map, published per domain.
///
/// # Invariants
///
/// - Immutable after publication: no field is ever written again.
/// - Never freed: superseded snapshots are intentionally leaked.
#[derive(Debug)]
pub(crate) enum ReverseMap {
    /// Dense linear table for small contiguous domains (IO-APIC), indexed by
    /// `hwirq`. Sized to the highest mapped line at build time.
    Linear(KBox<[Option<Virq>]>),
    /// Sorted ascending slice for sparse domains (GIC, PLIC); binary search.
    /// Chosen over `BTreeMap` because the snapshot is read-only: one
    /// contiguous allocation, cache-friendly, no node-pointer chasing.
    Sparse(KBox<[(Hwirq, Virq)]>),
}

impl ReverseMap {
    /// Returns the mapped logical IRQ, or `None` when this domain has no
    /// mapping for `hwirq`. Deterministic and allocation-free.
    #[inline]
    fn lookup(&self, hwirq: Hwirq) -> Option<Virq> {
        match self {
            Self::Linear(table) => table.get(hwirq).copied().flatten(),
            Self::Sparse(entries) => entries
                .binary_search_by_key(&hwirq, |&(hwirq, _)| hwirq)
                .ok()
                .map(|index| entries[index].1),
        }
    }
}

/// Atomically published immutable snapshot.
///
/// Readers pay a single `Acquire` load and then read through a reference that
/// is valid for the rest of the program. Writers serialize on the IRQ control
/// lock and publish with `AcqRel`, leaking the superseded snapshot on purpose.
/// `get()` returns `&'static T` so the "pointee lives forever" property is
/// encoded in the type, not just in comments.
///
/// This type deliberately has **no `Drop` implementation**: freeing a record
/// would dangle racing readers. Production owners are static domains or slots
/// in the permanently published MSI table.
pub(crate) struct Published<T> {
    ptr: AtomicPtr<T>,
    // Raw-pointer marker negates the auto Send/Sync traits of `AtomicPtr<T>`
    // so the shared-read soundness bound can be re-established explicitly.
    _not_send_sync: PhantomData<*mut T>,
}

// SAFETY: moving `Published<T>` to another CPU only transfers the atomic
// pointer; readers dereference through `get()`, whose returned references are
// valid for the program lifetime. `T` must be `Send` for the pointee to be
// usable on the destination CPU.
unsafe impl<T: Send + Sync> Send for Published<T> {}
// SAFETY: `get()` may run on any CPU and hands out `'static` shared references
// to a pointee that is never mutated or freed after publication, so cross-CPU
// sharing is sound exactly when `T` is `Send + Sync`. The `Acquire` load in
// `get()` pairs with the `AcqRel` store in `publish()`, making snapshot
// contents visible to readers.
unsafe impl<T: Send + Sync> Sync for Published<T> {}

impl<T> Published<T> {
    pub(crate) const fn new() -> Self {
        Self {
            ptr: AtomicPtr::new(core::ptr::null_mut()),
            _not_send_sync: PhantomData,
        }
    }

    /// Lock-free read of the current snapshot, if any has been published.
    ///
    /// Safe to call from hardirq / NMI context: one atomic load, no allocation,
    /// no blocking. The returned reference is `'static`: published snapshots
    /// are never freed, so the pointee outlives every use of it, including use
    /// after this `Published` itself has been dropped.
    pub(crate) fn get(&self) -> Option<&'static T> {
        let ptr = self.ptr.load(Ordering::Acquire);
        // SAFETY: a non-null pointer here was produced by `KBox::into_raw` in
        // `publish()` and is never freed afterwards; mutation requires safe
        // interior mutability (the MSI table's atomic slots). The Acquire
        // load synchronizes with the AcqRel store in `publish()` so the
        // snapshot contents are visible. The pointee is never deallocated, so
        // the reference is valid for the program lifetime and may be given the
        // `'static` lifetime. A null pointer means "not published yet" and
        // yields `None`.
        unsafe { ptr.as_ref() }
    }

    /// Clears a slot during serialized release, restoring it on rejection.
    #[cfg(any(target_arch = "x86_64", unittest))]
    pub(crate) fn clear_with(&self, release: impl FnOnce() -> bool) -> bool {
        let previous = self.ptr.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if release() {
            true
        } else {
            self.ptr.store(previous, Ordering::Release);
            false
        }
    }

    /// Publishes an immutable owner permanently under control-lock exclusion.
    pub(crate) fn publish(&self, value: KBox<T>) {
        let new = KBox::into_raw(value);
        let old = self.ptr.swap(new, Ordering::AcqRel);
        // Deliberately leak `old`: a racing reader may still hold a reference
        // to it, so it must remain alive for the rest of the program. Dropping
        // the raw pointer here only discards the value; it does not free it.
        let _ = old;
    }
}

/// How a domain resolves hardware IRQs that have no published mapping.
///
/// This is a per-domain declaration (D-10), **never** a failure fallback:
/// resolution is lock-free, so a missing mapping can only mean the line is
/// genuinely not mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnmappedPolicy {
    /// Unmapped lines resolve to their raw hardware number (identity
    /// dispatch) — e.g. the GIC domain, where the arch timer and IPIs are
    /// registered as plain numbers and dispatched through this domain.
    Identity,
    /// Unmapped lines are strict misses; callers report them unhandled.
    Strict,
}

/// One interrupt domain: identity plus its published reverse-map snapshot.
pub(crate) struct IrqDomain {
    id: IrqDomainId,
    revmap: Published<ReverseMap>,
    unmapped_policy: UnmappedPolicy,
    /// Monotonic snapshot counter, bumped on every publish. Lets the control
    /// plane observe abnormal runtime re-publication frequency.
    publishes: AtomicUsize,
}

impl IrqDomain {
    pub(crate) const fn new(id: IrqDomainId, unmapped_policy: UnmappedPolicy) -> Self {
        Self {
            id,
            revmap: Published::new(),
            unmapped_policy,
            publishes: AtomicUsize::new(0),
        }
    }

    /// Lock-free lookup.
    ///
    /// Returns the mapped virq, or — for domains with the explicit identity
    /// policy — the raw `hwirq`. `None` means the domain has no mapping and
    /// no identity policy; callers report the interrupt as unhandled.
    pub(crate) fn resolve(&self, hwirq: Hwirq) -> Option<Virq> {
        if self.id == crate::desc::MSI_DOMAIN {
            return MSI_TABLE.get()?.slot(hwirq)?.get().copied();
        }
        if let Some(virq) = self.revmap.get().and_then(|map| map.lookup(hwirq)) {
            return Some(virq);
        }
        if matches!(self.unmapped_policy, UnmappedPolicy::Identity) {
            // Identity returns the raw hwirq as the virq. This overlaps the
            // dynamic virq space when hwirq >= DYNAMIC_VIRQ_BASE (4096), which
            // matches the pre-refactor `unwrap_or(hwirq)` fallback; real GIC
            // lines (SGI/PPI/SPI) stay below that bound, and larger line
            // numbers must be explicitly mapped.
            return Some(hwirq);
        }
        None
    }

    pub(crate) fn publish(&self, revmap: KBox<ReverseMap>) {
        self.revmap.publish(revmap);
        self.publishes.fetch_add(1, Ordering::Relaxed);
        debug!(
            "published irq domain {} snapshot #{}",
            self.id.as_u32(),
            self.publish_count()
        );
    }

    pub(crate) fn publish_count(&self) -> usize {
        self.publishes.load(Ordering::Relaxed)
    }
}

/// Compile-time domain registry: each domain ID binds to exactly one static
/// instance here, and [`domain`] is the single lookup entry point. New
/// domains add one static plus one match arm.
static GIC_IRQ_DOMAIN: IrqDomain =
    IrqDomain::new(crate::desc::GIC_ROOT_DOMAIN, UnmappedPolicy::Identity);
static PLIC_IRQ_DOMAIN: IrqDomain =
    IrqDomain::new(crate::desc::PLIC_ROOT_DOMAIN, UnmappedPolicy::Strict);
static IO_APIC_IRQ_DOMAIN: IrqDomain =
    IrqDomain::new(crate::desc::IO_APIC_DOMAIN, UnmappedPolicy::Strict);
static MSI_IRQ_DOMAIN: IrqDomain = IrqDomain::new(crate::desc::MSI_DOMAIN, UnmappedPolicy::Strict);

// Mapping fixtures must not pretend that synthetic IDs belong to real hardware.
#[cfg(unittest)]
pub(crate) const TEST_DOMAIN: IrqDomainId = IrqDomainId::new(u32::MAX);
#[cfg(unittest)]
static TEST_IRQ_DOMAIN: IrqDomain = IrqDomain::new(TEST_DOMAIN, UnmappedPolicy::Strict);

/// Upper bound on the IO-APIC linear reverse-map size, indexed by hwirq.
///
/// The linear table is sized `max(hwirq)+1`, so an unvalidated hwirq from a
/// malformed firmware/ACPI table could otherwise force a giant allocation
/// while the IRQ control lock is held. 4096 covers any realistic IO-APIC GSI
/// space (hardware redirection tables are at most a few hundred pins per
/// controller); larger values return InvalidHwirq before mutation.
const MAX_IO_APIC_LINEAR_ENTRIES: usize = 0x1000;

/// Returns the registered domain for `id`, if any.
pub(crate) fn domain(id: IrqDomainId) -> Option<&'static IrqDomain> {
    match id {
        crate::desc::GIC_ROOT_DOMAIN => Some(&GIC_IRQ_DOMAIN),
        crate::desc::PLIC_ROOT_DOMAIN => Some(&PLIC_IRQ_DOMAIN),
        crate::desc::IO_APIC_DOMAIN => Some(&IO_APIC_IRQ_DOMAIN),
        crate::desc::MSI_DOMAIN => Some(&MSI_IRQ_DOMAIN),
        #[cfg(unittest)]
        TEST_DOMAIN => Some(&TEST_IRQ_DOMAIN),
        _ => None,
    }
}

/// Lock-free data-plane resolution for one domain.
///
/// Safe to call from hardirq / irqson NMI context. `None` means the domain has
/// no mapping for this hardware IRQ; it is never used as an identity fallback.
pub(crate) fn resolve(domain_id: IrqDomainId, hwirq: Hwirq) -> Option<Virq> {
    domain(domain_id)?.resolve(hwirq)
}

/// A complete unpublished update. Dropping it does not affect reverse lookup.
pub(crate) enum PreparedPublication {
    Snapshot(&'static IrqDomain, KBox<ReverseMap>),
    Msi(&'static Published<Virq>, KBox<Virq>),
}

impl PreparedPublication {
    pub(crate) fn publish(self) {
        match self {
            Self::Snapshot(domain, snapshot) => domain.publish(snapshot),
            Self::Msi(slot, record) => {
                slot.publish(record);
                MSI_IRQ_DOMAIN.publishes.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

pub(crate) struct MsiTable {
    first: Hwirq,
    slots: KBox<[Published<Virq>]>,
}

impl MsiTable {
    /// Builds exclusively owned slots before publication. Every slot is written
    /// once before assume_init, and initialization cannot unwind or fail.
    pub(crate) fn new(range: core::ops::Range<Hwirq>) -> Result<Self, IrqDescError> {
        let count = range
            .end
            .checked_sub(range.start)
            .filter(|&count| count != 0)
            .ok_or(IrqDescError::InvalidHwirq { hwirq: range.start })?;
        let mut slots =
            KBox::<[Published<Virq>]>::new_uninit_slice_with_flags(count, AllocFlags::NOWAIT)
                .map_err(|_| IrqDescError::NoMemory)?;
        for slot in slots.iter_mut() {
            slot.write(Published::new());
        }
        // SAFETY: every slot was initialized above; no references escape before ownership transfer.
        let slots = unsafe { slots.assume_init() };
        Ok(Self {
            first: range.start,
            slots,
        })
    }

    pub(crate) fn slot(&self, hwirq: Hwirq) -> Option<&Published<Virq>> {
        self.slots.get(hwirq.checked_sub(self.first)?)
    }
}

static MSI_TABLE: Published<MsiTable> = Published::new();

/// Called under registry exclusion. A failed attempt publishes no table and
/// may be retried; the backend range must never change after first admission.
fn prepare_msi(hwirq: Hwirq, virq: Virq) -> Result<PreparedPublication, IrqDescError> {
    #[cfg(target_arch = "x86_64")]
    let range = crate::MsiBackendIf::msi_hwirq_range();
    #[cfg(not(target_arch = "x86_64"))]
    let range: Option<core::ops::Range<Hwirq>> = None;
    let range = range.ok_or(IrqDescError::InvalidHwirq { hwirq })?;
    prepare_msi_with(
        &MSI_TABLE,
        range,
        hwirq,
        virq,
        |virq| KBox::new_nowait(virq).map_err(|_| IrqDescError::NoMemory),
        |range| KBox::new_nowait(MsiTable::new(range)?).map_err(|_| IrqDescError::NoMemory),
    )
}

/// Shared preparation seam. The caller serializes table initialization and
/// mapping changes. Record failure skips table preparation; table failure
/// drops the unpublished record. Reuse never calls the table factory.
pub(crate) fn prepare_msi_with(
    table: &Published<MsiTable>,
    range: core::ops::Range<Hwirq>,
    hwirq: Hwirq,
    virq: Virq,
    prepare_record: impl FnOnce(Virq) -> Result<KBox<Virq>, IrqDescError>,
    prepare_table: impl FnOnce(core::ops::Range<Hwirq>) -> Result<KBox<MsiTable>, IrqDescError>,
) -> Result<PreparedPublication, IrqDescError> {
    if !range.contains(&hwirq) {
        return Err(IrqDescError::InvalidHwirq { hwirq });
    }
    let record = prepare_record(virq)?;
    if table.get().is_none() {
        table.publish(prepare_table(range)?);
    }
    let slot = table
        .get()
        .and_then(|table| table.slot(hwirq))
        .ok_or(IrqDescError::InvalidHwirq { hwirq })?;
    Ok(PreparedPublication::Msi(slot, record))
}

/// Clears reverse lookup before returning a vector for reuse. Registry
/// exclusion must span this call and software removal. Backend rejection must
/// retain vector ownership; it restores the prior record without allocation.
#[cfg(any(target_arch = "x86_64", unittest))]
pub(crate) fn remove_msi(hwirq: Hwirq, release: impl FnOnce() -> bool) -> bool {
    let Some(slot) = MSI_TABLE.get().and_then(|table| table.slot(hwirq)) else {
        return false;
    };
    let removed = slot.clear_with(release);
    if removed {
        MSI_IRQ_DOMAIN.publishes.fetch_add(1, Ordering::Relaxed);
    }
    removed
}

/// Prepares a new mapping under IRQ control/Line exclusion using NOWAIT.
/// No mapping, descriptor or reverse lookup is changed on allocation failure.
pub(crate) fn prepare_mapping(
    domain_id: IrqDomainId,
    mut entries: Vec<(Hwirq, Virq)>,
    hwirq: Hwirq,
    virq: Virq,
) -> Result<PreparedPublication, IrqDescError> {
    if domain_id == crate::desc::MSI_DOMAIN {
        return prepare_msi(hwirq, virq);
    }
    entries.sort_unstable_by_key(|&(hwirq, _)| hwirq);
    prepare_snapshot(domain_id, &entries)
}

/// Prepares both ordinary snapshot owners before any live-state mutation.
/// The caller supplies sorted entries and retains registry exclusion.
pub(crate) fn prepare_snapshot(
    domain_id: IrqDomainId,
    entries: &[(Hwirq, Virq)],
) -> Result<PreparedPublication, IrqDescError> {
    let irq_domain = domain(domain_id).ok_or(IrqDescError::UnknownDomain { domain: domain_id })?;
    let revmap = if domain_id == crate::desc::IO_APIC_DOMAIN {
        let max_hwirq = entries.iter().map(|&(hwirq, _)| hwirq).max().unwrap_or(0);
        if max_hwirq >= MAX_IO_APIC_LINEAR_ENTRIES {
            return Err(IrqDescError::InvalidHwirq { hwirq: max_hwirq });
        }
        let mut linear =
            KBox::new_slice_with_flags(None::<Virq>, max_hwirq + 1, AllocFlags::NOWAIT)
                .map_err(|_| IrqDescError::NoMemory)?;
        for &(hwirq, virq) in entries {
            linear[hwirq] = Some(virq);
        }
        ReverseMap::Linear(linear)
    } else {
        debug_assert!(entries.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        ReverseMap::Sparse(
            KBox::from_slice_with_flags(entries, AllocFlags::NOWAIT)
                .map_err(|_| IrqDescError::NoMemory)?,
        )
    };
    let snapshot = KBox::new_nowait(revmap).map_err(|_| IrqDescError::NoMemory)?;
    Ok(PreparedPublication::Snapshot(irq_domain, snapshot))
}

/// How a claimed interrupt should be turned into an OS-visible logical IRQ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqRef {
    /// Resolve through the domain's published reverse map (lock-free).
    Domain(IrqDomainId, Hwirq),
    /// The claim is already an OS-visible logical IRQ — an explicit identity /
    /// `NO_MAP` line (e.g. LAPIC timer, RISC-V timer/IPI, LoongArch EXTIOI).
    /// Identity is a deliberate policy, not a fallback for failed lookups.
    Virq(Virq),
}
