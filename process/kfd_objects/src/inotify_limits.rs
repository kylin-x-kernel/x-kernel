// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Object-owned inotify reservations, independent of task and fd lifetimes.
//!
//! Linux bounds both instance count and watch count **per user**, not per
//! instance, and publishes the three numbers under `/proc/sys/fs/inotify/`.
//! This module owns that ledger: a `Uid`-keyed table of live instances and
//! watch memberships, plus the per-instance event ceiling that new instances
//! snapshot from the same source the sysctl node reads.
//!
//! The design follows the D6-002 submission package's inotify accounting
//! (`limits.rs`), adapted to this object's structures: reservations are
//! non-cloneable so exactly one decrement follows each successful increment,
//! and a rejected reservation leaves no empty UID entry behind.

use alloc::{collections::BTreeMap, sync::Arc};
use core::sync::atomic::{AtomicUsize, Ordering};

use kcred::{Cred, Uid};
use kerrno::{KError, KResult};
use klazy::Lazy;
use ksync::Mutex;

/// Live queue ceiling for a new instance; the Linux `max_queued_events` default.
pub const DEFAULT_MAX_QUEUED_EVENTS: usize = 16384;

/// Per-user watch ceiling default, the Linux `max_user_watches` default.
pub const DEFAULT_MAX_USER_WATCHES: usize = 8192;

/// Per-user instance ceiling default, the Linux `max_user_instances` default.
pub const DEFAULT_MAX_USER_INSTANCES: usize = 128;

/// Linux's nonnegative `int` sysctl domain, also used for its long limits.
const MAX_LIMIT: usize = i32::MAX as usize;

static RESOURCES: Lazy<Arc<Resources>> = Lazy::new(|| {
    Arc::new(Resources::new(
        DEFAULT_MAX_USER_WATCHES,
        DEFAULT_MAX_USER_INSTANCES,
        DEFAULT_MAX_QUEUED_EVENTS,
    ))
});

/// The live ledger, initialised on first use.
fn resources() -> &'static Arc<Resources> {
    Lazy::force(&RESOURCES)
}

/// One of the writable `/proc/sys/fs/inotify/` resource counts.
#[derive(Clone, Copy)]
pub enum InotifyLimit {
    /// Total watch memberships owned by instances created under one euid.
    UserWatches,
    /// Total live instances created under one euid, not fd aliases.
    UserInstances,
    /// Event records admitted to each instance, plus one overflow marker.
    QueuedEvents,
}

impl InotifyLimit {
    /// Reads the live limit; this does not reserve capacity.
    pub fn value(self) -> usize {
        resources().value(self)
    }

    /// Applies a sysctl write on behalf of the writing task's current cred.
    ///
    /// Linux re-runs `sysctl_perm` against the writer's current euid on every
    /// `proc_sys_call_handler` write; it does not trust the credential captured
    /// when the node was opened. For these root-owned `0644` nodes that check
    /// is `euid == 0`, so an unprivileged task that inherits or receives a
    /// root-opened descriptor is still refused. The procfs closure passes
    /// `kprocess::current_cred()`, not the file's opener credential.
    ///
    /// # Errors
    ///
    /// Returns `OperationNotPermitted` without mutation unless `cred.euid()`
    /// is 0, or `InvalidInput` for a write whose first token is not a base-0
    /// integer (Linux's `proc_get_long` rules) or lies outside
    /// `0..=i32::MAX`. Lowering a limit does not revoke existing objects or
    /// queued records: the event ceiling is snapshotted by new instances,
    /// matching Linux's creation-time behaviour.
    pub fn write(self, data: &[u8], cred: &Cred) -> KResult<()> {
        resources().write(self, data, cred)
    }
}

/// The event ceiling a new instance should snapshot.
pub(crate) fn queued_events_limit() -> usize {
    resources().max_events()
}

/// Reserves one watch membership for `uid`.
pub(crate) fn reserve_watch(uid: Uid) -> KResult<Reservation> {
    resources().reserve(uid, Resource::Watch)
}

/// Reserves one instance slot for `uid`.
pub(crate) fn reserve_instance(uid: Uid) -> KResult<Reservation> {
    resources().reserve(uid, Resource::Instance)
}

/// Parses a sysctl write the way Linux's `proc_get_long` does.
///
/// Works on raw bytes, not UTF-8: the first whitespace-terminated token is a
/// base-0 integer (`0x` hex, a leading `0` octal, otherwise decimal) with no
/// sign, and whatever follows the digit run belongs to the next vector
/// element, so a non-UTF-8 tail after whitespace is fine. The byte that ends
/// the number must be whitespace or the end of the buffer -- `"8192abc"` is
/// `EINVAL`, as in `proc_get_long` -- and a value beyond `usize` is rejected.
fn parse_proc_get_long(data: &[u8]) -> KResult<usize> {
    let is_space = |byte: u8| matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    // Leading blanks are skipped, as `proc_get_long` does before `strtoul`.
    let start = data
        .iter()
        .position(|byte| !is_space(*byte))
        .ok_or(KError::InvalidInput)?;
    let digits = &data[start..];
    let end = digits
        .iter()
        .position(|byte| is_space(*byte))
        .unwrap_or(digits.len());
    let token = &digits[..end];
    // `proc_get_long` copies at most `TMPBUFLEN - 1` (21) bytes; a number that
    // fills that buffer is rejected rather than truncated, so a 21-byte or
    // longer token is `EINVAL` here too.
    if token.len() > 20 {
        return Err(KError::InvalidInput);
    }

    // Base selection mirrors `_parse_integer_fixup_radix`. Anything after the
    // token is the next vector element's business, so a short write or a
    // non-UTF-8 tail is fine; garbage inside the token (`"8192abc"`) is not,
    // and the digit loop below rejects it the way `proc_get_long` does.
    let (radix, body) = if token.len() > 2 && token[0] == b'0' && (token[1] | 0x20) == b'x' {
        (16u32, &token[2..])
    } else if token.len() > 1 && token[0] == b'0' {
        (8u32, &token[1..])
    } else {
        (10u32, token)
    };

    let mut value = 0usize;
    for &byte in body {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return Err(KError::InvalidInput),
        };
        if u32::from(digit) >= radix {
            return Err(KError::InvalidInput);
        }
        value = value
            .checked_mul(radix as usize)
            .and_then(|value| value.checked_add(digit as usize))
            .ok_or(KError::InvalidInput)?;
    }
    Ok(value)
}

