// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Wiring between the filesystem crate and the inotify object crate.
//!
//! `posix_fs` needs to report directory-entry changes, and `kfd_objects::inotify`
//! knows which watches care about them.  Neither can depend on the other:
//! `kfd_objects` already depends on `kvfs`, so `posix_fs -> kfd_objects` would close
//! a cycle.  `posix_fs` therefore exposes an injectable function pointer, and this
//! module -- in the one crate that already depends on both -- installs it.
//!
//! Kept separate from `dispatch` so the one-time cost is obviously one-time.

use core::sync::atomic::{AtomicBool, Ordering};

/// Whether the notifier has been installed.
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Installs the filesystem notifier exactly once.
///
/// Called from the syscall entry point rather than from a start-up hook because
/// `entry` only depends on this crate, and adding a kernel start-up callback for one
/// pointer would mean touching a layer that gains nothing from it.  Once the flag
/// is set, every later call is one relaxed load on the syscall path.
pub(crate) fn install_fs_notifier() {
    // The syscall path calls this on every entry, so the completed case must
    // not pay an atomic read-modify-write: `swap` would take the cache line
    // exclusively on every syscall on every CPU, contending with no relation
    // to whether any watch exists. A relaxed load first leaves the swap to the
    // one call that actually installs.
    if INSTALLED.load(Ordering::Acquire) {
        return;
    }
    // Install before publishing the flag: a concurrent event that observes
    // `INSTALLED` must already find the pointers in place, and re-storing the
    // same `fn` items is idempotent, so several installers racing here all
    // write the identical values.
    posix_fs::notify::set_entry_notifier(kfd_objects::inotify::notify_path);
    posix_fs::notify::set_watch_probe(kfd_objects::inotify::any_watch_registered);
    posix_fs::notify::set_retire_notifier(kfd_objects::inotify::retire_entry);
    posix_fs::notify::set_self_notifier(kfd_objects::inotify::notify_self_object);
    INSTALLED.store(true, Ordering::Release);
    debug!("filesystem entry notifier installed");
}
