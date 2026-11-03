// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Directory-entry and content notification hooks for filesystem implementations.
//!
//! `inotify` watches can only see changes if something tells them a directory entry
//! changed.  The natural place to observe that is here, in the syscalls that create
//! and remove entries -- but the watch bookkeeping lives in `kfd_objects`, and
//! `kfd_objects` already depends on `kvfs`, so `posix_fs` importing it would create a
//! cycle.
//!
//! The dependency is therefore inverted: this module owns function pointers that
//! whoever can see both sides (`ksyscall`) installs once at startup.  Until they are
//! installed every notification is a no-op, which is also what makes the filesystem
//! crate usable in unit tests with no inotify instance anywhere.
//!
//! Every entry-changing syscall reports through here: `open(O_CREAT)` and `mkdir`,
//! `mknod`, `symlink` and `link` for creations, `unlinkat` for removals, and
//! `renameat` for a move, which reports the two halves of one event pair.
//! Content operations report the accessed dentry after request completion and
//! release of position/backend locks; `kfd_objects` owns routing and watch policies.
//!
//! Event identity is captured unconditionally, inside the operation's own
//! namespace lock, and never re-derived afterwards. Gating that capture on
//! "is any watch registered" would be cheaper -- the probe is one atomic load --
//! but wrong: a watch can be registered concurrently with the operation, after
//! the gate has already been read, and the capture it skipped is exactly what
//! retires that watch when the object disappears. The probe therefore gates only
//! the work that reports an event. Captures retain inode references plus, where a
//! retirement decision depends on it, one `getattr`.

use alloc::{string::String, sync::Arc};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use kvfs::{Path, VfsFile, VfsInode};

/// Signature of the installed notifier: `(parent_inode, event_mask, entry_name,
/// cookie)`.
///
/// The parent is identified by its inode object rather than by a path, because
/// that is what a watch is *about*: Linux walks the changed entry's parent inode
/// (`fsnotify_parent()`) and reports to the marks hanging on it, so a directory
/// that is renamed keeps its watches and the same object spelt two ways is one
/// watch.
///
/// `cookie` pairs the `IN_MOVED_FROM` and `IN_MOVED_TO` halves of a rename and
/// is 0 for every other event, which is what Linux reports for them.
pub type EntryNotifier = fn(&VfsInode, u32, &[u8], u32);

// The event bits come from `linux_raw_sys`, the same source the object crate and
// the syscall adapter use, so a bit cannot mean one thing here and another
// there.
pub(crate) use linux_raw_sys::general::{
    IN_ATTRIB, IN_CREATE, IN_DELETE, IN_DELETE_SELF, IN_ISDIR, IN_MODIFY, IN_MOVE_SELF,
    IN_MOVED_FROM, IN_MOVED_TO,
};

// `IN_ISDIR` rides along with the event bit the watcher subscribed to, exactly
// as Linux adds `FS_ISDIR` in `fsnotify_parent()`: the check there is
// `test_mask & marks_mask & ~marks_ignore_mask`, so it is part of the event
// rather than part of the subscription.

/// A content-event callback installed by the syscall integration layer.
pub type ContentNotifier = fn(&kvfs::Dentry, u32);

/// The installed content-event router, or 0 when none is installed.
static CONTENT: AtomicUsize = AtomicUsize::new(0);

/// Installs the object-content router from the syscall entry point.
///
/// Publishes the `ContentNotifier` function pointer to `notify_modified`, whose
/// `Acquire` load pairs with this `Release` store. Concurrent installers publish
/// the same function item, so repeating an interrupted installation cannot
/// change the router.
pub fn set_content_notifier(notifier: ContentNotifier) {
    CONTENT.store(notifier as usize, Ordering::Release);
}

/// The installed notifier, or 0 when none is installed.
///
/// A plain `AtomicUsize` rather than a lazily-initialised lock: this is read on
/// every file creation and removal, and a lock there would be a hot-path cost for a
/// facility that is usually absent.
static NOTIFIER: AtomicUsize = AtomicUsize::new(0);

/// Reports whether any watch is registered anywhere, verdict supplied by the
/// object crate that owns the registry.
pub type WatchProbe = fn() -> bool;

