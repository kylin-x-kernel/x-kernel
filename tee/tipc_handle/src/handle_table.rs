// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    collections::{BTreeMap, btree_map::Entry},
    sync::Arc,
    vec::Vec,
};

use kerrno::{KError, KResult};
use kpoll::{PollContext, PollRegisterError};
use smallvec::SmallVec;

use crate::{Handle, HandleSet, HandleWaitState};

/// Immutable handle list shared across `wait_any` poll attempts.
type WaitAnySnapshot = Arc<[(i32, Arc<dyn Handle>)]>;

/// Process-local mapping from integer IDs to TIPC kernel objects.
#[derive(Default)]
pub struct HandleTable {
    next_id: i32,
    handles: BTreeMap<i32, Arc<dyn Handle>>,
    handle_set_ids: SmallVec<[i32; 4]>,
    wait_any_snapshot: Option<WaitAnySnapshot>,
    // `wait_any` also waits for membership changes: a newly installed handle
    // might already be ready even though every handle in its prior snapshot
    // was not. This wait state wakes those pollers after cache invalidation.
    wait_any_handle: HandleWaitState,
}

impl HandleTable {
    /// Creates an empty table.
    ///
    /// Not `const`: [`HandleWaitState`] embeds a [`kpoll::PollSet`] that
    /// allocates on construction.
    pub fn new() -> Self {
        Self {
            next_id: 0,
            handles: BTreeMap::new(),
            handle_set_ids: SmallVec::new_const(),
            wait_any_snapshot: None,
            wait_any_handle: HandleWaitState::new(),
        }
    }

    /// Installs a handle and returns its process-local ID.
    pub fn uctx_handle_install(&mut self, handle: Arc<dyn Handle>) -> KResult<i32> {
        let is_handle_set = handle.as_any().is::<HandleSet>();
        let start = self.next_id.max(0);
        let mut id = start;
        loop {
            if let Entry::Vacant(entry) = self.handles.entry(id) {
                entry.insert(handle);
                if is_handle_set {
                    self.handle_set_ids.push(id);
                }
                self.next_id = id.checked_add(1).unwrap_or(0);
                self.invalidate_wait_any_snapshot();
                return Ok(id);
            }
            id = id.checked_add(1).unwrap_or(0);
            // start -> i32::MAX -> 0 -> start is a full cycle, so we have no free IDs.
            if id == start {
                return Err(KError::TooManyOpenFiles);
            }
        }
    }

    /// Returns a strong reference to an installed handle.
    pub fn uctx_handle_get(&self, id: i32) -> KResult<Arc<dyn Handle>> {
        self.handles
            .get(&id)
            .cloned()
            .ok_or(KError::BadFileDescriptor)
    }

    /// Removes and closes one handle.
    pub fn uctx_handle_remove(&mut self, id: i32) -> KResult {
        let handle = self.uctx_handle_uninstall(id)?;
        handle.close();
        Ok(())
    }

    /// Closes and removes every handle owned by this process-local table.
    ///
    /// This is used during exec, where no TIPC handle has an inheritable or
    /// close-on-exec flag.  Each handle is explicitly closed so ports are
    /// unpublished and channel peers are notified before their references are
    /// dropped.
    pub fn uctx_handle_close_all(&mut self) {
        let handles = core::mem::take(&mut self.handles);
        self.handle_set_ids.clear();
        self.next_id = 0;
        self.invalidate_wait_any_snapshot();
        for (_, handle) in handles {
            handle.close();
        }
    }

    /// Removes one handle table entry without closing the underlying object.
    ///
    /// This is used when a syscall installed handles speculatively and then
    /// failed before returning the new identifiers to userspace.
    pub fn uctx_handle_uninstall(&mut self, id: i32) -> KResult<Arc<dyn Handle>> {
        let handle = self.handles.remove(&id).ok_or(KError::BadFileDescriptor)?;
        if handle.as_any().is::<HandleSet>() {
            self.handle_set_ids.retain(|hset_id| *hset_id != id);
        }
        self.detach_from_handle_sets(id);
        self.invalidate_wait_any_snapshot();
        Ok(handle)
    }

    fn detach_from_handle_sets(&self, id: i32) {
        for hset_id in &self.handle_set_ids {
            if let Some(hset) = self
                .handles
                .get(hset_id)
                .and_then(|handle| handle.as_any().downcast_ref::<HandleSet>())
            {
                hset.remove_handle_id(id);
            }
        }
    }

    /// Returns the cached handle list used by `wait_any`.
    ///
    /// The cache is rebuilt only after the table changes, so repeated wait
    /// polling does not clone every handle on every pass.
    pub fn wait_any_snapshot(&mut self) -> WaitAnySnapshot {
        if let Some(snapshot) = &self.wait_any_snapshot {
            return snapshot.clone();
        }

        let snapshot: WaitAnySnapshot = Arc::from(
            self.handles
                .iter()
                .map(|(id, handle)| (*id, handle.clone()))
                .collect::<Vec<_>>(),
        );
        self.wait_any_snapshot = Some(snapshot.clone());
        snapshot
    }

    /// Registers the current task for handle-table membership changes.
    ///
    /// Callers must recheck readiness after registering. An install or removal
    /// may race with registration, and the recheck turns that race into an
    /// immediate result instead of a missed wakeup.
    pub fn register_wait_any_table_change(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        self.wait_any_handle.register(context)
    }

    fn invalidate_wait_any_snapshot(&mut self) {
        // A stale snapshot could omit a newly ready handle or retain a removed
        // one. Wake waiters so they retry with a snapshot matching the table.
        self.wait_any_snapshot = None;
        self.wait_any_handle.notify();
    }
}
