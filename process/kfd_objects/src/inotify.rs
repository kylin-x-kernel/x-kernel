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
//! Events reach the queue only through [`Inotify::queue_event`]. This commit
//! queues two kernel-generated kinds itself — `IN_IGNORED` when a watch is
//! removed and `IN_Q_OVERFLOW` when the queue overruns — and nothing else yet:
//! the filesystem event source is wired in a separate change, so a watch on a
//! quiet filesystem simply never reports readiness instead of faking events.
//!
//! Two limits follow Linux defaults with one deliberate simplification: the
//! 16384-event queue ceiling and the 8192-watch ceiling are enforced per
//! *instance*, while Linux accounts both per *user* across instances. The
//! per-user accounting needs a credential-keyed global table and is left as a
//! follow-up; the per-instance limits still bound kernel memory so an
//! undrained instance cannot grow without end.

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicI32, Ordering};

use anon_inodefs::AnonInodeFs;
use hashbrown::HashMap;
use iov_iter::IovIterDest;
use kcred::Cred;
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::Mutex;
use kvfs::{FMode, FileOperations, Kiocb, OpenFlags, VfsFile, poll_file_io};
use linux_raw_sys::{
    general::{IN_DONT_FOLLOW, IN_IGNORED, IN_ISDIR, IN_ONLYDIR, IN_Q_OVERFLOW, IN_UNMOUNT},
    ioctl::FIONREAD,
};

/// Largest accepted watch path argument, **including** its terminator.
///
/// Matches Linux `PATH_MAX`. The bound belongs to the syscall adapter, which
/// copies at most `INOTIFY_PATH_MAX - 1` bytes of the *caller's* argument --
/// the same bytes Linux bounds -- and is exported here so both sides quote one
/// number. The object itself checks no path: a watch is registered by identity.
pub const INOTIFY_PATH_MAX: usize = 4096;

/// Watch descriptor numbers start at 1, as on Linux.
pub const FIRST_WATCH_DESCRIPTOR: i32 = 1;

/// Queued-event ceiling per instance, the Linux `max_queued_events` default.
///
/// Linux accounts the queue per user across instances; that accounting is not
/// modeled here (see the module docs). The limit exists so an instance nobody
/// is draining cannot exhaust kernel memory.
pub const MAX_QUEUED_EVENTS: usize = 16384;

/// Watch ceiling per instance, the Linux `max_user_watches` default.
///
/// Same per-instance simplification as [`MAX_QUEUED_EVENTS`]: Linux counts
/// watches per user, this counts per instance.
pub const MAX_WATCHES: usize = 8192;

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

/// The identity of a watched object.
///
/// A watch is a statement about an *object*, not about a spelling: Linux hangs
/// the mark on the inode (`inotify_update_existing_watch()` looks it up with
/// `fsnotify_find_mark()` on the inode) and finds it again by walking the
/// dentry's parent inode. This kernel stores the same pair — the device and
/// inode the path resolved to at registration — so that a re-registration
/// through another spelling (a relative argument, `..`, a symlink alias, or a
/// renamed directory) is the *same* watch rather than a second one, and so a
/// directory that is renamed keeps its watches.
///
/// `device` is first because the tuple form of this pair crosses the crate
/// boundary to the filesystem hook, which cannot name this type.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WatchIdentity {
    /// Device number of the mount holding the object.
    pub device: u64,
    /// Inode number of the object.
    pub inode: u64,
}

/// One registered watch.
///
/// `mask` holds the *event bits* the watch subscribes to and nothing else: it is
/// compared against the event being delivered, so a bit that never appears in an
/// event must not sit there. Linux splits the same way — `inotify_arg_to_mask()`
/// keeps only `arg & IN_ALL_EVENTS` in the mark's mask, and the behavioural bits
/// become separate mark flags — and a caller that passes `IN_ISDIR` alone
/// subscribes to no events at all.
#[derive(Clone, Debug, Eq, PartialEq)]
struct WatchEntry {
    /// Event bits currently registered for the object.
    mask: u32,
    /// Behavioural bits that outlive the call: `IN_EXCL_UNLINK`, `IN_ONESHOT`.
    flags: u32,
    /// Identity recorded when the watch was added.
    identity: WatchIdentity,
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
    descriptors: HashMap<WatchIdentity, i32>,
}

impl WatchTable {
    /// The descriptor already registered for `identity`, if any.
    fn descriptor_of(&self, identity: WatchIdentity) -> Option<i32> {
        self.descriptors.get(&identity).copied()
    }

