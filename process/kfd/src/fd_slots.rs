// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fixed-capacity numbered slot storage for the descriptor table.
//!
//! [`FileSlots`] keeps the value storage, the occupancy bitmap, and the
//! free-slot lower bound in one place, so every mutation path maintains the
//! bound next to the bits it describes. Range allocation scans the bitmap one
//! word at a time instead of testing individual ids, so the cost of finding the
//! lowest free id in `[minimum, end)` depends on the number of fully occupied
//! words, not on the width of the range.
//!
//! # Occupancy invariant
//!
//! A set bit means the slot already holds an initialized value. Four paths
//! change the bitmap, and each upholds the invariant by ordering:
//!
//! - `place` writes the value before setting the bit, so insertion never
//!   publishes an unwritten slot. Both steps are infallible, so this order is not
//!   observable on its own; it keeps the rule uniform and stays correct if a
//!   fallible step is ever inserted between them;
//! - `clone_from` writes every clone before publishing the copied bitmap, so a
//!   panic in `T::clone` leaves nothing published (clones already written leak,
//!   which is safe);
//! - `clear` clears the bitmap before dropping the values it snapshotted, so a
//!   panic in `T::drop` cannot lead to a second drop (values not dropped yet leak
//!   instead);
//! - `remove` clears the single bit before reading the value out, so the slot is
//!   no longer reachable through the bitmap by the time its value moves to the
//!   caller. Both steps are infallible as well, and the order follows the same
//!   rule.
//!
//! `get`, `get_mut` and `remove` therefore use the bit alone to decide whether a
//! slot may be read, and `clear` to decide which slots must be dropped.
//!
//! # Provenance
//!
//! This is an in-tree implementation. Its layout - a value array plus an
//! occupancy bitmap - follows the upstream `flatten_objects` crate that it
//! replaces, whose local fork under `third_party/` was removed with this change,
//! but it retains no upstream source lines and adds the range search, the
//! free-slot bound and its own id iterator.

use core::mem::MaybeUninit;

/// Physical descriptor capacity, matching the [`krlimit::FILE_LIMIT`] hard limit.
const CAPACITY: usize = krlimit::FILE_LIMIT;
/// Number of bits in one occupancy bitmap word.
const WORD_BITS: usize = u64::BITS as usize;
/// Number of words needed to cover [`CAPACITY`] slots.
const WORDS: usize = CAPACITY.div_ceil(WORD_BITS);

/// Mask with the `count` lowest bits set, for `count < 64`.
const fn low_bits(count: usize) -> u64 {
    (1u64 << count) - 1
}

/// Finds the lowest assigned id at or after `from`.
fn assigned_at_or_after(occupied: &[u64; WORDS], from: usize) -> Option<usize> {
    let mut word = from / WORD_BITS;
    if word >= WORDS {
        return None;
    }
    // Clear the bits below `from` so the first candidate is `from` itself.
    let mut bits = occupied[word] & !low_bits(from % WORD_BITS);
    loop {
        if bits != 0 {
            return Some(word * WORD_BITS + bits.trailing_zeros() as usize);
        }
        word += 1;
        if word >= WORDS {
            return None;
        }
        bits = occupied[word];
    }
}

/// Finds the highest assigned id at or before `from`.
fn assigned_at_or_before(occupied: &[u64; WORDS], from: usize) -> Option<usize> {
    // Clamp so callers that pass an id beyond the capacity still index in range;
    // the highest assigned id remains the right answer for them.
    let from = from.min(CAPACITY - 1);
    let mut word = from / WORD_BITS;
    // Keep the bits at or below `from`.
    let mut bits = occupied[word] & (u64::MAX >> (WORD_BITS - 1 - (from % WORD_BITS)));
    loop {
        if bits != 0 {
            return Some(word * WORD_BITS + (WORD_BITS - 1 - bits.leading_zeros() as usize));
        }
        if word == 0 {
            return None;
        }
        word -= 1;
        bits = occupied[word];
    }
}

/// Iterator over the assigned ids of a [`FileSlots`], in ascending order.
///
/// Both ends stay valid because the cursor pair only ever narrows: a yielded id
/// is excluded by advancing the cursor that produced it.
pub(crate) struct OccupiedIds<'a> {
    occupied: &'a [u64; WORDS],
    /// Lowest id that may still be yielded from the front.
    front: usize,
    /// Highest id that may still be yielded from the back.
    back: usize,
}