struct Resources {
    ledger: Mutex<Ledger>,
    /// Sampled at instance creation without the UID ledger lock. Relaxed
    /// ordering suffices: it publishes no other data or object state.
    queued_events: AtomicUsize,
}

struct Ledger {
    max_watches: usize,
    max_instances: usize,
    users: BTreeMap<Uid, Usage>,
}

#[derive(Default)]
struct Usage {
    watches: usize,
    instances: usize,
}

#[derive(Clone, Copy)]
enum Resource {
    Watch,
    Instance,
}

/// Non-cloneable reservation: exactly one decrement per successful increment.
pub(crate) struct Reservation {
    resources: Arc<Resources>,
    uid: Uid,
    resource: Resource,
}

impl Resources {
    fn new(max_watches: usize, max_instances: usize, max_events: usize) -> Self {
        Self {
            ledger: Mutex::new(Ledger {
                max_watches,
                max_instances,
                users: BTreeMap::new(),
            }),
            queued_events: AtomicUsize::new(max_events),
        }
    }

    fn value(&self, limit: InotifyLimit) -> usize {
        match limit {
            InotifyLimit::UserWatches => self.ledger.lock().max_watches,
            InotifyLimit::UserInstances => self.ledger.lock().max_instances,
            InotifyLimit::QueuedEvents => self.max_events(),
        }
    }

    fn max_events(&self) -> usize {
        self.queued_events.load(Ordering::Relaxed)
    }

    fn write(&self, limit: InotifyLimit, data: &[u8], cred: &Cred) -> KResult<()> {
        // Linux's `proc_sys_call_handler` rechecks the writer's current euid
        // through `sysctl_perm` on every write. Matching that check means a
        // root-opened descriptor passed to an unprivileged task cannot change
        // a system-wide limit.
        if !cred.is_privileged() {
            return Err(KError::OperationNotPermitted);
        }
        // Linux's `proc_get_long` works on raw bytes: the first
        // whitespace-terminated token is parsed with base-0 strtoul rules --
        // `0x` hex, a leading `0` octal, otherwise decimal, no sign -- and
        // everything after the digit run (more values, arbitrary bytes)
        // belongs to the following vector elements, so `write(fd, "8192 0", 6)`
        // applies 8192 and even a non-UTF-8 tail is tolerated. Only the byte
        // right after the digits must be whitespace or the end of the buffer,
        // so trailing garbage without a separator (`"8192abc"`) stays
        // `EINVAL`, as there. The kvfs write path reports the whole buffer
        // consumed, so Linux's short-write count cannot be echoed back; the
        // applied value, not the count, is what matters.
        let value = parse_proc_get_long(data)?;
        if value > MAX_LIMIT {
            return Err(KError::InvalidInput);
        }
        match limit {
            // Serialize limit changes with the check-and-increment, so a
            // completed write cannot be bypassed by a later reservation.
            InotifyLimit::UserWatches => self.ledger.lock().max_watches = value,
            InotifyLimit::UserInstances => self.ledger.lock().max_instances = value,
            InotifyLimit::QueuedEvents => self.queued_events.store(value, Ordering::Relaxed),
        }
        Ok(())
    }

    fn reserve(self: &Arc<Self>, uid: Uid, resource: Resource) -> KResult<Reservation> {
        let mut ledger = self.ledger.lock();
        let usage = ledger.users.get(&uid);
        let (count, limit, error) = match resource {
            Resource::Watch => (
                usage.map_or(0, |u| u.watches),
                ledger.max_watches,
                KError::StorageFull,
            ),
            Resource::Instance => (
                usage.map_or(0, |u| u.instances),
                ledger.max_instances,
                KError::TooManyOpenFiles,
            ),
        };
        if count >= limit {
            return Err(error);
        }
        // A rejected zero-limit request must not leave an empty UID entry.
        let usage = ledger.users.entry(uid).or_default();
        match resource {
            Resource::Watch => usage.watches += 1,
            Resource::Instance => usage.instances += 1,
        }
        Ok(Reservation {
            resources: self.clone(),
            uid,
            resource,
        })
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut ledger = self.resources.ledger.lock();
        if let Some(usage) = ledger.users.get_mut(&self.uid) {
            match self.resource {
                Resource::Watch => usage.watches -= 1,
                Resource::Instance => usage.instances -= 1,
            }
            if usage.watches == 0 && usage.instances == 0 {
                ledger.users.remove(&self.uid);
            }
        }
    }
}

unittest::path_mod_test!(tests, "../ut-tests/inotify_limits.rs");