/// Signature of the installed retirement notifier: `(inode, self_mask, is_dir)`.
///
/// Reported *before* the object disappears, so the watches that name it can be
/// retired with `IN_DELETE_SELF`/`IN_MOVE_SELF` and `IN_IGNORED`.
pub type RetireNotifier = fn(&VfsInode, u32, bool);

/// Signature of the installed self-event notifier: `(inode, self_mask, is_dir)`.
///
/// The counterpart of [`RetireNotifier`] for an object that *moves* rather than
/// disappears: its watches stay, and only hear `IN_MOVE_SELF`.
pub type SelfNotifier = fn(&VfsInode, u32, bool);

/// The installed retirement notifier, or 0 when none is installed.
static RETIRE: AtomicUsize = AtomicUsize::new(0);

/// The installed self-event notifier, or 0 when none is installed.
static SELF_NOTIFY: AtomicUsize = AtomicUsize::new(0);

/// The installed probe, or 0 when none is installed.
static WATCH_PROBE: AtomicUsize = AtomicUsize::new(0);

/// Counter behind the rename cookies, matching `fsnotify_get_cookie()`.
///
/// Linux increments a process-wide counter and hands the result to both halves
/// of a move, so a watcher can tell a rename it saw twice (once in each
/// directory) from two unrelated events. Starting from zero and incrementing
/// before use makes the first cookie 1, as there.
static NEXT_COOKIE: AtomicU32 = AtomicU32::new(0);

/// Installs the notifier.  Called once during kernel start-up.
pub fn set_entry_notifier(notifier: EntryNotifier) {
    NOTIFIER.store(notifier as usize, Ordering::Release);
}

/// Installs the retirement notifier alongside [`set_entry_notifier`].
pub fn set_retire_notifier(notifier: RetireNotifier) {
    RETIRE.store(notifier as usize, Ordering::Release);
}

/// Installs the watch probe alongside [`set_entry_notifier`].
pub fn set_watch_probe(probe: WatchProbe) {
    WATCH_PROBE.store(probe as usize, Ordering::Release);
}

/// Installs the self-event notifier alongside [`set_entry_notifier`].
pub fn set_self_notifier(notifier: SelfNotifier) {
    SELF_NOTIFY.store(notifier as usize, Ordering::Release);
}

/// Whether a directory-entry notification could go anywhere.
/// Namespace operations still capture their objects before this reporting gate.
pub(crate) fn entry_notify_wanted() -> bool {
    if NOTIFIER.load(Ordering::Acquire) == 0 {
        return false;
    }
    let raw = WATCH_PROBE.load(Ordering::Acquire);
    if raw == 0 {
        return false;
    }
    // SAFETY: only ``set_watch_probe`` writes `WATCH_PROBE`; it stores `notifier as usize`,
    // the address of a `fn` item coerced to a `fn` pointer and cast to an
    // integer, so every non-zero value observed here is exactly such a code
    // address (no real function pointer is null, which is why the zero check
    // above cleanly excludes the "not installed" sentinel). A `fn` pointer is a
    // plain `Copy` code address: the cast can lose no ownership, lifetime or
    // provenance, the `Release` store pairs with the `Acquire` load above so a
    // published pointer is seen with its initialisation. Several syscall
    // entries may reach the setter concurrently, and each stores the same
    // typed value, so the invariant rests on the typed setter parameter, the
    // zero sentinel and the `Release`/`Acquire` pairing rather than on a single
    // writer. Transmuting the address back to the same `fn`-pointer type
    // therefore reconstructs exactly the value that was stored.
    let probe: WatchProbe = unsafe { core::mem::transmute::<usize, WatchProbe>(raw) };
    probe()
}

