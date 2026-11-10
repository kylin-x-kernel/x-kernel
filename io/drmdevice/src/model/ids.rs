// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Typed object ids and their single allocator.
//!
//! Every userspace-visible id is a bare `u32` on the wire, but inside the driver
//! it carries the object class in the type system: a framebuffer id cannot be
//! passed where a blob id or a dumb handle is expected, so the "ids of two
//! different tables flow into each other" class of bug is a compile error.

use alloc::collections::BTreeMap;
use core::marker::PhantomData;

/// Tag of framebuffer ids.
pub(crate) struct FbTag;
/// Tag of property blob ids.
pub(crate) struct BlobTag;
/// Tag of dumb-buffer handles (per open file, like GEM handles).
pub(crate) struct DumbTag;
/// Tag of CRTC ids.
pub(crate) struct CrtcTag;
/// Tag of property ids.
pub(crate) struct PropertyTag;
/// Tag of syncobj handles (per open file, like GEM handles).
pub(crate) struct SyncobjTag;

pub(crate) type FbId = Id<FbTag>;
pub(crate) type BlobId = Id<BlobTag>;
pub(crate) type DumbHandle = Id<DumbTag>;
pub(crate) type CrtcId = Id<CrtcTag>;
pub(crate) type PropertyId = Id<PropertyTag>;

/// A userspace-visible object id tagged with its object class.
pub(crate) struct Id<T> {
    raw: u32,
    _class: PhantomData<fn() -> T>,
}

impl<T> Id<T> {
    /// Wraps a numeric id received from userspace.
    pub(crate) const fn from_raw(raw: u32) -> Self {
        Self {
            raw,
            _class: PhantomData,
        }
    }

    /// The wire value.
    pub(crate) const fn raw(self) -> u32 {
        self.raw
    }

    /// Whether this is the "no object" id.
    pub(crate) const fn is_none(self) -> bool {
        self.raw == 0
    }
}

impl<T> Clone for Id<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Id<T> {}

impl<T> PartialEq for Id<T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl<T> Eq for Id<T> {}

impl<T> PartialOrd for Id<T> {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Id<T> {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.raw.cmp(&other.raw)
    }
}

impl<T> core::fmt::Debug for Id<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.raw)
    }
}

impl<T> Default for Id<T> {
    fn default() -> Self {
        Self::from_raw(0)
    }
}

/// Allocates ids of one object class and owns the objects behind them.
///
/// The registry is the only allocator for its class, so two tables cannot hand
/// out the same number for different kinds of object.
pub(crate) struct IdRegistry<T, Tag> {
    next: u32,
    base: u32,
    entries: BTreeMap<u32, T>,
    _tag: PhantomData<fn() -> Tag>,
}

impl<T, Tag> IdRegistry<T, Tag> {
    /// Creates a registry whose ids start at `base`.
    pub(crate) const fn new(base: u32) -> Self {
        Self {
            next: base,
            base,
            entries: BTreeMap::new(),
            _tag: PhantomData,
        }
    }

    /// Stores `value` under a fresh id, or `None` when the id space is full.
    ///
    /// Refusing rather than wrapping matters: wrapping would hand a new object
    /// the id of a live one and silently alias two objects.
    pub(crate) fn insert(&mut self, value: T) -> Option<Id<Tag>> {
        // Bounded scan: at most `entries.len() + 1` probes, so a wrapped
        // counter can never spin. Exhaustion is reported as `None` instead of
        // aliasing a live id.
        let start = self.next.max(self.base.max(1));
        let mut candidate = start;
        let mut probes = self.entries.len() + 1;
        while probes > 0 {
            probes -= 1;
            if !self.entries.contains_key(&candidate) {
                self.next = candidate.wrapping_add(1);
                self.entries.insert(candidate, value);
                return Some(Id::from_raw(candidate));
            }
            candidate = candidate.wrapping_add(1).max(self.base.max(1));
        }
        None
    }

    /// Borrows the object behind `id`.
    pub(crate) fn get(&self, id: Id<Tag>) -> Option<&T> {
        self.entries.get(&id.raw())
    }

    /// Removes and returns the object behind `id`.
    pub(crate) fn remove(&mut self, id: Id<Tag>) -> Option<T> {
        self.entries.remove(&id.raw())
    }

    /// Whether `id` names a live object.
    pub(crate) fn contains(&self, id: Id<Tag>) -> bool {
        self.entries.contains_key(&id.raw())
    }

    /// Every live entry, in id order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (Id<Tag>, &T)> {
        self.entries
            .iter()
            .map(|(raw, value)| (Id::from_raw(*raw), value))
    }
}
