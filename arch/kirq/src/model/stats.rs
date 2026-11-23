// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owned observation of one current IRQ Line's receive counters.
//!
//! The snapshot is a copied view, not a mapping lease or a capability token.
//! Counts may be sampled at different instants on different CPUs.

use alloc::vec::Vec;

use crate::{Hwirq, IrqController, IrqDesc, IrqDomainId, IrqTrigger, Virq};

/// Why a current Line is or is not part of the ordinary receive statistics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqStatsClass {
    /// Ordinary resolved flow; receive counts are meaningful.
    Countable,
    /// Owned by the NMI path; excluded from the first ordinary view.
    Nmi,
    /// No resolved flow; excluded from the first ordinary view.
    NoFlow,
}

/// Errors from an owned IRQ statistics observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqStatsError {
    /// The retained identity is no longer the current, non-retired Line.
    UnknownIrq,
    /// A real allocation failure was reported.
    ///
    /// Current name storage uses `Vec::try_reserve`; the global allocator may
    /// abort on OOM instead of returning this variant. The public error exists
    /// for a future fallible collection, not as a present OOM guarantee.
    NoMemory,
    /// The caller's preallocated name capacity lost a bounded race.
    CapacityChanged,
    /// Checked length or index arithmetic overflowed.
    Overflow,
    /// The query context cannot allocate or take the required locks.
    InvalidContext,
}

/// An owned, identity-checked snapshot of one current IRQ Line.
#[derive(Debug)]
pub struct IrqStatsSnapshot {
    virq: Virq,
    hwirq: Hwirq,
    trigger: IrqTrigger,
    controller: IrqController,
    domain: Option<IrqDomainId>,
    class: IrqStatsClass,
    has_actions: bool,
    counts: [u64; kbuild_config::NR_CPUS],
    names: Vec<&'static str>,
}

impl IrqStatsSnapshot {
    pub(crate) fn new(
        virq: Virq,
        desc: IrqDesc,
        class: IrqStatsClass,
        has_actions: bool,
        counts: [u64; kbuild_config::NR_CPUS],
        names: Vec<&'static str>,
    ) -> Self {
        Self {
            virq,
            hwirq: desc.hwirq,
            trigger: desc.trigger,
            controller: desc.controller,
            domain: desc.domain,
            class,
            has_actions,
            counts,
            names,
        }
    }

    pub const fn virq(&self) -> Virq {
        self.virq
    }

    pub const fn hwirq(&self) -> Hwirq {
        self.hwirq
    }

    pub const fn trigger(&self) -> IrqTrigger {
        self.trigger
    }

    pub const fn controller(&self) -> IrqController {
        self.controller
    }

    pub const fn domain(&self) -> Option<IrqDomainId> {
        self.domain
    }

    pub const fn class(&self) -> IrqStatsClass {
        self.class
    }

    pub const fn has_actions(&self) -> bool {
        self.has_actions
    }

    pub fn counts(&self) -> &[u64] {
        &self.counts
    }

    /// Returns only actions that registered a name.
    ///
    /// An empty slice does not mean the Line has no actions; use
    /// [`Self::has_actions`].
    pub fn action_names(&self) -> &[&'static str] {
        &self.names
    }
}