/// Reports that `name` inside the captured parent inode changed.
pub(crate) fn notify_entry(inode: &VfsInode, mask: u32, name: &[u8], cookie: u32) {
    let raw = NOTIFIER.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    // SAFETY: only ``set_entry_notifier`` writes `NOTIFIER`; it stores `notifier as usize`,
    // the address of a `fn` item coerced to a `fn` pointer and cast to an
    // integer, so every non-zero value observed here is exactly such a code
    // address (no real function pointer is null, which is why the zero check
    // above cleanly excludes the "not installed" sentinel). A `fn` pointer is a
    // plain `Copy` code address: the cast can lose no ownership, lifetime or
    // provenance, the `Release` store pairs with the `Acquire` load above so a
    // published pointer is seen with its initialisation. Several syscall
    // entries may reach the setter concurrently, and each stores the same
    // typed value, so the invariant rests on the typed setter parameter, the
    // zero sentinel and the `Release`/`Acquire` pairing rather than on a single
    // writer. Transmuting the address back to the same `fn`-pointer type
    // therefore reconstructs exactly the value that was stored.
    let notifier: EntryNotifier = unsafe { core::mem::transmute::<usize, EntryNotifier>(raw) };
    notifier(inode, mask, name, cookie);
}

/// What the retirement decision needs to know about an object.
#[derive(Clone)]
pub(crate) struct EntryObject {
    /// Object pinned across the completed namespace operation.
    pub(crate) inode: Arc<VfsInode>,
    /// Hard-link count just before the operation ran: authoritative when the
    /// backend answered, the inode's cached count otherwise. It must always be
    /// decidable -- a removal that cannot classify its victim would leave the
    /// watch pinned to a dead object forever, since the watch itself keeps that
    /// object alive.
    pub(crate) nlink: u64,
    pub(crate) is_dir: bool,
}

impl EntryObject {
    /// Builds the decision input from the inode the operation captured itself,
    /// under the lock it held -- no second path lookup is involved.
    pub(crate) fn new(inode: Arc<VfsInode>, nlink: u64) -> Self {
        Self {
            is_dir: inode.is_dir(),
            inode,
            nlink,
        }
    }

    /// Whether this object stops existing when the named operation succeeds.
    ///
    /// `removed_dir` is certain (rmdir always removes the directory); a file is
    /// gone once its count reaches one, the state Linux observes at
    /// `fsnotify_delete()`.
    pub(crate) fn disappears(&self, removed_dir: bool) -> bool {
        removed_dir || self.is_dir || self.nlink <= 1
    }
}

/// Reports a creation event for an entry the creating operation reported.
///
/// The parent identity and the name come from the created entry's own record,
/// captured by the creating operation while it held the parent's namespace
/// lock. Reading them back from the dentry afterwards would race a concurrent
/// rename of the new entry, which rebinds the dentry's location and could
/// deliver `IN_CREATE` to a directory that never received the entry.
pub(crate) fn notify_created_entry(created: &kvfs::CreatedEntry, mask: u32) {
    if !entry_notify_wanted() {
        return;
    }
    notify_entry(&created.parent, mask, created.name.as_bytes(), 0);
}

/// Reports the creation of the entry a file names, attributed to the object
/// the entry actually lives in.
///
/// Attribution comes from the creation snapshot the open captured under the
/// parent's namespace lock (`VfsFile::created_under`), never from a later read
/// of the dentry: the created entry's location is mutable, and a concurrent
/// rename between `open` returning and this hook would otherwise report the
/// event in a directory that never received the entry. That snapshot is taken
/// unconditionally by the open path -- see the module documentation for why a
/// watch-existence gate there would be wrong -- and this hook itself returns
/// immediately when no watch exists.
pub(crate) fn notify_created(file: &VfsFile, mask: u32) {
    if !entry_notify_wanted() {
        return;
    }
    if let Some((parent, name)) = file.created_under() {
        notify_entry(parent, mask, name.as_bytes(), 0);
    }
}

/// Reports a completed content change to the parent and the object itself.
///
/// Call after successful truncation or a write/transfer that made progress,
/// once the request's position and backend locks have been released. The
/// notifier may allocate, acquire sleepable locks and wake polling tasks.
/// Notification cannot change the result of an already completed mutation.
pub(crate) fn notify_modified(path: &Path) {
    let raw = CONTENT.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    // SAFETY: set_content_notifier stores only ContentNotifier function pointers.
    // The acquire load observes a published pointer; zero is handled above.
    let notifier = unsafe { core::mem::transmute::<usize, ContentNotifier>(raw) };
    notifier(path.dentry(), IN_MODIFY);
}

