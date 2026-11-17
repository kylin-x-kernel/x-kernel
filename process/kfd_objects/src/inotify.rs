// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `inotify` instance object.
//!
//! The instance owns the watch table, the queued event list, and the anonymous
//! inode file that `inotify_init1` hands to user space. Reads follow the Linux
//! ABI: fixed 16-byte `struct inotify_event` headers, names NUL-terminated and
//! padded up to that header size (the same rounded length the `len` field
//! reports, so a reader stepping by `header + len` walks the array), `EINVAL`
//! for a buffer that cannot hold the next whole event, and `FIONREAD` reporting
//! the queued byte count.
//!
//! Events reach the queue through [`Inotify::queue_event`]. The object queues
//! `IN_IGNORED` on watch removal and `IN_Q_OVERFLOW` on overrun itself;
//! directory-entry events arrive from the VFS hooks through [`notify_path`],
//! which routes by the registered inode objects: create and delete from the
//! entry-creating and entry-removing syscalls, and a rename as the
//! `IN_MOVED_FROM`/`IN_MOVED_TO` pair carrying one cookie. Content changes arrive
//! through [`notify_content`], which applies the parent/object and unlink policies.
//! General metadata updates and close-class events have no source yet.
//!
//! Limits follow Linux's per-user accounting: instance count and watch count
//! are charged to the creator's euid across instances (see
//! [`crate::inotify_limits`], which also backs `/proc/sys/fs/inotify/`), and
//! each instance snapshots the queued-event ceiling at creation — lowering the
//! sysctl does not shrink a live queue, as on Linux. The defaults are the
//! Linux ones: 128 instances, 8192 watches, 16384 queued events.

use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

use anon_inodefs::AnonInodeFs;
use hashbrown::HashMap;
use iov_iter::IovIterDest;
use kcred::{Cred, Uid};
use kerrno::{KError, KResult};
use klazy::Lazy;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::Mutex;
use kvfs::{FMode, FileOperations, Kiocb, OpenFlags, VfsFile, VfsInode, poll_file_io};
use linux_raw_sys::{
    general::{IN_DONT_FOLLOW, IN_IGNORED, IN_ISDIR, IN_ONLYDIR, IN_Q_OVERFLOW, IN_UNMOUNT},
    ioctl::FIONREAD,
};

use crate::inotify_limits::{self, Reservation};

/// Largest accepted watch path argument, **including** its terminator.
///
/// Matches Linux `PATH_MAX`. The bound belongs to the syscall adapter, which
/// copies at most `INOTIFY_PATH_MAX - 1` bytes of the *caller's* argument --
/// the same bytes Linux bounds -- and is exported here so both sides quote one
/// number. The object itself checks no path: a watch is registered by identity.
pub const INOTIFY_PATH_MAX: usize = 4096;

/// Watch descriptor numbers start at 1, as on Linux.
pub(crate) const FIRST_WATCH_DESCRIPTOR: i32 = 1;

/// Default queued-event ceiling, the Linux `max_queued_events` default.
///
/// A new instance snapshots the live sysctl value, which starts here; the
/// constant stays for tests and for readers who want the default without the
/// live ledger.
#[cfg(unittest)]
pub(crate) const MAX_QUEUED_EVENTS: usize = inotify_limits::DEFAULT_MAX_QUEUED_EVENTS;

/// Default per-user watch ceiling, the Linux `max_user_watches` default.
#[cfg(unittest)]
pub(crate) const MAX_WATCHES: usize = inotify_limits::DEFAULT_MAX_USER_WATCHES;

// The mask bits come from `linux_raw_sys`, the same source the syscall adapter
// validates against: one definition of a bit means the two layers cannot drift
// into disagreeing about what a mask means.
pub(crate) use linux_raw_sys::general::{
    IN_ALL_EVENTS, IN_EXCL_UNLINK, IN_MASK_ADD, IN_MASK_CREATE, IN_ONESHOT,
};

/// The behavioural bits Linux keeps beside the mark rather than in its mask.
const INOTIFY_MARK_FLAGS: u32 = IN_EXCL_UNLINK | IN_ONESHOT;

/// The mask a watch stores for matching: the requested event bits, plus
/// `IN_UNMOUNT`, which `inotify_arg_to_mask()` ORs into every mark
/// unconditionally rather than taking from the argument. Linux also ORs
/// `FS_EVENT_ON_CHILD` into directory marks (the bit behind `IN_ISDIR`
/// delivery); that bit is supplied by the event source when directory-entry
/// events land, not by registration.
fn stored_mask(requested: u32) -> u32 {
    (requested & IN_ALL_EVENTS) | IN_UNMOUNT
}

/// The mask domain Linux accepts in `inotify_add_watch`.
///
/// Linux rejects only a mask bit outside this set with `EINVAL`, so refusing
/// documented bits here would fail calls that work there. The set is Linux's
/// `ALL_INOTIFY_BITS`: the subscribable event bits, the kernel-generated event
/// bits (`IN_UNMOUNT`, `IN_Q_OVERFLOW`, `IN_IGNORED`, `IN_ISDIR`) and the
/// behavioural flags (`IN_ONLYDIR`, `IN_DONT_FOLLOW`, `IN_EXCL_UNLINK`,
/// `IN_MASK_CREATE`, `IN_MASK_ADD`, `IN_ONESHOT`).
pub const IN_ACCEPTED: u32 = IN_ALL_EVENTS
    | IN_UNMOUNT
    | IN_Q_OVERFLOW
    | IN_IGNORED
    | IN_ISDIR
    | IN_ONLYDIR
    | IN_DONT_FOLLOW
    | IN_EXCL_UNLINK
    | IN_MASK_CREATE
    | IN_MASK_ADD
    | IN_ONESHOT;
/// The resolved identity of a watched object, as `fdinfo` reports it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WatchIdentity {
    /// Inode number of the watched path at registration time.
    pub inode: u64,
    /// Device number of the mount that held the path at registration time.
    pub device: u64,
}

/// One registered watch, as `fdinfo` reports it.
#[derive(Clone, Copy, Debug)]
pub struct FdinfoWatch {
    /// Watch descriptor user space holds.
    pub watch_descriptor: i32,
    /// Inode number recorded at registration.
    pub inode: u64,
    /// Device number recorded at registration.
    pub device: u64,
    /// The mask currently registered for the path.
    pub mask: u32,
}

