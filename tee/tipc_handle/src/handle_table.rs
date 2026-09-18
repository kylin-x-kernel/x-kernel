// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-local handle IDs, explicit close paths, and cached wait snapshots.

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

/// Process-local mapping from nonnegative integer IDs to shared TIPC objects.
///
/// Callers select the owning process and synchronize access. Mutations may
/// allocate and invoke callbacks under caller-held locks. IDs are reusable
/// and local to this table; multiple IDs may refer to one object.
///
/// Drop only releases references. Use [`Self::uctx_handle_close_all`] for
/// explicit shutdown; see the [crate example](crate#examples).
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
    /// Creates an empty table, allocating its membership notification source.
    pub fn new() -> Self {
        Self {
            next_id: 0,
            handles: BTreeMap::new(),
            handle_set_ids: SmallVec::new_const(),
            wait_any_snapshot: None,
            wait_any_handle: HandleWaitState::new(),
        }
    }

    /// Installs a strong reference and returns a free process-local ID.
    ///
    /// Wraps from `i32::MAX` to zero. Success invalidates the wait cache and
    /// notifies table waiters; no sendability or object-uniqueness check is made.
    ///
    /// # Errors
    ///
    /// [`KError::TooManyOpenFiles`] if every nonnegative ID is occupied.
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

    /// Clones the strong reference associated with `id` in this table.
    ///
    /// Keeps the object allocated after removal, but does not prevent close.
    ///
    /// # Errors
    ///
    /// [`KError::BadFileDescriptor`] if `id` is absent or negative.
    pub fn uctx_handle_get(&self, id: i32) -> KResult<Arc<dyn Handle>> {
        self.handles
            .get(&id)
            .cloned()
            .ok_or(KError::BadFileDescriptor)
    }

    /// Uninstalls an entry, then explicitly closes its underlying object.
    ///
    /// Calls [`Self::uctx_handle_uninstall`] before [`Handle::close`].
    ///
    /// # Errors
    ///
    /// [`KError::BadFileDescriptor`] if `id` is absent; no close occurs.
    pub fn uctx_handle_remove(&mut self, id: i32) -> KResult {
        let handle = self.uctx_handle_uninstall(id)?;
        handle.close();
        Ok(())
    }

    /// Closes and removes every handle owned by this process-local table.
    ///
    /// Empties the table, resets ID allocation and notifies waiters before
    /// closing objects. Used for exec/exit cleanup; retained references and
    /// snapshots remain allocated but observe closed objects.
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
    /// Detaches the ID from sets still in the table, invalidates the wait cache,
    /// and notifies waiters. Used for speculative-install rollback; the caller
    /// assumes close responsibility. A removed set retains its own entries.
    ///
    /// # Errors
    ///
    /// [`KError::BadFileDescriptor`] if `id` is absent.
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
    /// Returns an ID-ordered `Arc<[(i32, Arc<dyn Handle>)]>`, reused until the
    /// table changes. Old snapshots retain removed/closed objects; fetch a new
    /// snapshot after notification. See the [wait example](Self::register_wait_any_table_change).
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
    /// Also register on each object for its events. Retain registrations while
    /// pending and recheck readiness after registering to avoid missed wake-ups.
    ///
    /// # Errors
    ///
    /// Propagates [`HandleWaitState::register`] errors: allocation failure or
    /// exhausted registration IDs.
    ///
    /// # Examples
    ///
    /// One wait round with exclusive table access. Keep `owner` across pending
    /// rounds (`None`), clear it on completion/error, and release table locks
    /// before yielding. An empty table waits for a later installation.
    ///
    /// ```no_run
    /// use core::task::Context;
    ///
    /// use kpoll::{PollRegisterError, PollRegistrations};
    /// use tipc_handle::{HandleEventMask, HandleTable};
    ///
    /// fn poll_any(
    ///     table: &mut HandleTable,
    ///     mask: HandleEventMask,
    ///     owner: &mut PollRegistrations,
    ///     cx: &Context<'_>,
    /// ) -> Result<Option<(i32, HandleEventMask)>, PollRegisterError> {
    ///     let mut context = owner.context(cx);
    ///     table.register_wait_any_table_change(&mut context)?;
    ///     for (_, handle) in table.wait_any_snapshot().iter() {
    ///         handle.register(&mut context, mask)?;
    ///     }
    ///     // Recheck after all registrations; do not consume events here.
    ///     Ok(table.wait_any_snapshot().iter().find_map(|(id, handle)| {
    ///         let events = handle.poll(false) & mask;
    ///         (!events.is_empty()).then_some((*id, events))
    ///     }))
    /// }
    /// ```
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
