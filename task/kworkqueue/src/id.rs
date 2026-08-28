// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Opaque identities shared by workqueue state and executor backends.

use core::num::NonZeroUsize;

/// Identity of an external queue/binding that owns submitted entries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntryOwner(NonZeroUsize);

impl EntryOwner {
    pub const fn new(raw: NonZeroUsize) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> usize {
        self.0.get()
    }
}

/// Stable identity of one queue binding.
///
/// Executor entries carry this id so runtime claim paths can return directly to
/// the binding that produced the entry. It is separate from [`EntryOwner`],
/// which remains the worker-pool grouping key used for inactive promotion.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BindingId(NonZeroUsize);

impl BindingId {
    pub const fn new(raw: NonZeroUsize) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> usize {
        self.0.get()
    }
}

/// Identity of one executor-visible pending entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntryKey(NonZeroUsize);

impl EntryKey {
    pub const fn new(raw: NonZeroUsize) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> usize {
        self.0.get()
    }
}

/// Opaque executor payload passed back when a pending entry is claimed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntryPayload(NonZeroUsize);

impl EntryPayload {
    pub const fn new(raw: NonZeroUsize) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> usize {
        self.0.get()
    }
}

/// Stable identity of a `Work` object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkKey {
    address: NonZeroUsize,
    generation: NonZeroUsize,
}

impl WorkKey {
    pub(crate) fn from_parts<T>(work: &T, generation: NonZeroUsize) -> Self {
        let raw = core::ptr::from_ref(work).addr();
        Self {
            address: NonZeroUsize::new(raw).expect("kernel object addresses are non-zero"),
            generation,
        }
    }

    pub const fn address(self) -> usize {
        self.address.get()
    }

    pub const fn generation(self) -> usize {
        self.generation.get()
    }
}

/// Monotonic per-work pending instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkInstanceId(u64);

impl WorkInstanceId {
    pub(crate) const FIRST: Self = Self(1);

    pub(crate) fn next(self) -> Self {
        Self(self.0.wrapping_add(1).max(1))
    }

    pub(crate) const fn as_key(self) -> EntryKey {
        EntryKey(NonZeroUsize::new(self.0 as usize).expect("work instance id is non-zero"))
    }

    #[cfg(unittest)]
    pub const fn for_tests(raw: u64) -> Self {
        Self(raw)
    }
}

/// Flush generation. A flush snapshots one color and later queues use the next.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkColor(u8);

impl WorkColor {
    pub(crate) const COUNT: usize = 16;
    pub(crate) const DEFAULT: Self = Self(0);

    pub(crate) const fn next(self) -> Self {
        Self((self.0 + 1) & 0x0f)
    }

    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingRecordId(usize);

impl PendingRecordId {
    const SLOT_BITS: usize = 16;
    const SLOT_MASK: usize = (1 << Self::SLOT_BITS) - 1;

    pub(crate) const fn new(slot: usize, generation: usize) -> Self {
        Self((generation << Self::SLOT_BITS) | slot)
    }

    pub(crate) const fn slot(self) -> usize {
        self.0 & Self::SLOT_MASK
    }

    pub(crate) const fn generation(self) -> usize {
        self.0 >> Self::SLOT_BITS
    }

    pub(crate) fn payload(self) -> EntryPayload {
        EntryPayload::new(NonZeroUsize::new(self.0.wrapping_add(1)).expect("payload is non-zero"))
    }

    pub(crate) fn from_payload(payload: EntryPayload) -> Self {
        Self(payload.get().wrapping_sub(1))
    }
}