/// One registered watch.
///
/// `mask` holds the *event bits* the watch subscribes to and nothing else: it is
/// compared against the event being delivered, so a bit that never appears in an
/// event must not sit there. Linux splits the same way — `inotify_arg_to_mask()`
/// keeps only `arg & IN_ALL_EVENTS` in the mark's mask, and the behavioural bits
/// become separate mark flags — and a caller that passes `IN_ISDIR` alone
/// subscribes to no events at all.
///
/// The reservation slot is held for the watch's lifetime, so removing the watch
/// returns its membership to the per-user ledger by drop; it is also why the
/// entry derives nothing -- a reservation must not be duplicated.
struct WatchEntry {
    /// Event bits currently registered for the object.
    mask: u32,
    /// Behavioural bits that outlive the call: `IN_EXCL_UNLINK`, `IN_ONESHOT`.
    flags: u32,
    /// Identity recorded when the watch was added.
    identity: WatchIdentity,
    /// Address of the pinned inode, used only as a routing key.
    object_key: usize,
    /// Per-user watch membership held while this entry exists.
    _slot: Reservation,
}

/// The watch table: descriptors to entries, plus the identity index that makes
/// a repeated registration O(1) instead of a scan.
///
/// Both maps live under one lock so an entry is never visible in one and not
/// the other.
#[derive(Default)]
struct WatchTable {
    entries: HashMap<i32, WatchEntry>,
    /// Identity of each registered watch, pointing at its descriptor.
    descriptors: HashMap<usize, i32>,
}

impl WatchTable {
    /// The descriptor already registered for `identity`, if any.
    fn descriptor_of(&self, identity: usize) -> Option<i32> {
        self.descriptors.get(&identity).copied()
    }

    /// Registers a watch.
    fn insert(
        &mut self,
        identity: WatchIdentity,
        object_key: usize,
        mask: u32,
        flags: u32,
        next: &AtomicI32,
        slot: Reservation,
    ) -> i32 {
        // Numbers are handed out in ascending order and wrap to the first one at
        // `i32::MAX`, so a long-lived instance never reaches the negative range
        // (`-1` there is the overrun record's sentinel). Linux recycles numbers
        // with `idr_alloc_cyclic()`; this only guarantees what the identity key
        // depends on -- a number in use is never handed out twice -- by skipping
        // occupied ones after each wrap.
        let mut descriptor = take_descriptor(next);
        while self.entries.contains_key(&descriptor) {
            descriptor = take_descriptor(next);
        }
        self.entries.insert(
            descriptor,
            WatchEntry {
                mask,
                flags,
                identity,
                object_key,
                _slot: slot,
            },
        );
        self.descriptors.insert(object_key, descriptor);
        descriptor
    }

    /// Removes the watch under `descriptor`, if there is one.
    fn remove(&mut self, descriptor: i32) -> Option<WatchEntry> {
        let entry = self.entries.remove(&descriptor)?;
        self.descriptors.remove(&entry.object_key);
        Some(entry)
    }

    /// Number of registered watches; the unit tests read it back.
    #[cfg(unittest)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// A kernel `inotify` instance.
pub struct Inotify {
    /// Identity used to tell this instance's registry entries from another
    /// instance's entries for the same path and descriptor number.
    id: usize,
    next_watch_descriptor: AtomicI32,
    watches: Mutex<WatchTable>,
    poll_rx: PollSet,
    queue: Mutex<EventQueue>,
    /// euid whose per-user watch budget this instance charges.
    owner: Uid,
    /// Queued-event ceiling snapshotted at creation from the live sysctl.
    max_queued_events: usize,
    /// Per-user instance membership, returned on final drop.
    _instance_slot: Reservation,
}

/// Queued events plus the overrun marker for one instance.
///
/// The marker is a real queue member — the 16-byte nameless `IN_Q_OVERFLOW`
/// record Linux appends when a push finds the queue at its ceiling — so a
/// reader sees the loss in order and promptly instead of waiting for a drain
/// that a continuously busy producer never allows. `overflow_queued` only
/// remembers that the record is already in the queue, which is what Linux's
/// `list_empty(&group->overflow_event->list)` check expresses: one overrun
/// record at a time, re-armed by the read that consumes it.
#[derive(Default)]
struct EventQueue {
    events: VecDeque<QueuedEvent>,
    /// Whether an overrun record is already queued.
    overflow_queued: bool,
}

/// One queued event, held in the layout `read` hands to user space.
#[derive(Clone)]
struct QueuedEvent {
    watch_descriptor: i32,
    mask: u32,
    /// Rename pairing value: the two halves of one move share it, and every
    /// other event reports 0, which is what Linux's `fsnotify_get_cookie()`
    /// values look like to user space.
    cookie: u32,
    name: Vec<u8>,
    /// Whether this is the overrun marker: it carries `wd = -1` and
    /// `IN_Q_OVERFLOW`, and consuming it re-arms overrun reporting. Tracked
    /// explicitly so the read path never has to infer it from the other
    /// fields.
    overflow: bool,
}

/// Takes the next watch descriptor, wrapping at `i32::MAX`.
///
/// Kept separate so the wrap is one place: a plain `fetch_add` would overflow
/// into negative numbers (and debug builds would panic) after `i32::MAX`
/// registrations, and `-1` is already the overrun record's `wd`. `Relaxed`
/// ordering is enough because the counter is only touched under the watch
/// table's lock, which orders hand-out against the insertion that publishes
/// the number.
fn take_descriptor(next: &AtomicI32) -> i32 {
    loop {
        let current = next.load(Ordering::Relaxed);
        let (descriptor, next_value) = if current == i32::MAX {
            (FIRST_WATCH_DESCRIPTOR, FIRST_WATCH_DESCRIPTOR + 1)
        } else {
            (current, current + 1)
        };
        if next
            .compare_exchange_weak(current, next_value, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return descriptor;
        }
    }
}

/// Bytes of the fixed `struct inotify_event` header: wd, mask, cookie, len.
const EVENT_HEADER_BYTES: usize = 16;

/// Length of an event's name field, as Linux reports it in `len`: the
/// NUL-inclusive name rounded up to `sizeof(struct inotify_event)` — the header
/// size. A record with no name (`IN_IGNORED`, `IN_Q_OVERFLOW`) has a zero-length
/// field and no padding at all.
fn name_field_len(name_len: usize) -> usize {
    if name_len == 0 {
        0
    } else {
        (name_len + 1).next_multiple_of(EVENT_HEADER_BYTES)
    }
}

/// A registered watch, as seen by the filesystem side.
///
/// The descriptor alone cannot identify an entry: watch descriptors are
/// numbered per instance, so two instances watching the same path both hold
/// `wd = 1`. The instance id makes removal target exactly one instance's
/// entry.
struct RegistryEntry {
    instance: Weak<Inotify>,
    instance_id: usize,
    watch_descriptor: i32,
    /// The watched object itself, kept alive for as long as any watch on it
    /// exists.
    ///
    /// This is Linux's model: an fsnotify mark holds the inode, so an inode
    /// cannot be freed -- and its number reused -- while a watch still names
    /// it. Without the pin, releasing the last in-memory reference would end
    /// an inode that is still linked on disk, and any retirement keyed on that
    /// release would report a death that never happened.
    object: Pin,
    /// Routing-mask cache, updated under watches -> registry lock order.
    mask: u32,
}

/// Strong reference to the watched VFS inode.
///
/// The registry retains it through routing removal, preventing address reuse
/// while the object remains a routing key.
pub type Pin = Arc<VfsInode>;

/// Hands out the per-instance ids used to disambiguate registry entries.
static NEXT_INSTANCE_ID: AtomicUsize = AtomicUsize::new(1);

/// Object-to-watches index shared by every inotify instance.
///
/// Keyed by the pinned VFS inode's address, so a filesystem operation
/// can find the watches interested in it without walking every instance, and so
/// a watched directory keeps its watches when it is renamed -- Linux reaches the
/// same marks by walking the changed entry's parent inode. A `Weak` reference is
/// held deliberately: an instance user space has closed must not be kept alive
/// by the registry, or its watch table would leak for the lifetime of the
/// kernel.
type WatchRegistry = HashMap<usize, Vec<RegistryEntry>>;

// Registry entries pin the inode until after removal, so this address cannot
// be reused while it is a key. Never dereference the integer key.
fn object_key(inode: &VfsInode) -> usize {
    core::ptr::from_ref(inode) as usize
}

/// Lazily initialised: `ksync::Mutex::new` is not a `const fn` once lock
/// statistics are compiled in, so a bare static initialiser would not build
/// under that configuration. The named type keeps the static's own type
/// readable (and below clippy's complexity threshold).
static WATCH_REGISTRY: Lazy<Mutex<Option<WatchRegistry>>> = Lazy::new(|| Mutex::new(None));

/// The shared registry.
fn registry() -> &'static Mutex<Option<WatchRegistry>> {
    Lazy::force(&WATCH_REGISTRY)
}

