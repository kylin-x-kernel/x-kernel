// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Time namespace offsets.
//!
//! A time namespace shifts only the monotonic and boottime clocks. Wall-clock
//! (`CLOCK_REALTIME`) time is global, and the CPU-time clocks count work rather
//! than elapsed time, so neither carries a namespace offset.

use alloc::sync::Arc;

use kcred::{NamespaceId, UserNamespace, initial_user_namespace};
use kspin::SpinNoIrq;
use ktime_types::TimeSpec;

/// The pair of clock offsets owned by one time namespace.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TimeNamespaceOffsets {
    /// Offset added to `CLOCK_MONOTONIC` (and its coarse/raw variants).
    pub monotonic: TimeSpec,
    /// Offset added to `CLOCK_BOOTTIME`.
    pub boottime: TimeSpec,
}

/// Time namespace.
///
/// A namespace starts writable and freezes permanently when its first member
/// enters it. Offsets are stored together so a reader never observes a
/// half-updated pair.
pub struct TimeNamespace {
    id: NamespaceId,
    owner: Arc<UserNamespace>,
    state: SpinNoIrq<TimeNamespaceState>,
}

/// Mutable time-namespace state guarded as one unit.
///
/// Offsets and the frozen flag must be read and updated under the same lock:
/// Linux checks `frozen_offsets` and replaces the offsets while holding
/// `offset_lock`, so a write can never use a stale membership decision.
struct TimeNamespaceState {
    offsets: TimeNamespaceOffsets,
    frozen_offsets: bool,
}

impl Default for TimeNamespace {
    fn default() -> Self {
        Self::new()
    }
}

impl TimeNamespace {
    /// Creates a time namespace with zero offsets.
    pub fn new() -> Self {
        Self::new_in(initial_user_namespace())
    }

    /// Creates a writable time namespace owned by `owner`.
    ///
    /// Linux stores the owning user namespace on every time namespace and
    /// uses it for `CAP_SYS_TIME` checks on `/proc/<pid>/timens_offsets`.
    pub fn new_in(owner: Arc<UserNamespace>) -> Self {
        Self::with_owner_and_offsets(owner, TimeNamespaceOffsets::default())
    }

    /// Creates the initial time namespace, whose offsets are permanently
    /// frozen because it already has members.
    pub fn initial() -> Self {
        let namespace = Self::new_in(initial_user_namespace());
        namespace.freeze_offsets();
        namespace
    }

    /// Creates a time namespace with the supplied offsets.
    ///
    /// Used by tests; runtime adjustment goes through
    /// [`Self::try_set_offsets`] so the frozen check is atomic with the write.
    pub fn with_offsets(offsets: TimeNamespaceOffsets) -> Self {
        Self::with_owner_and_offsets(initial_user_namespace(), offsets)
    }

    fn with_owner_and_offsets(owner: Arc<UserNamespace>, offsets: TimeNamespaceOffsets) -> Self {
        Self {
            id: NamespaceId::new(),
            owner,
            state: SpinNoIrq::new(TimeNamespaceState {
                offsets,
                frozen_offsets: false,
            }),
        }
    }

    /// Returns the namespace ID.
    pub fn id(&self) -> NamespaceId {
        self.id
    }

    /// Returns the user namespace that owns this time namespace.
    pub fn owner(&self) -> &Arc<UserNamespace> {
        &self.owner
    }

    /// Returns the current offsets as one consistent snapshot.
    pub fn offsets(&self) -> TimeNamespaceOffsets {
        self.state.lock().offsets
    }

    /// Returns the offset applied to `CLOCK_MONOTONIC`.
    pub fn monotonic_offset(&self) -> TimeSpec {
        self.state.lock().offsets.monotonic
    }

    /// Returns the offset applied to `CLOCK_BOOTTIME`.
    pub fn boottime_offset(&self) -> TimeSpec {
        self.state.lock().offsets.boottime
    }

    /// Returns whether the offsets can no longer be changed.
    pub fn has_frozen_offsets(&self) -> bool {
        self.state.lock().frozen_offsets
    }

    /// Freezes the offsets because the namespace gained its first member.
    ///
    /// Linux `timens_set_vvar_page()` performs this transition when a task
    /// first enters a non-initial time namespace; from then on writes to
    /// `/proc/<pid>/timens_offsets` fail with `EACCES`.
    pub fn freeze_offsets(&self) {
        self.state.lock().frozen_offsets = true;
    }

    /// Replaces both offsets unless they have been frozen.
    ///
    /// The frozen check and the update share one lock, so a concurrent first
    /// task entry cannot interleave between them. Returns `false` when the
    /// offsets are frozen.
    pub fn try_set_offsets(&self, offsets: TimeNamespaceOffsets) -> bool {
        self.try_update_offsets(|current| *current = offsets)
    }

    /// Updates offsets in place unless they have been frozen.
    ///
    /// Reading and replacing one or both fields under the same lock preserves
    /// Linux's per-write atomicity of `/proc/<pid>/timens_offsets`, which may
    /// change only one clock and must not lose a concurrent update to the
    /// other. Returns `false` when the offsets are frozen.
    pub fn try_update_offsets(&self, update: impl FnOnce(&mut TimeNamespaceOffsets)) -> bool {
        let mut state = self.state.lock();
        if state.frozen_offsets {
            return false;
        }
        update(&mut state.offsets);
        true
    }
}
