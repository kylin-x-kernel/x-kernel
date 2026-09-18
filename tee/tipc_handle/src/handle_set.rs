// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Handle-set registration commands and event selection over shared objects.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::any::Any;

use kerrno::{KError, KResult};
use kpoll::{PollContext, PollRegisterError};
use kspin::SpinNoIrq;

use crate::{Handle, HandleEventMask, HandleKind, HandleWaitState};

/// Add a handle to a handle set.
pub const HSET_ADD: u32 = 0;
/// Delete a handle from a handle set.
pub const HSET_DEL: u32 = 1;
/// Modify a handle-set entry.
pub const HSET_MOD: u32 = 2;
/// Delete an entry and return its cookie.
pub const HSET_DEL_GET_COOKIE: u32 = 3;
/// Delete an entry only when its cookie matches.
pub const HSET_DEL_WITH_COOKIE: u32 = 4;
/// Modify an entry only when its cookie matches.
pub const HSET_MOD_WITH_COOKIE: u32 = 5;

/// One returned handle event.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UEvent {
    /// Process-local handle identifier.
    pub handle: i32,
    /// Events observed on the handle.
    pub event: HandleEventMask,
    /// Opaque registration cookie.
    pub cookie: usize,
}

/// Operation applied by [`HandleSet::handle_set_ctrl`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandleSetCommand {
    /// Add a new handle.
    Add,
    /// Delete a handle.
    Delete,
    /// Modify event mask and cookie.
    Modify,
    /// Delete and return the stored cookie.
    DeleteGetCookie,
    /// Delete only if the cookie matches.
    DeleteWithCookie,
    /// Modify only if the cookie matches.
    ModifyWithCookie,
}

/// Registration stored in a handle set.
///
/// Callers must pair the ID and object from the set's process-local table;
/// this association is not checked. The cookie is separate from [`Handle::cookie`].
#[derive(Clone)]
pub struct HandleSetEntry {
    /// Process-local handle identifier returned in events.
    pub handle_id: i32,
    /// Underlying TIPC object.
    pub handle: Arc<dyn Handle>,
    /// Requested event mask.
    pub event: HandleEventMask,
    /// Opaque registration cookie.
    pub cookie: usize,
}

/// TIPC event multiplexer over registrations keyed by process-local handle ID.
///
/// Owns strong child references under an IRQ-disabling lock; child `kind` and
/// `poll` methods must not block or re-enter the set. Nested sets are rejected.
/// As a [`Handle`], reports `READY` for matching child events without consuming
/// them and forbids transfer. Closing clears entries and wakes waiters, but
/// neither closes children nor prevents reuse of the set.
pub struct HandleSet {
    entries: SpinNoIrq<BTreeMap<i32, HandleSetEntry>>,
    handle: HandleWaitState,
}

impl HandleSet {
    /// Creates an empty handle set.
    pub fn handle_set_create() -> Arc<Self> {
        Arc::new(Self {
            entries: SpinNoIrq::new(BTreeMap::new()),
            handle: HandleWaitState::new(),
        })
    }