/// Whether any watch is registered anywhere.
///
/// The filesystem side reads this (through the installed probe) before doing
/// work that only matters when a watch exists.
pub fn any_watch_registered() -> bool {
    REGISTRY_NON_EMPTY.load(Ordering::Acquire)
}

/// Whether any watch is registered anywhere, so the VFS hook can skip the
/// registry lock on the common path where no watch exists at all.
///
/// Only set while holding the registry lock and only cleared while holding it
/// with an empty map, so a stale `true` costs one lock acquisition and a stale
/// `false` cannot occur and drop an event.
static REGISTRY_NON_EMPTY: AtomicBool = AtomicBool::new(false);

/// Delivers a filesystem event to every watch registered for an object.
///
/// The parent is the inode captured by the completed namespace operation.
/// The inode's aggregated mask rejects uninterested events before registry
/// lookup; delivery revalidates each watch under the instance lock.
pub fn notify_path(inode: &VfsInode, mask: u32, name: &[u8], cookie: u32) {
    route_event(inode, mask, name, cookie, None);
}

fn route_event(inode: &VfsInode, mask: u32, name: &[u8], cookie: u32, is_unlinked: Option<bool>) {
    if !inode.watches_notifications(mask) {
        return;
    }
    for_each_watch(inode, mask, |instance, descriptor| {
        instance.deliver(descriptor, mask, name, cookie, is_unlinked);
    });
}

/// Routes a completed file-content event using dentry and inode semantics.
///
/// The caller has released file-position and backend locks. Special files
/// suppress parent access/modify events, but retain their own subscriptions.
/// Linux 6.8: __fsnotify_parent() and fsnotify_handle_inode_event().
pub fn notify_content(dentry: &kvfs::Dentry, mask: u32) {
    if !any_watch_registered() {
        return;
    }
    let inode = dentry.inode_ref();
    let parent_mask = if mask
        & (linux_raw_sys::general::IN_ACCESS | linux_raw_sys::general::IN_MODIFY)
        != 0
        && matches!(
            inode.node_type(),
            kvfs::NodeType::CharacterDevice
                | kvfs::NodeType::BlockDevice
                | kvfs::NodeType::Fifo
                | kvfs::NodeType::Socket
        ) {
        0
    } else {
        mask
    };
    if parent_mask == 0 && !inode.watches_notifications(mask) {
        return;
    }
    let context = dentry.notification_context(parent_mask);
    if let Some((parent, name)) = context.parent {
        route_event(&parent, mask, name.as_bytes(), 0, Some(context.is_unlinked));
    }
    route_event(&inode, mask, &[], 0, Some(context.is_unlinked));
}

/// Reports a self-event (`IN_DELETE_SELF`, `IN_MOVE_SELF`) to every watch on
/// the given inode, leaving those watches in place.
///
/// A rename moves the object rather than destroying it, so its watches survive
/// and only hear about it.
pub fn notify_self_object(inode: &VfsInode, self_mask: u32, is_dir: bool) {
    // Linux's `fsnotify_inode()` sets `FS_ISDIR` for every inode event whose
    // inode is a directory, so a `IN_MOVE_SELF`/`IN_DELETE_SELF`/`IN_ATTRIB` on
    // a directory carries it too.
    let self_mask = if is_dir {
        self_mask | IN_ISDIR
    } else {
        self_mask
    };
    for_each_watch(inode, self_mask, |instance, descriptor| {
        instance.report_self(descriptor, self_mask);
    });
}

/// Retires every watch on the inode, which the caller has
/// established is going away: a removal, or a rename that replaces it.
///
/// `self_mask` is the self-event to report (`IN_DELETE_SELF` or
/// `IN_MOVE_SELF`); every retiree also gets `IN_IGNORED`, as on Linux.
pub fn retire_entry(inode: &VfsInode, self_mask: u32, is_dir: bool) {
    let self_mask = if is_dir {
        self_mask | IN_ISDIR
    } else {
        self_mask
    };
    for_each_watch(inode, u32::MAX, |instance, descriptor| {
        instance.retire(descriptor, self_mask);
    });
}

