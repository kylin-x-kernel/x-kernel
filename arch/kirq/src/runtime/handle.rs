// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Sparse enumeration and retained-Line control handles.
//!
//! A handle retains the original Line Arc observed in the directory. The label
//! is not permission: every control operation revalidates the identity under
//! management admission. Enumeration is task/control-only and never creates a
//! mapping, action or hardware route.

use alloc::sync::{Arc, Weak};

use kcpu_id_map::KCpuMask;

use super::{
    affinity::{
        apply_locked, apply_user_locked, check_user_affinity_change_locked, snapshot_locked,
        source_is_user_settable,
    },
    index::DIRECTORY,
    manager::{self, ManagementMode},
    retirement::{self, IrqLineRetirementObserver, IrqLineRetirementSubscription},
    state::{IRQ_STATE, IrqLine},
};
use crate::{IrqAffinityError, IrqAffinitySnapshot, IrqDescError, Virq};

/// A retained IRQ Line identity with a stable virq label.
///
/// The handle owns one storage reference. It does not retain an action, mapping
/// lease, route permission or hardware source. Once the original Line is
/// retired, query and control operations fail with `UnknownIrq`; they never
/// redirect to a replacement that reuses the same virq.
pub struct IrqLineHandle {
    virq: Virq,
    line: Arc<IrqLine>,
}

impl IrqLineHandle {
    /// Returns the logical IRQ number observed with this identity.
    ///
    /// The label survives retirement and is not proof of current directory
    /// membership or permission to operate on the source.
    pub fn virq(&self) -> Virq {
        self.virq
    }

