// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX IPC syscall implementations.
//!
//! - Message queues (msgget, msgsnd, msgrcv, msgctl)
//! - Shared memory (shmget, shmat, shmdt, shmctl)
//!
//! Dispatch user requests through [`sys_msgget`] / [`sys_shmget`] and their
//! transfer/control entry points in sleepable process context. Process teardown
//! uses [`ShmManager::clear_proc_shm`] after releasing address-space ownership.
//! A standalone queue sequence needs no user runtime:
//!
//! ```no_run
//! let mut queue = posix_ipc::MessageQueue::new(1, 0o600, 1, 0, 0);
//! queue.enqueue_message(1, b"hello".to_vec()).unwrap();
//! let message = queue.remove_message_by_index(0).unwrap();
//! assert_eq!(message.data, b"hello");
//! ```
//!
//! Queue creation reads kernel realtime, so this example requires the project's
//! kernel test environment; host execution is not a supported validation path.

#![no_std]

#[macro_use]
extern crate klogger;

extern crate alloc;

mod msg;
mod shm;

use alloc::sync::Arc;
use core::sync::atomic::{AtomicI32, Ordering};

#[cfg(unittest)]
use kcred::initial_user_namespace;
use kcred::{CAP_IPC_OWNER, Cred, UserNamespace};
use linux_raw_sys::general::__kernel_time_t;
use posix_types::IpcPerm;

pub use self::{msg::*, shm::*};

static IPC_ID: AtomicI32 = AtomicI32::new(0);

fn next_ipc_id() -> i32 {
    IPC_ID.fetch_add(1, Ordering::Relaxed)
}

#[inline]
fn current_unix_seconds() -> __kernel_time_t {
    ktime::realtime().unix_seconds() as __kernel_time_t
}

// IPC command constants
const IPC_PRIVATE: i32 = 0;
const IPC_CREAT: i32 = 0o1000;
const IPC_EXCL: i32 = 0o2000;
const IPC_RMID: i32 = 0;
const IPC_SET: i32 = 1;
const IPC_STAT: i32 = 2;
const IPC_INFO: i32 = 3;
const MSG_STAT: i32 = 11;
const MSG_INFO: i32 = 12;

/// Owner, group, and other read bits for an IPC permission check.
pub(crate) const IPC_READ: u32 = 0o444;
/// Owner, group, and other write bits for an IPC permission check.
pub(crate) const IPC_WRITE: u32 = 0o222;

/// Applies Linux `ipcperms()` to `perm` for the requested access bits in `flag`.
///
/// Both the creator and current owner IDs select the owner class; both creator
/// and current group IDs select the group class through the filesystem GID and
/// supplementary groups. The requested class bits are folded out of `flag`, so
/// callers pass the raw operation flags (including `msgget` mode bits). Effective
/// `CAP_IPC_OWNER` overrides a denial; UID zero alone grants no authority.
#[cfg(unittest)]
fn has_ipc_permission(perm: &IpcPerm, cred: &Cred, flag: u32) -> bool {
    has_ipc_permission_in(perm, cred, flag, &initial_user_namespace())
}

/// Namespace-aware form used by syscall paths that know the queue owner.
fn has_ipc_permission_in(
    perm: &IpcPerm,
    cred: &Cred,
    flag: u32,
    owner_ns: &Arc<UserNamespace>,
) -> bool {
    let requested = (flag >> 6) | (flag >> 3) | flag;
    let euid = cred.euid();
    let granted = if euid == perm.cuid || euid == perm.uid {
        perm.mode >> 6
    } else if cred.in_group(perm.cgid) || cred.in_group(perm.gid) {
        perm.mode >> 3
    } else {
        perm.mode
    };

    if requested & !granted & 0o7 == 0 {
        return true;
    }

    cred.has_cap_in(owner_ns, CAP_IPC_OWNER)
}

/// Applies Linux `ipcctl_obtain_check()` control authority to `perm`.
///
/// Queue control is reserved for the effective UID that matches the current
/// owner or creator, with effective `CAP_SYS_ADMIN` as the privileged override.
/// `CAP_IPC_OWNER` only overrides read/write access and is deliberately absent.
#[cfg(unittest)]
fn has_ipc_control_permission(perm: &IpcPerm, cred: &Cred) -> bool {
    has_ipc_control_permission_in(perm, cred, &initial_user_namespace())
}

/// Namespace-aware form used by syscall paths that know the queue owner.
fn has_ipc_control_permission_in(
    perm: &IpcPerm,
    cred: &Cred,
    owner_ns: &Arc<UserNamespace>,
) -> bool {
    let euid = cred.euid();
    euid == perm.cuid || euid == perm.uid || cred.has_cap_in(owner_ns, kcred::CAP_SYS_ADMIN)
}
