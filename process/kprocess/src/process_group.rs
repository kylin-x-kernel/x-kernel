// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process group management.
use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::fmt;

use kidentity::{PidHandle, PidNamespace};
use kspin::SpinNoIrq;

use crate::{Pid, Process, Session};

pub(crate) struct ProcessGroupMemberSlot {
    process: SpinNoIrq<Option<Weak<Process>>>,
}

impl ProcessGroupMemberSlot {
    fn new() -> Self {
        Self {
            process: SpinNoIrq::new(None),
        }
    }

    pub(crate) fn publish(&self, process: &Arc<Process>) {
        *self.process.lock() = Some(Arc::downgrade(process));
    }

    pub(crate) fn retire(&self) {
        *self.process.lock() = None;
    }

    fn snapshot(&self) -> Option<Arc<Process>> {
        self.process.lock().as_ref().and_then(Weak::upgrade)
    }
}

/// A [`ProcessGroup`] is a collection of [`Process`]es.
pub struct ProcessGroup {
    pgid: Pid,
    leader_number: Arc<PidHandle>,
    pub(crate) session: Arc<Session>,
    pub(crate) processes: SpinNoIrq<BTreeMap<Pid, Arc<ProcessGroupMemberSlot>>>,
}

impl ProcessGroup {
    /// Creates an empty group in a private session for isolated job-control cases.
    /// Neither identity is published, and no live process membership is changed.
    #[cfg(unittest)]
    pub fn new_isolated_for_test() -> Arc<Self> {
        let leader_number = PidHandle::fixed_root(0);
        Self::new(0, &Session::new(0, leader_number.clone()), leader_number)
    }

    /// Create a new [`ProcessGroup`] within a [`Session`].
    pub(crate) fn new(
        pgid: Pid,
        session: &Arc<Session>,
        leader_number: Arc<PidHandle>,
    ) -> Arc<Self> {
        let group = Arc::new(Self {
            pgid,
            leader_number,
            session: session.clone(),
            processes: SpinNoIrq::new(BTreeMap::new()),
        });
        session.process_groups.lock().insert(pgid, &group);
        group
    }

    pub(crate) fn reserve_process_slot(&self, pid: Pid) -> Arc<ProcessGroupMemberSlot> {
        self.processes
            .lock()
            .entry(pid)
            .or_insert_with(|| Arc::new(ProcessGroupMemberSlot::new()))
            .clone()
    }
}

impl ProcessGroup {
    /// The [`ProcessGroup`] ID.
    pub fn pgid(&self) -> Pid {
        self.pgid
    }

    /// Returns this group's leader number in `observer_ns`.
    ///
    /// Linux renders `/proc/<pid>/stat` fields 5 and 6 with
    /// `task_pgrp_nr_ns`/`task_session_nr_ns`; the leader handle keeps the
    /// namespace projection available even after the leader exits.
    pub fn number_in_namespace(&self, observer_ns: &Arc<PidNamespace>) -> Option<Pid> {
        self.leader_number.nr_in(observer_ns)
    }

    /// The [`Session`] that the [`ProcessGroup`] belongs to.
    pub fn session(&self) -> Arc<Session> {
        self.session.clone()
    }

    /// The [`Process`]es that belong to this [`ProcessGroup`].
    pub fn processes(&self) -> Vec<Arc<Process>> {
        self.processes
            .lock()
            .values()
            .filter_map(|slot| slot.snapshot())
            .collect()
    }
}

impl fmt::Debug for ProcessGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ProcessGroup({}, session={})",
            self.pgid,
            self.session.sid()
        )
    }
}