/// Hands every watch on the pinned inode to `f`, outside the registry lock.
fn for_each_watch(inode: &VfsInode, mask: u32, f: impl Fn(&Arc<Inotify>, i32)) {
    if !REGISTRY_NON_EMPTY.load(Ordering::Acquire) {
        return;
    }
    let identity = object_key(inode);
    // Every upgraded instance reference must leave the registry lock before
    // dropping: its final release can run Inotify::drop and re-enter registry.
    let mut interested = Vec::new();
    {
        let registry = registry().lock();
        let Some(registry) = registry.as_ref() else {
            return;
        };
        let Some(entries) = registry.get(&identity) else {
            return;
        };
        for entry in entries {
            if entry.mask & mask == 0 {
                continue;
            }
            let Some(instance) = entry.instance.upgrade() else {
                continue;
            };
            interested.push((instance, entry.watch_descriptor));
        }
    }
    for (instance, descriptor) in interested {
        f(&instance, descriptor);
    }
}

/// Number of registry entries for `identity`; the unit tests assert that
/// removal and instance teardown leave no key behind.
#[cfg(unittest)]
pub(crate) fn registry_entry_count(inode: &VfsInode) -> usize {
    let identity = object_key(inode);
    let registry = registry().lock();
    registry
        .as_ref()
        .and_then(|map| map.get(&identity))
        .map_or(0, |entries| entries.len())
}

/// Adds `identity` to the shared index for `instance`.
fn register_watch(
    identity: usize,
    object: Pin,
    mask: u32,
    instance: &Arc<Inotify>,
    watch_descriptor: i32,
) {
    let mut registry = registry().lock();
    let map = registry.get_or_insert_with(Default::default);
    let entries = map.entry(identity).or_default();
    // Dead references are pruned so closed instances do not accumulate, but
    // their removal is deferred until the lock is released: an entry owns a
    // pin on the watched object, and dropping the last one runs the inode's
    // eviction -- filesystem work that must never happen under this lock.
    let dead: Vec<RegistryEntry> = entries
        .extract_if(.., |entry| entry.instance.strong_count() == 0)
        .collect();
    entries.push(RegistryEntry {
        instance: Arc::downgrade(instance),
        instance_id: instance.id,
        watch_descriptor,
        object: object.clone(),
        mask,
    });
    object.set_notification_mask(entries.iter().fold(0, |mask, entry| mask | entry.mask));
    REGISTRY_NON_EMPTY.store(true, Ordering::Release);
    drop(registry);
    drop(dead);
}

// Called while the instance's watches lock is held. No delivery path takes
// that lock under the registry lock. Recompute only this object's subscriptions
// on watch changes, never on a write or other filesystem event.
fn update_watch_mask(inode: &VfsInode, instance_id: usize, descriptor: i32, mask: u32) {
    let mut registry = registry().lock();
    let Some(entries) = registry
        .as_mut()
        .and_then(|map| map.get_mut(&object_key(inode)))
    else {
        return;
    };
    if let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.instance_id == instance_id && entry.watch_descriptor == descriptor)
    {
        entry.mask = mask;
    }
    inode.set_notification_mask(entries.iter().fold(0, |mask, entry| mask | entry.mask));
}

/// Removes one watch from the shared index.
///
/// Matched on `(instance_id, watch_descriptor)`, not the descriptor alone:
/// descriptors are per instance, so filtering by number would delete a
/// different instance's watch on the same path and silence it permanently.
fn unregister_watch(identity: usize, watch_descriptor: i32, instance_id: usize) {
    let mut registry = registry().lock();
    let mut removed = Vec::new();
    if let Some(map) = registry.as_mut()
        && let Some(entries) = map.get_mut(&identity)
    {
        removed.extend(entries.extract_if(.., |entry| {
            entry.watch_descriptor == watch_descriptor && entry.instance_id == instance_id
        }));
        let mask = entries.iter().fold(0, |mask, entry| mask | entry.mask);
        if let Some(entry) = removed.first() {
            entry.object.set_notification_mask(mask);
        }
        if entries.is_empty() {
            map.remove(&identity);
        }
        if map.is_empty() {
            REGISTRY_NON_EMPTY.store(false, Ordering::Release);
        }
    }
    // Released outside the lock: see `register_watch`.
    drop(registry);
    drop(removed);
}

/// Record size of one queued event: the fixed header plus the NUL-terminated
/// name padded to the header boundary, which is what Linux's
/// `round_event_name_len()` computes. A zero-length name contributes nothing —
/// Linux's `IN_IGNORED`/`IN_Q_OVERFLOW` records are exactly the 16-byte header,
/// with no name field and no padding.
fn record_len(name_len: usize) -> usize {
    EVENT_HEADER_BYTES + name_field_len(name_len)
}

impl Drop for Inotify {
    fn drop(&mut self) {
        // A closed instance must not leave its keys behind: the registry is
        // keyed by watch identity and nothing else prunes a key whose instance
        // went away, so without this a process could grow kernel memory once
        // per distinct object it ever watched. The table is taken under the lock
        // and the registry is entered after releasing it, so `notify_path` --
        // which takes the registry and then the watches lock -- cannot deadlock
        // against this.
        let watches = core::mem::take(&mut *self.watches.lock());
        for (descriptor, watch) in watches.entries {
            unregister_watch(watch.object_key, descriptor, self.id);
        }
    }
}

impl Inotify {
    /// Creates an empty watch table charged to `owner`.
    ///
    /// The instance counts against the per-user `max_user_instances` budget
    /// and snapshots the live `max_queued_events` value.
    ///
    /// # Errors
    ///
    /// Returns `KError::TooManyOpenFiles` when the owner already holds the
    /// per-user instance limit.
    pub(crate) fn new(owner: Uid) -> KResult<Arc<Self>> {
        let instance_slot = inotify_limits::reserve_instance(owner)?;
        Ok(Arc::new(Self {
            id: NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
            next_watch_descriptor: AtomicI32::new(FIRST_WATCH_DESCRIPTOR),
            watches: Mutex::new(WatchTable::default()),
            poll_rx: PollSet::new(),
            queue: Mutex::new(EventQueue::default()),
            owner,
            max_queued_events: inotify_limits::queued_events_limit(),
            _instance_slot: instance_slot,
        }))
    }