impl<'a> OccupiedIds<'a> {
    fn new(occupied: &'a [u64; WORDS]) -> Self {
        Self {
            occupied,
            front: 0,
            back: CAPACITY - 1,
        }
    }

    /// Marks the remaining range empty.
    fn exhaust(&mut self) {
        self.front = self.back + 1;
    }
}

impl Iterator for OccupiedIds<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        if self.front > self.back {
            return None;
        }
        match assigned_at_or_after(self.occupied, self.front) {
            Some(id) if id <= self.back => {
                self.front = id + 1;
                Some(id)
            }
            _ => {
                self.exhaust();
                None
            }
        }
    }
}

impl DoubleEndedIterator for OccupiedIds<'_> {
    fn next_back(&mut self) -> Option<usize> {
        if self.front > self.back {
            return None;
        }
        match assigned_at_or_before(self.occupied, self.back) {
            Some(id) if id >= self.front => {
                if id == 0 {
                    // The front cursor is the only way to mark exhaustion past id 0.
                    self.front = 1;
                } else {
                    self.back = id - 1;
                }
                Some(id)
            }
            _ => {
                self.exhaust();
                None
            }
        }
    }
}

/// Numbered slot storage of fixed capacity with occupancy tracking.
///
/// Invariant: every id below `next_free` is assigned. The bound may therefore be
/// used as a starting point for allocation, and it must never over-estimate or a
/// lower free slot would be skipped. A loose bound is allowed: staying below the
/// first free id only costs a bounded bitmap walk, and every release lowers it.
pub(crate) struct FileSlots<T> {
    slots: [MaybeUninit<T>; CAPACITY],
    occupied: [u64; WORDS],
    next_free: usize,
    count: usize,
}

impl<T> FileSlots<T> {
    /// Creates empty slot storage.
    pub(crate) const fn new() -> Self {
        Self {
            slots: [const { MaybeUninit::uninit() }; CAPACITY],
            occupied: [0; WORDS],
            next_free: 0,
            count: 0,
        }
    }

    /// Returns the number of assigned slots.
    pub(crate) const fn count(&self) -> usize {
        self.count
    }

    /// Returns the free-slot lower bound for invariant checks in unit tests.
    #[cfg(unittest)]
    pub(crate) const fn next_free(&self) -> usize {
        self.next_free
    }

    /// Checks whether `id` is assigned.
    fn is_assigned(&self, id: usize) -> bool {
        id < CAPACITY && self.bit_is_set(id)
    }

    /// Returns the value in `id` if it is assigned.
    pub(crate) fn get(&self, id: usize) -> Option<&T> {
        if self.is_assigned(id) {
            // SAFETY: the module occupancy invariant guarantees that an assigned
            // slot holds an initialized value, so this read is sound.
            Some(unsafe { self.slots[id].assume_init_ref() })
        } else {
            None
        }
    }

    /// Returns a mutable reference to the value in `id` if it is assigned.
    pub(crate) fn get_mut(&mut self, id: usize) -> Option<&mut T> {
        if self.is_assigned(id) {
            // SAFETY: see `get`; the module occupancy invariant still holds, and
            // `&mut self` excludes aliasing readers.
            Some(unsafe { self.slots[id].assume_init_mut() })
        } else {
            None
        }
    }

