// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cgroup v2 hierarchy, controller admission, and stable task membership.
//!
//! Use [`CgroupNamespace::initial`] for the system hierarchy and
//! [`CgroupNamespace::clone_for`] to create a `CLONE_NEWCGROUP` view. Adapters
//! must supply authorization; they must not keep parallel membership or
//! controller state. Reserve a [`TaskCharge`] before task publication and commit
//! it to a stable identity, or drop it to roll back. APIs require kernel
//! allocation and sleepable locks; migration intentionally ignores new-task pids
//! limits.
//!
//! Controllers whose enforcement hooks live in another subsystem are listed in
//! [`ControllerKind`] but report [`ControllerKind::is_implemented`] as `false`.
//! They never appear in `cgroup.controllers` and never materialize control files.
//!
//! # Example
//!
//! ```no_run
//! use kcgroup::CgroupNamespace;
//! let namespace = CgroupNamespace::new();
//! let root = namespace.root();
//! root.set_pids_subtree_enabled(true).unwrap();
//! let group = root.create_child("worker").unwrap();
//! let operation = group.begin_operation().unwrap();
//! group.set_pids_max(Some(1)).unwrap();
//! drop(operation);
//! let identity = kidentity::TaskIdentityToken::allocate();
//! let member = group.reserve_task().unwrap().commit(identity).unwrap();
//! assert_eq!(group.pids_current(), Ok(1));
//! member.detach();
//! root.remove_child("worker").unwrap();
//! ```

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use alloc::sync::Arc;

use kbandwidth::Bandwidth;
use kspin::{SpinNoIrq, static_lock};

/// Optional scheduler callback invoked before a cpu tracker is discarded.
pub type BandwidthReleaseHook = fn(&Arc<Bandwidth>);

static_lock! {
    static BANDWIDTH_RELEASE_HOOK: SpinNoIrq<Option<BandwidthReleaseHook>> = SpinNoIrq::new(None);
}

/// Installs the scheduler's bandwidth-release callback once.
pub fn install_bandwidth_release_hook(hook: BandwidthReleaseHook) {
    let mut slot = BANDWIDTH_RELEASE_HOOK.lock();
    if slot.is_none() {
        *slot = Some(hook);
    }
}

pub(crate) fn release_bandwidth(tracker: &Arc<Bandwidth>) {
    // Copy the function pointer out before calling it: CG-013 forbids holding
    // the SpinNoIrq guard across scheduler code.
    let hook = {
        let slot = BANDWIDTH_RELEASE_HOOK.lock();
        *slot
    };
    if let Some(hook) = hook {
        hook(tracker);
    }
}

mod controller;
mod hierarchy;
mod membership;
mod namespace;

pub use controller::{
    ChargeGuard, ChargeSet, Controller, ControllerKind, ControllerSet, ControllerSlot,
    ControllerState, CpuController, CpuMask, CpusetController, MAX_CPUS, MemoryController,
    NodeMask, PIDS_MAX_LIMIT, PidsController,
};
pub use hierarchy::Cgroup;
pub use kbandwidth::{
    DEFAULT_PERIOD_US as DEFAULT_PERIOD, PERIOD_MAX_US, PERIOD_MIN_US, QUOTA_MIN_US,
};
pub use membership::{CgroupOperationGuard, TaskCharge, TaskMembership};
pub use namespace::{CgroupNamespace, CgroupNamespaceId, refresh_root_cpuset_universe};

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