    /// Creates the anonymous-inode file used by `inotify_init1`.
    ///
    /// `cred` is captured as the new file's immutable open credential. The
    /// anonymous inode consumes only the nonblock bit; close-on-exec is
    /// applied by the fd table.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` for unknown open-flag bits and forwards
    /// errors from anonymous-inode file creation.
    pub fn new_file(open_flags: u32, cred: Arc<Cred>) -> KResult<Arc<VfsFile>> {
        let open_flags = OpenFlags::from_bits(open_flags).ok_or(KError::InvalidInput)?;
        AnonInodeFs::global().get_file(
            "[inotify]",
            Arc::new(InotifyFops),
            Self::new(cred.euid())?,
            FMode::READ | FMode::STREAM,
            open_flags,
            cred,
        )
    }

    /// Returns the inotify instance attached to an inotify file.
    ///
    /// # Errors
    ///
    /// Returns `KError::BadFileDescriptor` when the file has no matching private
    /// data. The adapter maps that to `EINVAL`, matching Linux for a descriptor
    /// that is not an inotify instance.
    pub fn from_file(file: &VfsFile) -> KResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(KError::BadFileDescriptor)
    }

    /// Registers `path` with `mask`, returning its watch descriptor.
    ///
    /// The receiver is `&Arc<Self>` on purpose: the registry introduced with the
    /// event-source change plants a `Weak` reference per watch, and
    /// `Arc::downgrade` needs the owning handle. Nothing else in the call needs
    /// the strong count.
    ///
    /// Re-watching a path without [`IN_MASK_ADD`] replaces the previous mask and
    /// returns the existing descriptor, while [`IN_MASK_CREATE`] turns that same
    /// condition into `EEXIST`. A mask outside [`IN_ACCEPTED`] is rejected with
    /// `EINVAL`, and an empty path cannot name a watch.
    ///
    /// An empty mask is `EINVAL`, as is one naming both `IN_MASK_ADD` and
    /// `IN_MASK_CREATE`; a mask of behavioural flags alone is accepted, as on
    /// Linux. `IN_MASK_ADD` and
    /// `IN_MASK_CREATE` are commands and are not stored.
    /// `IN_EXCL_UNLINK` suppresses content events from removed dentries for
    /// this watch. `IN_ONESHOT` is accepted but not honoured yet: a watch stays
    /// registered after it fires. `IN_ONLYDIR` is validated and
    /// `IN_DONT_FOLLOW` applied by the syscall adapter, the layer that resolves
    /// the path.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` for an empty path or an out-of-range mask,
    /// `KError::AlreadyExists` when `IN_MASK_CREATE` names a watched path, and
    /// `KError::StorageFull` when the owner's per-user watch budget is spent
    /// (the ledger's live `/proc/sys/fs/inotify/max_user_watches`).
    pub fn add_watch(
        self: &Arc<Self>,
        mask: u32,
        identity: WatchIdentity,
        object: Pin,
    ) -> KResult<i32> {
        if mask & !IN_ACCEPTED != 0 {
            return Err(KError::InvalidInput);
        }
        // Linux requires at least one valid bit: its second check is
        // `!(mask & ALL_INOTIFY_BITS)` on a mask already proven to lie inside
        // that set, which is exactly `mask != 0`. A mask naming only
        // behavioural flags (for example `IN_ONLYDIR` alone) is accepted there
        // and is accepted here; only an empty mask is refused.
        if mask == 0 {
            return Err(KError::InvalidInput);
        }
        // Linux: "IN_MASK_ADD and IN_MASK_CREATE don't make sense together".
        // The adapter decides this at the position Linux does (right after the
        // descriptor is known to be an inotify instance), so that the error a
        // caller sees for a bad descriptor is still `EBADF`; the object repeats
        // the rule for callers that reach it by another route.
        if mask & IN_MASK_ADD != 0 && mask & IN_MASK_CREATE != 0 {
            return Err(KError::InvalidInput);
        }

        // The watch is looked up by *identity*, so a second registration through
        // another spelling of the same object updates this watch instead of
        // adding one, which is what Linux returns for the same inode too.
        let mut watches = self.watches.lock();
        let key = object_key(&object);
        if let Some(descriptor) = watches.descriptor_of(key) {
            if mask & IN_MASK_CREATE != 0 {
                return Err(KError::AlreadyExists);
            }
            // The stored mask keeps the event bits and `IN_UNMOUNT` (every Linux
            // mark subscribes to `FS_UNMOUNT` unconditionally) and nothing else;
            // `IN_MASK_ADD` merges into the previous subscription, otherwise the
            // request replaces it.
            let requested = stored_mask(mask);
            let requested_flags = mask & INOTIFY_MARK_FLAGS;
            if let Some(watch) = watches.entries.get_mut(&descriptor) {
                // `IN_MASK_ADD` merges into the previous subscription, flags
                // included (`fsn_mark->mask |= ...; fsn_mark->flags |= ...`);
                // otherwise the request replaces it.
                if mask & IN_MASK_ADD != 0 {
                    watch.mask |= requested;
                    watch.flags |= requested_flags;
                } else {
                    watch.mask = requested;
                    watch.flags = requested_flags;
                }
                update_watch_mask(&object, self.id, descriptor, watch.mask);
            }
            return Ok(descriptor);
        }
        // Reserve before inserting, so a rejected reservation leaves no watch
        // behind; the slot drops with the entry.
        let slot = inotify_limits::reserve_watch(self.owner)?;
        let descriptor = watches.insert(
            identity,
            key,
            stored_mask(mask),
            mask & INOTIFY_MARK_FLAGS,
            &self.next_watch_descriptor,
            slot,
        );
        // Publish to the shared registry inside the watches critical section,
        // so a concurrent filesystem event cannot observe the gap where the
        // watch table already has the entry but the registry cannot route to
        // it yet. Nesting the registry lock under the watches lock cannot
        // deadlock: the registry is always the *last* lock taken (no path
        // holds it while acquiring a watches lock -- `notify_path` collects
        // its references and releases it before delivering; `Inotify::drop`
        // releases the watches lock before unregistering), and the dead
        // references dropped while here are `Weak`s of already-gone
        // instances, which run no `Inotify::drop`.
        register_watch(key, object, stored_mask(mask), self, descriptor);
        drop(watches);
        Ok(descriptor)
    }

    /// Reports a self-event (`IN_DELETE_SELF`, `IN_MOVE_SELF`) to one watch,
    /// leaving the watch in place.
    ///
    /// A rename moves the object rather than destroying it, so its watches must
    /// survive and merely hear about it; only an object that is *replaced* or
    /// removed is retired.
    fn report_self(&self, descriptor: i32, self_mask: u32) -> bool {
        // Same invariant as `deliver`: the subscription check and the enqueue
        // share one critical section, so a concurrent removal cannot make this
        // wd see `IN_IGNORED` first and a self-event after it.
        let watches = self.watches.lock();
        let subscribed = watches
            .entries
            .get(&descriptor)
            .is_some_and(|watch| watch.mask & self_mask != 0);
        if subscribed {
            self.queue_event(descriptor, self_mask, &[], 0);
        }
        drop(watches);
        subscribed
    }

    /// Retires one watch whose object is going away, reporting `self_mask`
    /// (`IN_DELETE_SELF` or `IN_MOVE_SELF`) when the watch subscribes to it.
    ///
    /// Linux reports `IN_DELETE_SELF`/`IN_MOVE_SELF` and then `IN_IGNORED` when
    /// the watched inode goes away, and the mark does not survive it. Doing the
    /// same here releases its object pin and subscription accounting after
    /// reporting the final event. Routing keys cannot adopt a different inode:
    /// the registry keeps the original object alive until it removes the key.
    ///
    /// # Errors
    ///
    /// Returns `false` when the descriptor is not registered.
    fn retire(&self, descriptor: i32, self_mask: u32) -> bool {
        let mut watches = self.watches.lock();
        let Some(entry) = watches.entries.get(&descriptor) else {
            return false;
        };
        let identity = entry.object_key;
        let subscribed = entry.mask & self_mask != 0;
        watches.remove(descriptor);
        drop(watches);

        // Registry after watches, and only ever after the watches lock is
        // released; `Drop` unregisters in the same order.
        unregister_watch(identity, descriptor, self.id);
        if subscribed {
            self.queue_event(descriptor, self_mask, &[], 0);
        }
        self.queue_event(descriptor, IN_IGNORED, &[], 0);
        true
    }

    /// Removes the watch registered under `descriptor` and queues `IN_IGNORED`.
    ///
    /// Linux reports the destruction to the watcher with `IN_IGNORED` after the
    /// events already queued for that watch, which is what `inotifyd` waits on
    /// before exiting.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` when no such watch exists.
    pub fn remove_watch(&self, descriptor: i32) -> KResult<()> {
        let removed = self.watches.lock().remove(descriptor);
        match removed {
            Some(watch) => {
                unregister_watch(watch.object_key, descriptor, self.id);
                self.queue_event(descriptor, IN_IGNORED, &[], 0);
                Ok(())
            }
            None => Err(KError::InvalidInput),
        }
    }

    /// Delivers one event to `descriptor` if it is still subscribed to `mask`.
    ///
    /// The subscription is re-checked here, under the same lock that guards the
    /// watch table, and the enqueue happens before that lock is released. A
    /// check that released the lock first would let `remove_watch` run to
    /// completion in between — descriptor gone, `IN_IGNORED` already queued —
    /// and then let this event land behind it, or, once descriptors recycle,
    /// land on a *different* watch that reused the number. Linux does not have
    /// the second problem at all (an inode mark remembers its own `wd` and
    /// reports `-1` once detached) and closes the first by holding its
    /// `idr_lock` across the queue insert; this does the equivalent.
    ///
    /// Lock order is watches before queue, and nothing takes them in the other
    /// order, so the nesting cannot deadlock.
    fn deliver(
        &self,
        descriptor: i32,
        mask: u32,
        name: &[u8],
        cookie: u32,
        is_unlinked: Option<bool>,
    ) {
        let watches = self.watches.lock();
        let subscribed = watches.entries.get(&descriptor).is_some_and(|watch| {
            watch.mask & mask != 0
                && !(is_unlinked == Some(true) && watch.flags & IN_EXCL_UNLINK != 0)
        });
        if subscribed {
            self.queue_event(descriptor, mask, name, cookie);
        }
        drop(watches);
    }

    /// Appends an event and wakes anyone polling this instance.
    ///
    /// When the queue already holds the instance's snapshotted event ceiling
    /// ([`MAX_QUEUED_EVENTS`] unless `/proc/sys/fs/inotify/max_queued_events`
    /// moved it) the new one is
    /// dropped and the overrun record is queued in its place, which is what
    /// Linux does: `fsnotify_insert_event` finds `q_len >= max_events`, throws
    /// the event away and appends the group's pre-allocated `FS_Q_OVERFLOW`
    /// event instead (`wd = -1`, no name). That record takes a queue slot of its
    /// own, so an overrun is visible to a reader immediately — including one
    /// that never drains the queue, where a flag reported only on an empty queue
    /// would hide the loss forever.
    fn queue_event(&self, watch_descriptor: i32, mask: u32, name: &[u8], cookie: u32) {
        let mut queue = self.queue.lock();
        if queue.events.len() >= self.max_queued_events {
            if !queue.overflow_queued {
                queue.events.push_back(QueuedEvent {
                    watch_descriptor: -1,
                    mask: IN_Q_OVERFLOW,
                    cookie: 0,
                    name: Vec::new(),
                    overflow: true,
                });
                queue.overflow_queued = true;
            }
            drop(queue);
            self.poll_rx.wake();
            return;
        }
        queue.events.push_back(QueuedEvent {
            watch_descriptor,
            mask,
            cookie,
            name: name.to_vec(),
            overflow: false,
        });
        drop(queue);
        self.poll_rx.wake();
    }

    /// Whether a `read` would return at least one record.
    fn has_readable_record(&self) -> bool {
        !self.queue.lock().events.is_empty()
    }

    /// Bytes a `read` of the whole queue would return: every event's header plus
    /// its NUL-terminated name padded to the header boundary. An overrun record
    /// is an ordinary member of the queue here, so it needs no separate term.
    ///
    /// This is what `FIONREAD` reports for an inotify descriptor. Userspace
    /// needs it to size its buffer, and busybox `inotifyd` refuses to run
    /// without it.
    fn queued_bytes(&self) -> usize {
        self.queue
            .lock()
            .events
            .iter()
            .map(|event| record_len(event.name.len()))
            .sum()
    }

    /// Drains whole records into `buf`, serialized and ready for user space,
    /// and also returns the drained events so the iterator path can put them
    /// back when the user-space copy fails. Linux dequeues each event right
    /// before copying it, so a failing copy costs one event; this batches for
    /// one wait, and [`Inotify::requeue_front`] keeps the failing batch
    /// intact instead of losing it.
    fn drain_events(&self, buf: &mut [u8]) -> KResult<usize> {
        self.drain_events_batch(buf).map(|(written, _)| written)
    }

    fn drain_events_batch(&self, buf: &mut [u8]) -> KResult<(usize, Vec<QueuedEvent>)> {
        let mut queue = self.queue.lock();
        let mut written = 0usize;
        let mut drained = Vec::new();
        while let Some(event) = queue.events.pop_front() {
            let total = record_len(event.name.len());
            if written + total > buf.len() {
                // Does not fit: the record goes back to the front, untouched.
                queue.events.push_front(event);
                break;
            }
            write_record(
                &mut buf[written..written + total],
                event.watch_descriptor,
                event.mask,
                event.cookie,
                event.name.as_slice(),
            );
            // Consuming the overrun record re-arms overflow reporting, the way
            // dequeuing Linux's overflow event makes it queueable again.
            if event.overflow {
                queue.overflow_queued = false;
            }
            written += total;
            drained.push(event);
        }
        if written == 0 {
            // Nothing was reported: either the queue is genuinely empty — a
            // blocking reader goes back to waiting — or the next record cannot
            // fit in `buf`, which Linux reports as `EINVAL` rather than a
            // partial record.
            return if queue.events.is_empty() {
                Err(KError::WouldBlock)
            } else {
                Err(KError::InvalidInput)
            };
        }
        Ok((written, drained))
    }

    /// Puts records back at the front of the queue in their original order
    /// after a user-space copy failed mid-batch: the records were serialized
    /// into kernel memory and never reached the caller, so destroying them
    /// would lose events Linux would have delivered (it only ever loses the
    /// one event whose copy failed). An overrun record that comes back also
    /// re-arms the overrun marker, since the queue holds it again.
    fn requeue_front(&self, drained: Vec<QueuedEvent>) {
        if drained.is_empty() {
            return;
        }
        let mut queue = self.queue.lock();
        // Taking the overrun marker off the queue re-armed overrun reporting,
        // so a producer may have queued a fresh marker while the batch was
        // being copied out. Putting the stale one back on top of it would
        // leave two, which the queue's own invariant forbids; the marker that
        // is already queued describes the same loss, so the stale one is
        // simply dropped. The reconciliation happens under the queue lock, so
        // it cannot race a producer either.
        let marked = queue.overflow_queued;
        for event in drained.iter().rev() {
            if event.overflow && marked {
                continue;
            }
            queue.events.push_front(event.clone());
        }
        if !marked && drained.iter().any(|event| event.overflow) {
            queue.overflow_queued = true;
        }
        // A reader that went to sleep while the queue was empty is still
        // registered on the receive side: the requeued records are readable
        // now, so it must be woken, exactly as a fresh event would wake it.
        drop(queue);
        self.poll_rx.wake();
    }

    /// Number of registered watches. Used by the unit tests.
    #[cfg(unittest)]
    pub(crate) fn watch_count(&self) -> usize {
        self.watches.lock().len()
    }

    /// Snapshot of the registered watches for `/proc/<pid>/fdinfo/<fd>`,
    /// ordered by watch descriptor so the rendering is stable.
    ///
    /// `mask` is rendered the way Linux renders it: `inotify_mark_user_mask()`
    /// intersects the mark's mask with `IN_ALL_EVENTS`, then ORs back the two
    /// behavioural flags it tracks separately (`IN_EXCL_UNLINK`,
    /// `IN_ONESHOT`). This object stores the same two things the same way --
    /// event bits in `mask`, behavioural bits in `flags` -- so the rendering is
    /// one OR; `IN_UNMOUNT` is subscribed for matching but not echoed, and the
    /// add directives (`IN_MASK_ADD`, `IN_MASK_CREATE`) and resolution flags
    /// (`IN_ONLYDIR`, `IN_DONT_FOLLOW`) are not part of the watch.
    pub fn fdinfo_watches(&self) -> Vec<FdinfoWatch> {
        let watches = self.watches.lock();
        let mut snapshot: Vec<FdinfoWatch> = watches
            .entries
            .iter()
            .map(|(&descriptor, watch)| FdinfoWatch {
                watch_descriptor: descriptor,
                inode: watch.identity.inode,
                device: watch.identity.device,
                // `inotify_mark_user_mask()`: the event bits, plus the two
                // behavioural flags it tracks separately. `IN_UNMOUNT` is
                // subscribed for matching but is not an event bit, so it is
                // not echoed here.
                mask: (watch.mask & IN_ALL_EVENTS) | watch.flags,
            })
            .collect();
        snapshot.sort_unstable_by_key(|watch| watch.watch_descriptor);
        snapshot
    }

    /// Path text registered for `descriptor`. Used by the unit tests.
    #[cfg(unittest)]
    pub(crate) fn watch_mask(&self, descriptor: i32) -> Option<u32> {
        self.watches
            .lock()
            .entries
            .get(&descriptor)
            .map(|watch| watch.mask)
    }

    /// Behavioural bits stored for `descriptor`. Used by the unit tests.
    #[cfg(unittest)]
    pub(crate) fn watch_flags(&self, descriptor: i32) -> Option<u32> {
        self.watches
            .lock()
            .entries
            .get(&descriptor)
            .map(|watch| watch.flags)
    }

    /// Identity recorded for `descriptor`. Used by the unit tests.
    #[cfg(unittest)]
    pub(crate) fn watch_identity(&self, descriptor: i32) -> Option<WatchIdentity> {
        self.watches
            .lock()
            .entries
            .get(&descriptor)
            .map(|watch| watch.identity)
    }
}