    /// Iterates the assigned ids in ascending order.
    pub(crate) fn ids(&self) -> OccupiedIds<'_> {
        OccupiedIds::new(&self.occupied)
    }

    /// Assigns `value` the lowest free id.
    ///
    /// Returns the unassigned value when every slot is taken.
    pub(crate) fn add(&mut self, value: T) -> Result<usize, T> {
        // A zero minimum always starts at the bound, so the first free id found
        // is the lowest free id overall.
        self.alloc(0, CAPACITY, value)
    }

    /// Assigns `value` the lowest free id in `[minimum, end)`.
    ///
    /// Returns the unassigned value when that range holds no free id. The
    /// exclusive end is clipped to [`CAPACITY`].
    pub(crate) fn alloc(&mut self, minimum: usize, end: usize, value: T) -> Result<usize, T> {
        let end = end.min(CAPACITY);
        let bound = self.next_free;
        let start = minimum.max(bound);
        if start >= end {
            return Err(value);
        }

        let mut word = start / WORD_BITS;
        // Treat the bits below `start` as assigned so the first candidate is
        // `start` itself, then skip fully assigned words without visiting their
        // individual bits.
        let mut bits = self.occupied[word] | low_bits(start % WORD_BITS);
        let id = loop {
            if bits != u64::MAX {
                break word * WORD_BITS + (!bits).trailing_zeros() as usize;
            }
            word += 1;
            if word * WORD_BITS >= end {
                return Err(value);
            }
            bits = self.occupied[word];
        };
        // The last word can hold free ids past an unaligned `end`.
        if id >= end {
            return Err(value);
        }

        self.place(id, value);
        // Everything below the new occupant is assigned only when the search
        // began at the bound: a higher `minimum` can leave holes below it.
        if start == bound {
            self.next_free = id + 1;
        }
        Ok(id)
    }

    /// Assigns `value` to `id`, which must be free.
    ///
    /// Returns the unassigned value when `id` is out of range or already taken.
    pub(crate) fn add_at(&mut self, id: usize, value: T) -> Result<usize, T> {
        if id >= CAPACITY || self.bit_is_set(id) {
            return Err(value);
        }
        self.place(id, value);
        // A fixed slot can sit above a hole, so the bound only moves when the
        // fixed slot was the bound itself.
        if id == self.next_free {
            self.next_free = id + 1;
        }
        Ok(id)
    }

    /// Removes and returns the value in `id` if it is assigned.
    pub(crate) fn remove(&mut self, id: usize) -> Option<T> {
        if !self.is_assigned(id) {
            return None;
        }
        self.clear_bit(id);
        self.count -= 1;
        // The freed slot can be the lowest free id again.
        if id < self.next_free {
            self.next_free = id;
        }
        // SAFETY: the occupancy bit proved the slot held an initialized value
        // (module occupancy invariant), and it is already cleared, so no other
        // path can observe the value read here.
        Some(unsafe { self.slots[id].assume_init_read() })
    }

    /// Replaces the stored values with clones of `other`'s values.
    pub(crate) fn clone_from(&mut self, other: &Self)
    where
        T: Clone,
    {
        self.clear();
        // Write the values before publishing the occupancy, because `T::clone`
        // may unwind and a published bit must never refer to an unwritten slot.
        // A panic here leaks the clones already written, which is safe.
        for id in other.ids() {
            if let Some(value) = other.get(id) {
                self.slots[id].write(value.clone());
            }
        }
        self.occupied = other.occupied;
        self.count = other.count;
        // The occupancy is identical, so the source bound holds here as well.
        self.next_free = other.next_free;
    }

    /// Drops every assigned value and empties the storage.
    fn clear(&mut self) {
        // Unpublish the occupancy before dropping: `T::drop` may unwind, and a
        // bit surviving a partial drop would make a later `Drop` drop the value
        // twice. Values not dropped yet leak instead, which is safe.
        let assigned = self.occupied;
        self.occupied = [0; WORDS];
        self.count = 0;
        self.next_free = 0;
        for id in OccupiedIds::new(&assigned) {
            // SAFETY: `assigned` is a snapshot of the slots that held values
            // (module occupancy invariant) and the live bitmap no longer claims
            // them, so each value is dropped exactly once even when a drop
            // unwinds.
            unsafe { self.slots[id].assume_init_drop() };
        }
    }

    /// Writes `value` into the free slot `id` and marks it assigned.
    ///
    /// The value is written before the occupancy bit, so a published bit always
    /// refers to an initialized slot.
    fn place(&mut self, id: usize, value: T) {
        self.slots[id].write(value);
        self.set_bit(id);
        self.count += 1;
    }

    fn bit_is_set(&self, id: usize) -> bool {
        self.occupied[id / WORD_BITS] & (1 << (id % WORD_BITS)) != 0
    }

    fn set_bit(&mut self, id: usize) {
        self.occupied[id / WORD_BITS] |= 1 << (id % WORD_BITS);
    }

    fn clear_bit(&mut self, id: usize) {
        self.occupied[id / WORD_BITS] &= !(1 << (id % WORD_BITS));
    }
}

impl<T> Drop for FileSlots<T> {
    fn drop(&mut self) {
        self.clear();
    }
}
