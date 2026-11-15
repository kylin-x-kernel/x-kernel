// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task charges, membership, and migration.
//!
//! A task owns exactly one [`TaskMembership`]. Fork reserves a [`TaskCharge`]
//! against every controller on the target's ancestry and converts it into
//! membership only at commit; migration moves an existing membership without
//! applying `pids.max`, because the source group already admitted the task.
//!
//! All mutations of controller counts and member sets run under the hierarchy
//! transaction, so a failed multi-task migration rolls back to the exact
//! pre-call state.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::Ordering;

use kerrno::{KError, KResult, LinuxError};
use kidentity::TaskIdentityToken;
use kspin::SpinNoIrq;

use crate::{
    Cgroup, Controller,
    controller::{ChargeGuard, ChargeSet},
};

/// Live cgroup reference held for the duration of one filesystem operation.
///
/// Dropping the guard releases the removal reservation, so `rmdir` cannot
/// delete a node that a concurrent file operation is still using.
pub struct CgroupOperationGuard {
    cgroup: Arc<Cgroup>,
}

impl CgroupOperationGuard {
    pub(crate) fn new(cgroup: Arc<Cgroup>) -> Self {
        Self { cgroup }
    }
}

impl Drop for CgroupOperationGuard {
    fn drop(&mut self) {
        self.cgroup.reservations.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A fork/clone task charge that rolls back unless converted into membership.
pub struct TaskCharge {
    cgroup: Arc<Cgroup>,
    charges: ChargeSet,
    reservation_held: bool,
}

impl TaskCharge {
    /// Reserves one task against every controller in `controllers`.
    ///
    /// The caller must hold the hierarchy transaction and have checked that
    /// the cgroup is active. On failure the reservation and every controller
    /// charge taken so far are released before returning.
    ///
    /// # Errors
    ///
    /// Propagates controller admission errors (`EAGAIN` for `pids.max`).
    pub(crate) fn reserve(
        cgroup: Arc<Cgroup>,
        controllers: &[Arc<dyn Controller>],
    ) -> KResult<Self> {
        let mut charge = Self {
            cgroup,
            charges: ChargeSet::new(),
            reservation_held: true,
        };
        charge.cgroup.reservations.fetch_add(1, Ordering::AcqRel);
        for controller in controllers {
            if let Err(error) = controller.fork_charge() {
                for ancestor in controllers {
                    ancestor.note_fork_failure();
                }
                return Err(error);
            }
            charge
                .charges
                .push(ChargeGuard::new(Arc::clone(controller), 1));
        }
        Ok(charge)
    }

    /// Commits the reservation for a stable task identity.
    ///
    /// # Errors
    ///
    /// Returns `EEXIST` when that identity already has membership in this
    /// cgroup. The reserved controller charge is rolled back on failure.
    ///
    /// # Panics
    ///
    /// Panics if not every live controller was charged or the reserved node is
    /// inactive. A successful reservation and its lifecycle reservation
    /// maintain these invariants until commit or rollback.
    pub fn commit(mut self, token: TaskIdentityToken) -> KResult<TaskMembership> {
        let cgroup = self.cgroup.clone();
        let mut charges = ChargeSet::new();
        core::mem::swap(&mut charges, &mut self.charges);
        {
            let _transaction = cgroup.hierarchy.transaction.lock();
            assert!(
                cgroup.is_active(),
                "committing a task charge to inactive cgroup"
            );
            if !charges.matches_lineage(&cgroup.controller_lineage()) {
                // A concurrent subtree_control transition changed the
                // lineage after the reservation. Fail the commit without
                // panicking; Drop releases the reserved controller charges.
                return Err(KError::from(LinuxError::EAGAIN));
            }
            let mut member_tasks = cgroup.member_tasks.lock();
            if !member_tasks.insert(token) {
                return Err(KError::from(LinuxError::EEXIST));
            }
            cgroup.reservations.fetch_sub(1, Ordering::AcqRel);
        }
        self.reservation_held = false;
        charges.commit();
        Ok(TaskMembership {
            token,
            cgroup: SpinNoIrq::new(Some(cgroup)),
        })
    }
}

impl Drop for TaskCharge {
    fn drop(&mut self) {
        if self.reservation_held {
            self.cgroup.reservations.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Canonical cgroup membership owned by one task/thread.
pub struct TaskMembership {
    token: TaskIdentityToken,
    cgroup: SpinNoIrq<Option<Arc<Cgroup>>>,
}

impl TaskMembership {
    /// Returns the current cgroup node.
    pub fn cgroup(&self) -> Option<Arc<Cgroup>> {
        self.cgroup.lock().clone()
    }

    /// Moves an existing task without applying `pids.max` to the target.
    ///
    /// # Errors
    ///
    /// Propagates [`Self::migrate_group`] errors: `ENOENT` for an inactive
    /// target, `EBUSY` for a non-root target that delegates a controller,
    /// hierarchy, and `EAGAIN` for controller-count overflow.
    pub fn migrate(&self, target: &Arc<Cgroup>) -> KResult<()> {
        Self::migrate_group(&[self], target)
    }

    /// Moves a fixed set of tasks as one membership transaction.
    ///
    /// # Errors
    ///
    /// Returns `ENOENT` for an inactive target, `EBUSY` if a non-root target
    /// delegates any controller, `EXDEV` for any attached source in
    /// another hierarchy, or `EAGAIN` on controller-count overflow. Validation
    /// precedes mutation and failed additions are rolled back. Detached
    /// members are skipped.
    pub fn migrate_group(memberships: &[&Self], target: &Arc<Cgroup>) -> KResult<()> {
        let _transaction = target.hierarchy.transaction.lock();
        if !target.is_active() {
            return Err(KError::from(LinuxError::ENOENT));
        }
        // A non-root node that has delegated a controller to its children may
        // not gain direct members: doing so would turn an internal node into a
        // leaf for a domain controller and bypass the subtree policy that the
        // delegated child controllers enforce.
        if !target.name.is_empty() && target.has_delegated_controllers() {
            return Err(KError::from(LinuxError::EBUSY));
        }
        let mut ordered = memberships.to_vec();
        ordered.sort_unstable_by_key(|membership| membership.token);
        ordered.dedup_by_key(|membership| membership.token);

        // Snapshot the source cgroups without holding the per-membership
        // SpinNoIrq guards across controller-lineage allocation or the
        // hierarchy/member Mutexes.
        let current: Vec<Option<Arc<Cgroup>>> = ordered
            .iter()
            .map(|membership| membership.cgroup.lock().clone())
            .collect();
        let target_lineage = target.controller_lineage();
        // Validate every source before mutating any controller count. This
        // keeps a mixed-hierarchy request atomic on its error path.
        let mut additions: Vec<Arc<dyn Controller>> = Vec::new();
        for source in &current {
            if let Some(source_cgroup) = source.as_ref()
                && !Arc::ptr_eq(&source_cgroup.hierarchy, &target.hierarchy)
            {
                return Err(KError::from(LinuxError::EXDEV));
            }
        }
        for source in &current {
            let Some(source_cgroup) = source.as_ref() else {
                continue;
            };
            if Arc::ptr_eq(source_cgroup, target) {
                continue;
            }
            let source_lineage = source_cgroup.controller_lineage();
            let shared = source_lineage
                .iter()
                .zip(&target_lineage)
                .take_while(|(source, target)| Arc::ptr_eq(source, target))
                .count();
            for controller in &target_lineage[shared..] {
                if let Err(error) = controller.migrate_in(1) {
                    for added in additions.iter().rev() {
                        added.migrate_out(1);
                    }
                    return Err(error);
                }
                additions.push(controller.clone());
            }
        }

        for (membership, source) in ordered.into_iter().zip(current.iter()) {
            let Some(source_cgroup) = source.as_ref() else {
                continue;
            };
            if Arc::ptr_eq(source_cgroup, target) {
                continue;
            }
            let source_lineage = source_cgroup.controller_lineage();
            let shared = source_lineage
                .iter()
                .zip(&target_lineage)
                .take_while(|(source, target)| Arc::ptr_eq(source, target))
                .count();
            target.member_tasks.lock().insert(membership.token);
            source_cgroup.member_tasks.lock().remove(&membership.token);
            for controller in source_lineage[shared..].iter().rev() {
                controller.migrate_out(1);
            }
            let mut cgroup = membership.cgroup.lock();
            if cgroup
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, source_cgroup))
            {
                *cgroup = Some(target.clone());
            }
        }
        Ok(())
    }

    /// Reserves a child task in the same cgroup.
    ///
    /// # Errors
    ///
    /// Returns `ESRCH` if membership is detached; otherwise propagates
    /// [`Cgroup::reserve_task`] errors (`ENOENT` or `EAGAIN`).
    pub fn reserve_child(&self) -> KResult<TaskCharge> {
        let cgroup = self
            .cgroup
            .lock()
            .clone()
            .ok_or_else(|| KError::from(LinuxError::ESRCH))?;
        cgroup.reserve_task()
    }

    /// Detaches an exiting task and releases its hierarchical charge.
    pub fn detach(&self) {
        self.do_detach();
    }

    fn do_detach(&self) {
        let cgroup_snapshot = self.cgroup.lock().clone();
        let Some(cgroup_snapshot) = cgroup_snapshot else {
            return;
        };
        let _transaction = cgroup_snapshot.hierarchy.transaction.lock();
        let Some(cgroup) = self.cgroup.lock().take() else {
            return;
        };
        cgroup.member_tasks.lock().remove(&self.token);
        for controller in cgroup.controller_lineage().iter().rev() {
            controller.migrate_out(1);
        }
    }
}

impl Drop for TaskMembership {
    fn drop(&mut self) {
        self.do_detach();
    }
}