/// Writes one `struct inotify_event` record: wd, mask, cookie, len, name,
/// NUL, and zero padding out to the name field's reported length.
///
/// The name field is padded up to `sizeof(struct inotify_event)` — 16 bytes on
/// every architecture Linux supports — because user space steps through the
/// records by `header + len` and an unpadded short name would misalign the
/// array. A nameless record has no field at all.
fn write_record(out: &mut [u8], watch_descriptor: i32, mask: u32, cookie: u32, name: &[u8]) {
    out[..4].copy_from_slice(&watch_descriptor.to_ne_bytes());
    out[4..8].copy_from_slice(&mask.to_ne_bytes());
    // The rename pairing value: both halves of a move carry the same one, every
    // other event carries 0.
    out[8..12].copy_from_slice(&cookie.to_ne_bytes());
    // Linux stores the padded field length in `len` (`inotify_event.len =
    // round_event_name_len(...)`), so a reader stepping by `header + len`
    // reaches the next record, exactly as man 7 inotify describes.
    out[12..16].copy_from_slice(&(name_field_len(name.len()) as u32).to_ne_bytes());
    out[EVENT_HEADER_BYTES..EVENT_HEADER_BYTES + name.len()].copy_from_slice(name);
    for byte in &mut out[EVENT_HEADER_BYTES + name.len()..] {
        *byte = 0;
    }
}

