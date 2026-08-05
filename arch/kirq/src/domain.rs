// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-domain IRQ reverse maps and lock-free data-plane resolution.
//!
//! Each [`IrqDomain`] owns an immutable `hwirq -> virq` snapshot that is
//! published atomically by the control plane and read without any lock by the
//! dispatch data path (hardirq / irqson NMI). Snapshots are never freed and
//! never mutated after publication, which is what makes the lock-free read
//! sound without RCU or seqlocks.

use alloc::{boxed::Box, vec, vec::Vec};
use core::{
    marker::PhantomData,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

use super::desc::{Hwirq, IrqDomainId, Virq};

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
    Linear(Box<[Option<Virq>]>),
    /// Sorted ascending slice for sparse domains (GIC, PLIC); binary search.
    /// Chosen over `BTreeMap` because the snapshot is read-only: one
    /// contiguous allocation, cache-friendly, no node-pointer chasing.
    Sparse(Box<[(Hwirq, Virq)]>),
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
/// This type deliberately has **no `Drop` implementation**: freeing a snapshot
/// would dangle racing readers. It is only ever used as a static.
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
        // SAFETY: a non-null pointer here was produced by `Box::into_raw` in
        // `publish()` and is never freed or mutated afterwards; the Acquire
        // load synchronizes with the AcqRel store in `publish()` so the
        // snapshot contents are visible. The pointee is never deallocated, so
        // the reference is valid for the program lifetime and may be given the
        // `'static` lifetime. A null pointer means "not published yet" and
        // yields `None`.
        unsafe { ptr.as_ref() }
    }

    /// Publishes a new snapshot, leaking any previously published one.
    ///
    /// Must be called with the control lock held so writers serialize.
    pub(crate) fn publish(&self, value: Box<T>) {
        let new = Box::into_raw(value);
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

    pub(crate) fn publish(&self, revmap: Box<ReverseMap>) {
        self.revmap.publish(revmap);
        self.publishes.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn publish_count(&self) -> usize {
        self.publishes.load(Ordering::Relaxed)
    }
}

/// Compile-time domain registry: each domain ID binds to exactly one static
/// instance here, and [`domain`] is the single lookup entry point. New
/// domains add one static plus one match arm.
static GIC_IRQ_DOMAIN: IrqDomain =
    IrqDomain::new(super::desc::GIC_ROOT_DOMAIN, UnmappedPolicy::Identity);
static PLIC_IRQ_DOMAIN: IrqDomain =
    IrqDomain::new(super::desc::PLIC_ROOT_DOMAIN, UnmappedPolicy::Strict);
static IO_APIC_IRQ_DOMAIN: IrqDomain =
    IrqDomain::new(super::desc::IO_APIC_DOMAIN, UnmappedPolicy::Strict);
static MSI_IRQ_DOMAIN: IrqDomain = IrqDomain::new(super::desc::MSI_DOMAIN, UnmappedPolicy::Strict);

/// Upper bound on the IO-APIC linear reverse-map size, indexed by hwirq.
///
/// The linear table is sized `max(hwirq)+1`, so an unvalidated hwirq from a
/// malformed firmware/ACPI table could otherwise force a giant allocation
/// while the IRQ control lock is held. 4096 covers any realistic IO-APIC GSI
/// space (hardware redirection tables are at most a few hundred pins per
/// controller); larger values are treated as a configuration error and abort.
const MAX_IO_APIC_LINEAR_ENTRIES: usize = 0x1000;

/// Returns the registered domain for `id`, if any.
pub(crate) fn domain(id: IrqDomainId) -> Option<&'static IrqDomain> {
    match id {
        super::desc::GIC_ROOT_DOMAIN => Some(&GIC_IRQ_DOMAIN),
        super::desc::PLIC_ROOT_DOMAIN => Some(&PLIC_IRQ_DOMAIN),
        super::desc::IO_APIC_DOMAIN => Some(&IO_APIC_IRQ_DOMAIN),
        super::desc::MSI_DOMAIN => Some(&MSI_IRQ_DOMAIN),
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

/// Rebuilds and atomically publishes the reverse map of one domain from its
/// current build-table entries.
///
/// Must be called with the IRQ control lock held (single writer). Readers on
/// the data path never block: they observe either the previous or the new
/// complete snapshot. Ordinary IRQ unregister keeps mappings append-only, while
/// MSI resource final free may remove a mapping and publish a replacement
/// snapshot after the backend vector is no longer owned by the device.
pub(crate) fn publish_snapshot(
    domain_id: IrqDomainId,
    entries: impl Iterator<Item = (Hwirq, Virq)>,
) -> bool {
    let Some(irq_domain) = domain(domain_id) else {
        return false;
    };
    let revmap = if domain_id == super::desc::IO_APIC_DOMAIN {
        let collected: Vec<(Hwirq, Virq)> = entries.collect();
        let max_hwirq = collected.iter().map(|&(hwirq, _)| hwirq).max().unwrap_or(0);
        assert!(
            max_hwirq < MAX_IO_APIC_LINEAR_ENTRIES,
            "IO-APIC hwirq {max_hwirq} exceeds linear snapshot bound {}",
            MAX_IO_APIC_LINEAR_ENTRIES - 1
        );
        let size = max_hwirq + 1;
        let mut linear = vec![None::<Virq>; size];
        for (hwirq, virq) in collected {
            linear[hwirq] = Some(virq);
        }
        ReverseMap::Linear(linear.into_boxed_slice())
    } else {
        let sparse: Vec<(Hwirq, Virq)> = entries.collect();
        // `mappings_of` iterates a BTreeMap keyed by `(domain, hwirq)`, so a
        // single-domain filter is already ordered by hwirq; the previous
        // `sort_unstable()` was redundant. Keep a defensive check so a future
        // caller that feeds unsorted entries fails loudly in debug builds
        // instead of silently breaking the binary search in
        // `ReverseMap::lookup`.
        debug_assert!(sparse.windows(2).all(|w| w[0].0 <= w[1].0));
        ReverseMap::Sparse(sparse.into_boxed_slice())
    };
    irq_domain.publish(Box::new(revmap));
    debug!(
        "published irq domain {} snapshot #{}",
        irq_domain.id.as_u32(),
        irq_domain.publish_count()
    );
    true
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