    /// Registers a watch, returning `None` when the table is full.
    fn insert(
        &mut self,
        identity: WatchIdentity,
        mask: u32,
        flags: u32,
        next: &AtomicI32,
    ) -> Option<i32> {
        if self.entries.len() >= MAX_WATCHES {
            return None;
        }
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
            },
        );
        self.descriptors.insert(identity, descriptor);
        Some(descriptor)
    }

    /// Removes the watch under `descriptor`, if there is one.
    fn remove(&mut self, descriptor: i32) -> Option<WatchEntry> {
        let entry = self.entries.remove(&descriptor)?;
        self.descriptors.remove(&entry.identity);
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
    next_watch_descriptor: AtomicI32,
    watches: Mutex<WatchTable>,
    poll_rx: PollSet,
    queue: Mutex<EventQueue>,
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

/// Bytes one queued record occupies: the header plus its name field.
fn record_len(name_len: usize) -> usize {
    EVENT_HEADER_BYTES + name_field_len(name_len)
}

impl Inotify {
    /// Creates an empty watch table.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            next_watch_descriptor: AtomicI32::new(FIRST_WATCH_DESCRIPTOR),
            watches: Mutex::new(WatchTable::default()),
            poll_rx: PollSet::new(),
            queue: Mutex::new(EventQueue::default()),
        })
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
            Self::new(),
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
    /// `IN_ONESHOT` and `IN_EXCL_UNLINK` are accepted (Linux accepts them) but
    /// not honoured yet — a watch stays registered after it fires, and events
    /// for unlinked entries are not suppressed; both are documented gaps, not
    /// silent behaviour changes. `IN_ONLYDIR` is validated and
    /// `IN_DONT_FOLLOW` applied by the syscall adapter, the layer that resolves
    /// the path.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` for an empty path or an out-of-range mask,
    /// `KError::AlreadyExists` when `IN_MASK_CREATE` names a watched path, and
    /// `KError::StorageFull` when the instance already holds [`MAX_WATCHES`] watches.
    pub fn add_watch(self: &Arc<Self>, mask: u32, identity: WatchIdentity) -> KResult<i32> {
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

        let mut watches = self.watches.lock();
        // The watch is looked up by *identity*, so a second registration through
        // another spelling of the same object updates this watch instead of
        // adding one, which is what Linux returns for the same inode too.
        if let Some(descriptor) = watches.descriptor_of(identity) {
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
            }
            return Ok(descriptor);
        }
        // Only the event bits are stored for matching, and only the behavioural
        // bits that outlive the call are kept beside them; Linux makes the same
        // split (`inotify_arg_to_mask()` / `inotify_arg_to_flags()`).
        watches
            .insert(
                identity,
                stored_mask(mask),
                mask & INOTIFY_MARK_FLAGS,
                &self.next_watch_descriptor,
            )
            .ok_or(KError::StorageFull)
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
            Some(_) => {
                self.queue_event(descriptor, IN_IGNORED, &[]);
                Ok(())
            }
            None => Err(KError::InvalidInput),
        }
    }

    /// Appends an event and wakes anyone polling this instance.
    ///
    /// When the queue already holds [`MAX_QUEUED_EVENTS`] events the new one is
    /// dropped and the overrun record is queued in its place, which is what
    /// Linux does: `fsnotify_insert_event` finds `q_len >= max_events`, throws
    /// the event away and appends the group's pre-allocated `FS_Q_OVERFLOW`
    /// event instead (`wd = -1`, no name). That record takes a queue slot of its
    /// own, so an overrun is visible to a reader immediately — including one
    /// that never drains the queue, where a flag reported only on an empty queue
    /// would hide the loss forever.
    fn queue_event(&self, watch_descriptor: i32, mask: u32, name: &[u8]) {
        let mut queue = self.queue.lock();
        if queue.events.len() >= MAX_QUEUED_EVENTS {
            if !queue.overflow_queued {
                queue.events.push_back(QueuedEvent {
                    watch_descriptor: -1,
                    mask: IN_Q_OVERFLOW,
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

    fn drain_events(&self, buf: &mut [u8]) -> KResult<usize> {
        self.drain_events_batch(buf).map(|(written, _)| written)
    }

    /// Drains whole records into `buf`, serialized and ready for user space,
    /// and also returns the drained events so the iterator path can put them
    /// back when the user-space copy fails. Linux dequeues each event right
    /// before copying it, so a failing copy costs one event; this batches for
    /// one wait, and [`Inotify::requeue_front`] keeps the failing batch
    /// intact instead of losing it.
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
        for event in drained.iter().rev() {
            queue.events.push_front(event.clone());
        }
        if drained.iter().any(|event| event.overflow) {
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

    /// Reads back the mask registered for `descriptor`. Used by the unit tests.
    #[cfg(unittest)]
    pub(crate) fn watch_mask(&self, descriptor: i32) -> Option<u32> {
        self.watches
            .lock()
            .entries
            .get(&descriptor)
            .map(|watch| watch.mask)
    }
}

/// Writes one `struct inotify_event` record: wd, mask, cookie(0), len, name,
/// NUL, and zero padding out to the name field's reported length.
///
/// The name field is padded up to `sizeof(struct inotify_event)` — 16 bytes on
/// every architecture Linux supports — because user space steps through the
/// records by `header + len` and an unpadded short name would misalign the
/// array. A nameless record has no field at all.
fn write_record(out: &mut [u8], watch_descriptor: i32, mask: u32, name: &[u8]) {
    out[..4].copy_from_slice(&watch_descriptor.to_ne_bytes());
    out[4..8].copy_from_slice(&mask.to_ne_bytes());
    // cookie is always 0: this implementation reports no rename pairing.
    out[8..12].copy_from_slice(&0u32.to_ne_bytes());
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
        if buf.is_empty() {
            // Linux reports short-buffer failures before touching the queue.
            return Err(KError::InvalidInput);
        }
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
        // A destination that cannot hold even the record header is `EINVAL`,
        // the answer the plain `read` path gives for the same buffer: Linux's
        // `inotify_read()` rejects `count < sizeof(struct inotify_event)`
        // before touching the queue, and a zero-length read is such a count.
        if iter.count() < EVENT_HEADER_BYTES {
            return Err(KError::InvalidInput);
        }
        // One record is at most the 16-byte header plus a PATH_MAX-sized name
        // field, so this scratch always holds at least one whole record.
        let mut chunk = alloc::vec![0u8; 8192];
        let mut total = 0usize;
        loop {
            let want = chunk.len().min(iter.count());
            if want == 0 {
                break;
            }
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

    /// Reports the queued byte count for `FIONREAD`, as Linux does for inotify.
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
            IoEvents::IN
        } else {
            IoEvents::empty()
        }
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.poll_rx)?;
        }
        Ok(())
    }
}

unittest::path_mod_test!(inotify_tests, "../ut-tests/inotify.rs");