struct InotifyFops;

impl InotifyFops {
    fn state(file: &VfsFile) -> KResult<Arc<Inotify>> {
        Inotify::from_file(file)
    }

    /// Waits for a readable record and drains it.
    ///
    /// Readiness means "at least one record is reportable", so wait for that
    /// first and then drain. Waiting on the poll event alone would return
    /// `EAGAIN` to a non-blocking reader that has records waiting, and would
    /// lose a wakeup that arrived between the readiness check and the drain.
    async fn read_wait(file: &VfsFile, buf: &mut [u8]) -> KResult<usize> {
        let state = Self::state(file)?;
        // No empty-buffer shortcut: Linux's `inotify_read` always peeks the
        // queue first (`get_one_event`), so an empty destination reports
        // `EINVAL` when a record is queued and waits (or `EAGAIN`) when it is
        // not.
        poll_file_io(state.as_ref(), IoEvents::IN, file.is_nonblocking(), || {
            state.drain_events(buf)
        })
        .await
    }
}

impl FileOperations for InotifyFops {
    fn supports_read(&self) -> bool {
        true
    }

    /// Reads queued records straight into the iterator destination.
    ///
    /// Overridden because the default implementation re-issues `read` in
    /// page-sized chunks, and a second chunk would wait for the *next* event
    /// after the first had already copied data: a blocking reader with a buffer
    /// larger than the queued bytes would hang until an unrelated event arrived
    /// instead of returning what it got. This waits once for the first record
    /// (Linux: `add_wait_queue` before the copy loop) and then drains whatever
    /// still fits, returning the copied byte count as soon as the queue is dry.
    fn read_iter(
        &self,
        iocb: &mut Kiocb<'_>,
        iter: &mut IovIterDest<'_>,
    ) -> kvfs::VfsResult<usize> {
        let state = Self::state(iocb.file())?;
        let nonblocking = iocb.file().is_nonblocking();
        // No size precheck here, on purpose: Linux's `get_one_event()` peeks
        // the queue first and only answers `EINVAL` when a record that exists
        // does not fit (`event_size > count`). An empty queue with a short
        // destination therefore blocks (or returns `EAGAIN`), which is what
        // `drain_events_batch` decides for us. A zero-length destination is
        // passed through as an empty scratch for the same reason: a queued
        // record makes it `EINVAL`, while an empty queue waits or `EAGAIN`s.
        // One record is at most the 16-byte header plus a PATH_MAX-sized name
        // field, so this scratch always holds at least one whole record.
        let mut chunk = alloc::vec![0u8; 8192];
        let mut total = 0usize;
        loop {
            let want = chunk.len().min(iter.count());
            let (read, drained) = match state.drain_events_batch(&mut chunk[..want]) {
                Ok(drain) => drain,
                // An empty queue waits only before the first record; afterwards
                // it just ends the read. A record that no longer fits is not an
                // error either once bytes are in -- dropping copied records
                // because a later one would not fit is exactly the loss this
                // override exists to prevent.
                Err(KError::WouldBlock | KError::InvalidInput) if total != 0 => break,
                Err(KError::WouldBlock) => ktask::future::block_on(poll_file_io(
                    state.as_ref(),
                    IoEvents::IN,
                    nonblocking,
                    || state.drain_events_batch(&mut chunk[..want]),
                ))?,
                Err(err) => return Err(err),
            };
            match iter.copy_to_iter(&chunk[..read]) {
                Ok(copied) => {
                    total += copied;
                    iocb.advance(copied);
                    if copied < read || iter.count() == 0 {
                        break;
                    }
                }
                // The user buffer turned out to be unwritable. The batch here
                // never reached user space, so it goes back to the front of
                // the queue whole -- Linux loses the one event whose copy
                // failed. The error itself propagates even when earlier
                // batches already copied: Linux's inotify_read deliberately
                // keeps -EFAULT in that spot (`ret != -EFAULT` guards the
                // byte-count replacement), and so do we.
                Err(err) => {
                    state.requeue_front(drained);
                    return Err(err);
                }
            }
        }
        Ok(total)
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], _offset: u64) -> KResult<usize> {
        ktask::future::block_on(Self::read_wait(file, buf))
    }

    /// Reports the instance's readiness; `None` when the file is not one, which
    /// is the file-operation trait's way of saying "ask the next source".
    fn poll(&self, file: &VfsFile) -> Option<IoEvents> {
        Self::state(file).ok().map(|state| state.poll())
    }

    /// Reports the queued byte count for `FIONREAD`, as Linux does for inotify:
    /// `inotify_ioctl()` sums `sizeof(struct inotify_event) +
    /// round_event_name_len()` over the whole queue (`inotify_user.c:328`–`:336`),
    /// which is the same total this returns -- the size of a full drain, not the
    /// length of the next record.
    ///
    /// Without this the descriptor rejects the query and a caller that sizes
    /// its buffer first — busybox `inotifyd` does — gives up before reading
    /// anything. The fallback stays `NotATty` so a descriptor that is *not* an
    /// inotify instance keeps its existing behaviour.
    fn ioctl(&self, file: &VfsFile, _cred: &kcred::Cred, cmd: u32, arg: usize) -> KResult<usize> {
        if cmd != FIONREAD {
            return Err(KError::NotATty);
        }
        let state = Self::state(file)?;
        // `write_vm` validates the user pointer before writing, so a bad
        // address surfaces as an error rather than a fault.
        posix_types::UserPtr::<i32>::from(arg as *mut i32).write_vm(state.queued_bytes() as i32)?;
        Ok(0)
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if let Ok(state) = Self::state(file) {
            state.register(context, events)?;
        }
        Ok(())
    }
}

impl Pollable for Inotify {
    fn poll(&self) -> IoEvents {
        if self.has_readable_record() {
            // Linux reports `EPOLLIN | EPOLLRDNORM` for a readable inotify
            // instance, so an epoll waiter that registered only `POLLRDNORM`
            // also matches and wakes.
            IoEvents::IN | IoEvents::RDNORM
        } else {
            IoEvents::empty()
        }
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        // `RDNORM` is Linux's alias of `IN` for readability, so a waiter that
        // asked for it alone subscribes to the same receive side.
        if events.intersects(IoEvents::IN | IoEvents::RDNORM) {
            context.register(&self.poll_rx)?;
        }
        Ok(())
    }
}

unittest::path_mod_test!(inotify_tests, "../ut-tests/inotify.rs");