/// Reports a self-event to the watches of one already-resolved object, leaving
/// them in place.
///
/// `is_dir` adds `IN_ISDIR`, which Linux's `fsnotify_inode()` sets for every
/// inode event whose inode is a directory.
pub(crate) fn notify_self_object(inode: &VfsInode, self_mask: u32, is_dir: bool) {
    let raw = SELF_NOTIFY.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    // SAFETY: only ``set_self_notifier`` writes `SELF_NOTIFY`; it stores `notifier as usize`,
    // the address of a `fn` item coerced to a `fn` pointer and cast to an
    // integer, so every non-zero value observed here is exactly such a code
    // address (no real function pointer is null, which is why the zero check
    // above cleanly excludes the "not installed" sentinel). A `fn` pointer is a
    // plain `Copy` code address: the cast can lose no ownership, lifetime or
    // provenance, the `Release` store pairs with the `Acquire` load above so a
    // published pointer is seen with its initialisation. Several syscall
    // entries may reach the setter concurrently, and each stores the same
    // typed value, so the invariant rests on the typed setter parameter, the
    // zero sentinel and the `Release`/`Acquire` pairing rather than on a single
    // writer. Transmuting the address back to the same `fn`-pointer type
    // therefore reconstructs exactly the value that was stored.
    let notifier: SelfNotifier = unsafe { core::mem::transmute::<usize, SelfNotifier>(raw) };
    notifier(inode, self_mask, is_dir);
}

/// Retires the watches of one already-resolved object.
///
/// The caller uses the inode captured by the namespace operation, without a
/// second path lookup that could resolve a different object.
pub(crate) fn retire_object(inode: &VfsInode, self_mask: u32, is_dir: bool) {
    let raw = RETIRE.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    // SAFETY: only ``set_retire_notifier`` writes `RETIRE`; it stores `notifier as usize`,
    // the address of a `fn` item coerced to a `fn` pointer and cast to an
    // integer, so every non-zero value observed here is exactly such a code
    // address (no real function pointer is null, which is why the zero check
    // above cleanly excludes the "not installed" sentinel). A `fn` pointer is a
    // plain `Copy` code address: the cast can lose no ownership, lifetime or
    // provenance, the `Release` store pairs with the `Acquire` load above so a
    // published pointer is seen with its initialisation. Several syscall
    // entries may reach the setter concurrently, and each stores the same
    // typed value, so the invariant rests on the typed setter parameter, the
    // zero sentinel and the `Release`/`Acquire` pairing rather than on a single
    // writer. Transmuting the address back to the same `fn`-pointer type
    // therefore reconstructs exactly the value that was stored.
    let notifier: RetireNotifier = unsafe { core::mem::transmute::<usize, RetireNotifier>(raw) };
    notifier(inode, self_mask, is_dir);
}

/// Reports a rename as the two halves Linux reports it in.
///
/// The watched directory that lost the entry sees `IN_MOVED_FROM` and the one
/// that gained it sees `IN_MOVED_TO`, both naming the same entry and both
/// carrying one cookie, so a watcher of both directories can pair them
/// (`fsnotify_move()` reports the two sides with `fsnotify_get_cookie()`'s
/// value). `moved_is_directory` adds `IN_ISDIR` to both, as Linux does for a
/// moved directory. Each half is optional: a path argument that names no object
/// simply has no half to report.
pub(crate) fn notify_rename_parts(
    from: Option<(Arc<VfsInode>, String)>,
    to: Option<(Arc<VfsInode>, String)>,
    moved_is_directory: bool,
) {
    let dir = if moved_is_directory { IN_ISDIR } else { 0 };
    let cookie = NEXT_COOKIE.fetch_add(1, Ordering::Relaxed) + 1;
    if let Some((inode, name)) = from {
        notify_entry(&inode, IN_MOVED_FROM | dir, name.as_bytes(), cookie);
    }
    if let Some((inode, name)) = to {
        notify_entry(&inode, IN_MOVED_TO | dir, name.as_bytes(), cookie);
    }
}

unittest::path_mod_test!(tests, "../ut-tests/notify.rs");