    /// Compares the underlying Line identity, not the wrapper allocation.
    pub fn is_same_line(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.line, &other.line) && self.virq == other.virq
    }

    /// Returns whether this exact Line is the current non-retired directory entry.
    ///
    /// This takes the registry and Line short locks. It is a task/control-plane
    /// observation, not an NMI-safe permission token.
    pub fn is_current(&self) -> bool {
        let state = IRQ_STATE.lock();
        if !state
            .descs
            .get(&self.virq)
            .is_some_and(|current| Arc::ptr_eq(&current, &self.line))
        {
            return false;
        }
        !self.line.lock().is_retired()
    }

    /// Returns whether this Line has ever published an action.
    ///
    /// The fact is not cleared by action removal; it is a visibility input only.
    /// Takes the Line short lock; not NMI-safe.
    pub fn has_action_history(&self) -> bool {
        self.line.lock().has_action_history()
    }

    /// Returns the stored affinity policy/route without a settable-capability check.
    ///
    /// Retired or replaced identities still return `UnknownIrq`; no hardware
    /// capability or action presence is consulted.
    pub fn affinity_snapshot(&self) -> Result<IrqAffinitySnapshot, IrqAffinityError> {
        if crate::context::is_in_nmi() {
            return Err(IrqAffinityError::Management(IrqDescError::InvalidContext {
                operation: "observe IRQ affinity snapshot",
            }));
        }
        let state = IRQ_STATE.lock();
        if !state
            .descs
            .get(&self.virq)
            .is_some_and(|current| Arc::ptr_eq(&current, &self.line))
        {
            return Err(IrqAffinityError::Management(IrqDescError::UnknownIrq));
        }
        let entry = self.line.lock();
        if entry.is_retired() {
            return Err(IrqAffinityError::Management(IrqDescError::UnknownIrq));
        }
        Ok(entry.affinity)
    }

    /// Reports whether this source has a static ordinary user-routing capability.
    ///
    /// This is a descriptor/source classification, not an action or policy
    /// token: ordinary immediate/deferred `Any` sources return `true`, while
    /// fixed, PerCpu/NMI/MSI, no-flow and no-controller sources return `false`.
    /// Action removal, action restrictions and churn do not change it. A retired
    /// or replaced identity still returns `UnknownIrq`.
    pub fn is_user_affinity_source_supported(&self) -> Result<bool, IrqAffinityError> {
        if crate::context::is_in_nmi() {
            return Err(IrqAffinityError::Management(IrqDescError::InvalidContext {
                operation: "classify IRQ affinity source",
            }));
        }
        let state = IRQ_STATE.lock();
        if !state
            .descs
            .get(&self.virq)
            .is_some_and(|current| Arc::ptr_eq(&current, &self.line))
        {
            return Err(IrqAffinityError::Management(IrqDescError::UnknownIrq));
        }
        let entry = self.line.lock();
        if entry.is_retired() {
            return Err(IrqAffinityError::Management(IrqDescError::UnknownIrq));
        }
        source_is_user_settable(&entry)
    }

    /// Registers a weak observer for this exact Line's retirement.
    ///
    /// The slot is allocated before the Line lock. Registration verifies the
    /// original Line is still current under `IRQ_STATE -> Line`. Requires
    /// task/control context outside hardirq, NMI, BH-disabled task and RCU read
    /// scope. A retired or replaced handle returns `UnknownIrq`.
    pub fn subscribe_retirement(
        &self,
        observer: Weak<dyn IrqLineRetirementObserver>,
    ) -> Result<IrqLineRetirementSubscription, IrqDescError> {
        if crate::context::is_in_interrupt_context() || krcu::is_reading() {
            return Err(IrqDescError::InvalidContext {
                operation: "subscribe IRQ retirement",
            });
        }
        retirement::subscribe(self.virq, &self.line, observer)
    }

    /// Reads confirmed affinity from this exact Line.
    ///
    /// May run in ordinary task or IRQ context and does not sleep or allocate.
    /// NMI is rejected before locking. The caller must not hold `IRQ_STATE`,
    /// this Line's lock, or the Line management mutex; the query acquires
    /// `IRQ_STATE` and then the Line lock itself. It is a coherent historical
    /// snapshot, not a mapping lease. A retired or replaced Line returns
    /// `UnknownIrq` without observing a successor.
    pub fn irq_affinity(&self) -> Result<IrqAffinitySnapshot, IrqAffinityError> {
        if crate::context::is_in_nmi() {
            return Err(IrqAffinityError::Management(IrqDescError::InvalidContext {
                operation: "query IRQ affinity handle",
            }));
        }
        let state = IRQ_STATE.lock();
        if !state
            .descs
            .get(&self.virq)
            .is_some_and(|current| Arc::ptr_eq(&current, &self.line))
        {
            return Err(IrqAffinityError::Management(IrqDescError::UnknownIrq));
        }
        let entry = self.line.lock();
        if entry.is_retired() {
            return Err(IrqAffinityError::Management(IrqDescError::UnknownIrq));
        }
        snapshot_locked(&entry)
    }

    /// Samples whether a user-originated routing change is currently allowed.
    ///
    /// This checks source/flow eligibility, teardown, action presence and owner
    /// policy under `IRQ_STATE -> Line`, but performs no hardware operation and
    /// does not authorize a future request. It must not be used as a
    /// capabilities token; a later setter repeats every check. NMI is rejected
    /// before locking. This result does not imply a particular CPU mask is
    /// ready or programmable.
    pub fn check_user_affinity_change(&self) -> Result<(), IrqAffinityError> {
        if crate::context::is_in_nmi() {
            return Err(IrqAffinityError::Management(IrqDescError::InvalidContext {
                operation: "check user IRQ affinity change",
            }));
        }
        let state = IRQ_STATE.lock();
        if !state
            .descs
            .get(&self.virq)
            .is_some_and(|current| Arc::ptr_eq(&current, &self.line))
        {
            return Err(IrqAffinityError::Management(IrqDescError::UnknownIrq));
        }
        let entry = self.line.lock();
        if entry.is_retired() {
            return Err(IrqAffinityError::Management(IrqDescError::UnknownIrq));
        }
        check_user_affinity_change_locked(&entry)
    }

    /// Applies a user-originated affinity request to this exact Line.
    ///
    /// Requires sleepable task context. The original Arc is revalidated under
    /// management admission; source/flow, teardown, action presence and owner
    /// policy are rechecked under the Line lock before any hardware callback.
    /// A stale handle returns `UnknownIrq`; a protected action returns
    /// `UserRestricted`. This is not the trusted-kernel setter and does not
    /// overwrite owner policy or initial-route obligations.
    pub fn set_user_affinity(
        &self,
        requested: &KCpuMask,
    ) -> Result<IrqAffinitySnapshot, IrqAffinityError> {
        manager::with_retained_line(self.virq, &self.line, ManagementMode::Task, |_, line| {
            let mut entry = line.lock();
            let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
            Ok(apply_user_locked(&mut entry, requested, ready))
        })
        .map_err(IrqAffinityError::Management)?
        .ok_or(IrqAffinityError::Management(IrqDescError::UnknownIrq))?
    }

    /// Applies affinity through this exact Line's admitted management transaction.
    ///
    /// Requires sleepable task context. The original Arc is revalidated against
    /// the directory under management admission; a stale handle returns
    /// `UnknownIrq` before any hardware callback. This is trusted kernel control,
    /// not user authorization or a source-owner policy bypass.
    pub fn set_irq_affinity(
        &self,
        requested: &KCpuMask,
    ) -> Result<IrqAffinitySnapshot, IrqAffinityError> {
        manager::with_retained_line(self.virq, &self.line, ManagementMode::Task, |_, line| {
            let mut entry = line.lock();
            let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
            Ok(apply_locked(&mut entry, requested, ready))
        })
        .map_err(IrqAffinityError::Management)?
        .ok_or(IrqAffinityError::Management(IrqDescError::UnknownIrq))?
    }
}