    /// Adds, removes, or replaces a registration and notifies set waiters.
    ///
    /// Add/modify store all fields. Cookie-checked commands require the stored
    /// cookie, so `ModifyWithCookie` cannot change it. Deletes use only the ID
    /// and, for `DeleteWithCookie`, cookie; they do not close the object.
    /// See [`HandleSetEntry`] for the caller's ID/object obligations.
    ///
    /// Returns the removed cookie for `DeleteGetCookie`, otherwise `None`.
    /// Errors leave entries unchanged and do not notify.
    ///
    /// # Errors
    ///
    /// - [`KError::InvalidInput`]: add/modify supplies a nested set.
    /// - [`KError::AlreadyExists`]: add targets an occupied ID.
    /// - [`KError::NotFound`]: delete/modify targets an absent ID or a
    ///   cookie-checked command has a mismatched cookie.
    ///
    /// # Examples
    ///
    /// Register an existing non-set handle, poll it, then remove its registration.
    ///
    /// ```no_run
    /// use tipc_handle::{HandleEventMask, HandleSet, HandleSetCommand, HandleSetEntry, HandleTable};
    ///
    /// fn observe(table: &HandleTable, id: i32) -> kerrno::KResult {
    ///     let set = HandleSet::handle_set_create();
    ///     let entry = HandleSetEntry {
    ///         handle_id: id,
    ///         handle: table.uctx_handle_get(id)?,
    ///         event: HandleEventMask::MSG,
    ///         cookie: 42,
    ///     };
    ///     set.handle_set_ctrl(HandleSetCommand::Add, entry.clone())?;
    ///     if let Some(event) = set.poll_one()? {
    ///         assert_eq!((event.handle, event.cookie), (id, 42));
    ///     }
    ///     assert_eq!(
    ///         set.handle_set_ctrl(HandleSetCommand::DeleteGetCookie, entry)?,
    ///         Some(42)
    ///     );
    ///     Ok(())
    /// }
    /// ```
    pub fn handle_set_ctrl(
        &self,
        command: HandleSetCommand,
        entry: HandleSetEntry,
    ) -> KResult<Option<usize>> {
        let result = {
            let mut entries = self.entries.lock();
            let current = entries.get(&entry.handle_id);
            match command {
                HandleSetCommand::Add => {
                    if entry.handle.kind() == HandleKind::HandleSet {
                        return Err(KError::InvalidInput);
                    }
                    if current.is_some() {
                        return Err(KError::AlreadyExists);
                    }
                    entries.insert(entry.handle_id, entry);
                    Ok(None)
                }
                HandleSetCommand::Delete | HandleSetCommand::DeleteGetCookie => {
                    let removed = entries.remove(&entry.handle_id).ok_or(KError::NotFound)?;
                    Ok((command == HandleSetCommand::DeleteGetCookie).then_some(removed.cookie))
                }
                HandleSetCommand::DeleteWithCookie => {
                    if current.is_none_or(|old| old.cookie != entry.cookie) {
                        return Err(KError::NotFound);
                    }
                    entries.remove(&entry.handle_id);
                    Ok(None)
                }
                HandleSetCommand::Modify => {
                    if current.is_none() {
                        return Err(KError::NotFound);
                    }
                    if entry.handle.kind() == HandleKind::HandleSet {
                        return Err(KError::InvalidInput);
                    }
                    entries.insert(entry.handle_id, entry);
                    Ok(None)
                }
                HandleSetCommand::ModifyWithCookie => {
                    if current.is_none_or(|old| old.cookie != entry.cookie) {
                        return Err(KError::NotFound);
                    }
                    if entry.handle.kind() == HandleKind::HandleSet {
                        return Err(KError::InvalidInput);
                    }
                    entries.insert(entry.handle_id, entry);
                    Ok(None)
                }
            }
        };
        if result.is_ok() {
            self.handle.notify();
        }
        result
    }

    /// Returns whether this handle set has no registered handles.
    pub fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }

    pub(crate) fn remove_handle_id(&self, handle_id: i32) {
        let removed = self.entries.lock().remove(&handle_id).is_some();
        if removed {
            self.handle.notify();
        }
    }

    /// Returns the first matching event in ascending handle-ID order.
    ///
    /// Returns `None` if no events match. Polls each visited child with
    /// `finalize = true` before filtering, so unmatched events may be consumed.
    /// The cookie comes from the registration; selection is not fair.
    /// See the [registration example](Self::handle_set_ctrl).
    ///
    /// # Errors
    ///
    /// [`KError::NotFound`] if the set is empty.
    pub fn poll_one(&self) -> KResult<Option<UEvent>> {
        let entries = self.entries.lock();
        if entries.is_empty() {
            return Err(KError::NotFound);
        }
        Ok(entries.values().find_map(|entry| {
            let event = entry.handle.poll(true) & entry.event;
            (!event.is_empty()).then_some(UEvent {
                handle: entry.handle_id,
                event,
                cookie: entry.cookie,
            })
        }))
    }
}

impl Handle for HandleSet {
    fn kind(&self) -> HandleKind {
        HandleKind::HandleSet
    }

    fn poll(&self, _finalize: bool) -> HandleEventMask {
        if self
            .entries
            .lock()
            .values()
            .any(|entry| !(entry.handle.poll(false) & entry.event).is_empty())
        {
            HandleEventMask::READY
        } else {
            HandleEventMask::empty()
        }
    }

    /// Registers for set changes and a snapshot of each child's stored mask.
    ///
    /// Ignores the outer mask. Follow the registration/recheck sequence in the
    /// [wait example](crate::HandleTable::register_wait_any_table_change).
    ///
    /// # Errors
    ///
    /// [`PollRegisterError::NoMemory`] if snapshot allocation fails; otherwise
    /// propagates [`HandleWaitState::register`] and child [`Handle::register`]
    /// errors. Partial registrations remain owned by the caller.
    fn register(
        &self,
        context: &mut PollContext<'_>,
        _event_mask: HandleEventMask,
    ) -> Result<(), PollRegisterError> {
        self.handle.register(context)?;
        let entries = {
            let entries = self.entries.lock();
            let mut snapshot = Vec::new();
            snapshot
                .try_reserve(entries.len())
                .map_err(|_| PollRegisterError::NoMemory)?;
            for entry in entries.values() {
                snapshot.push((Arc::clone(&entry.handle), entry.event));
            }
            snapshot
        };
        for (handle, event) in entries {
            handle.register(context, event)?;
        }
        Ok(())
    }

    fn close(&self) {
        self.entries.lock().clear();
        self.handle.notify();
    }

    fn set_cookie(&self, cookie: usize) {
        self.handle.set_cookie(cookie);
    }

    fn cookie(&self) -> usize {
        self.handle.cookie()
    }

    fn is_sendable(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