/// Looks up one currently mapped Line identity by exact virq.
///
/// Does not create a mapping or action. Returns `Ok(None)` for an absent or
/// already-retired entry. Requires initialized CPU-local/context primitives and
/// permits task/control context only: NMI, hardirq, serving-softirq,
/// BH-disabled task and active RCU reader contexts are rejected before the
/// XArray lock. Other subsystem locks may be held only under their own
/// protocols; this operation does not recursively acquire the array writer lock.
pub fn lookup_irq_line(virq: Virq) -> Result<Option<IrqLineHandle>, IrqDescError> {
    reject_enumeration_context("lookup IRQ line handle")?;
    let Some(line) = DIRECTORY.get_owned_locked(virq) else {
        return Ok(None);
    };
    if line.lock().is_retired() {
        return Ok(None);
    }
    Ok(Some(IrqLineHandle { virq, line }))
}

/// Returns the least currently mapped Line at or after `start`.
///
/// The start key is inclusive. Retired candidates are skipped after releasing
/// the XArray lock; `usize::MAX` terminates the caller's forward traversal.
/// The same initialized task/control context restrictions as
/// [`lookup_irq_line`] apply: NMI, hardirq, serving-softirq, BH-disabled task
/// and active RCU reader contexts are rejected before the XArray lock. The
/// result is weakly consistent: an entry may be inserted behind the cursor or
/// retire immediately after observation, and must be revalidated by every
/// control operation.
pub fn next_irq_line(start: Virq) -> Result<Option<IrqLineHandle>, IrqDescError> {
    reject_enumeration_context("enumerate IRQ line handles")?;
    let mut cursor = start;
    loop {
        let Some((virq, line)) = DIRECTORY.get_next_owned_locked(cursor) else {
            return Ok(None);
        };
        if !line.lock().is_retired() {
            return Ok(Some(IrqLineHandle { virq, line }));
        }
        cursor = match virq.checked_add(1) {
            Some(next) => next,
            None => return Ok(None),
        };
    }
}

fn reject_enumeration_context(operation: &'static str) -> Result<(), IrqDescError> {
    if crate::context::is_in_interrupt_context() || krcu::is_reading() {
        return Err(IrqDescError::InvalidContext { operation });
    }
    Ok(())
}

unittest::path_mod_test!(tests, "../../ut-tests/handle.rs");
