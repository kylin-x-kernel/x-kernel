// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `cpuset` controller: CPU and NUMA-node placement.
//!
//! Each cgroup stores the mask its own `cpuset.cpus`/`cpuset.mems` files
//! requested (possibly empty, meaning "inherit") and the effective mask that is
//! the intersection with every ancestor. Effective masks are the values the
//! scheduler is expected to enforce and are exposed read-only through
//! `cpuset.cpus.effective` and `cpuset.mems.effective`.
//!
//! The machine-wide universe comes from the CPU id map at hierarchy creation
//! time; unit tests construct a root with an explicit universe so mask
//! arithmetic is testable before architecture initialization.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::{
    fmt,
    ops::{BitAnd, BitAndAssign, BitOrAssign},
};

use kerrno::{KError, KResult, LinuxError};
use ksync::RwLock;

use super::{Controller, ControllerKind};

/// Largest CPU or NUMA-node index representable in an [`IndexMask`].
pub const MAX_CPUS: usize = 1024;

const MASK_WORDS: usize = MAX_CPUS / 64;

/// A fixed-size set of CPU or NUMA-node indexes.
///
/// cgroup control files use the Linux cpulist format (`0-3,8`), which is what
/// [`parse_list`](Self::parse_list) and [`format_list`](Self::format_list)
/// implement. Indexes at or above [`MAX_CPUS`] are rejected instead of being
/// silently truncated.
#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub struct IndexMask {
    words: [u64; MASK_WORDS],
}

/// CPU set stored in `cpuset.cpus` and `cpuset.cpus.effective`.
pub type CpuMask = IndexMask;

/// NUMA-node set stored in `cpuset.mems` and `cpuset.mems.effective`.
pub type NodeMask = IndexMask;

impl IndexMask {
    /// Returns the empty set.
    pub const fn empty() -> Self {
        Self {
            words: [0; MASK_WORDS],
        }
    }

    /// Returns a set containing exactly `index`, or `None` when out of range.
    pub fn from_index(index: usize) -> Option<Self> {
        let mut mask = Self::empty();
        mask.insert(index).then_some(mask)
    }

    /// Inserts `index` and reports whether it was representable.
    pub fn insert(&mut self, index: usize) -> bool {
        if index >= MAX_CPUS {
            return false;
        }
        self.words[index / 64] |= 1 << (index % 64);
        true
    }

    /// Removes `index`, ignoring out-of-range values.
    pub fn remove(&mut self, index: usize) {
        if index < MAX_CPUS {
            self.words[index / 64] &= !(1 << (index % 64));
        }
    }

    /// Reports whether `index` is a member.
    pub fn contains(&self, index: usize) -> bool {
        index < MAX_CPUS && self.words[index / 64] & (1 << (index % 64)) != 0
    }

    /// Reports whether the set has no members.
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|word| *word == 0)
    }

    /// Returns the number of members.
    pub fn len(&self) -> usize {
        self.words
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    /// Returns the smallest member, if any.
    pub fn first(&self) -> Option<usize> {
        self.iter().next()
    }

    /// Iterates members in ascending order.
    pub fn iter(&self) -> IndexMaskIter<'_> {
        IndexMaskIter {
            mask: self,
            word_index: 0,
            word: self.words[0],
        }
    }

    /// Returns the intersection of two sets.
    pub fn intersection(&self, other: &Self) -> Self {
        let mut result = Self::empty();
        for (index, word) in result.words.iter_mut().enumerate() {
            *word = self.words[index] & other.words[index];
        }
        result
    }

    /// Returns the union of two sets.
    pub fn union(&self, other: &Self) -> Self {
        let mut result = Self::empty();
        for (index, word) in result.words.iter_mut().enumerate() {
            *word = self.words[index] | other.words[index];
        }
        result
    }

    /// Reports whether every member of `self` is also a member of `other`.
    pub fn is_subset(&self, other: &Self) -> bool {
        self.words
            .iter()
            .zip(other.words.iter())
            .all(|(left, right)| left & !right == 0)
    }

    /// Parses a Linux cpulist such as `0-3,8`.
    ///
    /// An empty string is a valid empty list, which `cpuset.cpus` and
    /// `cpuset.mems` interpret as "inherit from the parent".
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` for malformed ranges, reversed ranges, non-numeric
    /// components, or indexes at or above [`MAX_CPUS`].
    pub fn parse_list(text: &str) -> KResult<Self> {
        let text = text.trim();
        let mut mask = Self::empty();
        if text.is_empty() {
            return Ok(mask);
        }
        for component in text.split(',') {
            let component = component.trim();
            if component.is_empty() {
                return Err(KError::from(LinuxError::EINVAL));
            }
            let mut bounds = component.split('-');
            let start = parse_index(bounds.next())?;
            let end = match bounds.next() {
                None => start,
                Some(end) => parse_index(Some(end))?,
            };
            if bounds.next().is_some() || start > end {
                return Err(KError::from(LinuxError::EINVAL));
            }
            for index in start..=end {
                if !mask.insert(index) {
                    return Err(KError::from(LinuxError::EINVAL));
                }
            }
        }
        Ok(mask)
    }

    /// Formats the set as a Linux cpulist such as `0-3,8`.
    pub fn format_list(&self) -> String {
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for index in self.iter() {
            match ranges.last_mut() {
                Some((_, end)) if *end + 1 == index => *end = index,
                _ => ranges.push((index, index)),
            }
        }
        let mut text = String::new();
        for (position, (start, end)) in ranges.into_iter().enumerate() {
            if position != 0 {
                text.push(',');
            }
            if start == end {
                text.push_str(&start.to_string());
            } else {
                text.push_str(&start.to_string());
                text.push('-');
                text.push_str(&end.to_string());
            }
        }
        text
    }
}

fn parse_index(text: Option<&str>) -> KResult<usize> {
    let text = text.ok_or_else(|| KError::from(LinuxError::EINVAL))?;
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(KError::from(LinuxError::EINVAL));
    }
    text.parse::<usize>()
        .map_err(|_| KError::from(LinuxError::EINVAL))
}

impl fmt::Debug for IndexMask {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("IndexMask")
            .field(&self.format_list())
            .finish()
    }
}

/// Iterator over the members of an [`IndexMask`] in ascending order.
pub struct IndexMaskIter<'a> {
    mask: &'a IndexMask,
    word_index: usize,
    word: u64,
}

impl Iterator for IndexMaskIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.word != 0 {
                let bit = self.word.trailing_zeros() as usize;
                self.word &= self.word - 1;
                return Some(self.word_index * 64 + bit);
            }
            self.word_index += 1;
            if self.word_index >= MASK_WORDS {
                return None;
            }
            self.word = self.mask.words[self.word_index];
        }
    }
}

/// Per-cgroup `cpuset` controller state.
pub struct CpusetController {
    cpus: RwLock<IndexMask>,
    mems: RwLock<IndexMask>,
    effective_cpus: RwLock<IndexMask>,
    effective_mems: RwLock<IndexMask>,
}

impl CpusetController {
    /// Creates a controller with empty own masks and explicit effective masks.
    pub fn new(effective_cpus: IndexMask, effective_mems: IndexMask) -> Self {
        Self {
            cpus: RwLock::new(IndexMask::empty()),
            mems: RwLock::new(IndexMask::empty()),
            effective_cpus: RwLock::new(effective_cpus),
            effective_mems: RwLock::new(effective_mems),
        }
    }

    /// Returns the mask requested through `cpuset.cpus` (empty means inherit).
    pub fn cpus(&self) -> IndexMask {
        *self.cpus.read()
    }

    /// Returns the mask requested through `cpuset.mems` (empty means inherit).
    pub fn mems(&self) -> IndexMask {
        *self.mems.read()
    }

    /// Returns the effective CPU mask visible to the scheduler.
    pub fn effective_cpus(&self) -> IndexMask {
        *self.effective_cpus.read()
    }

    /// Returns the effective NUMA-node mask.
    pub fn effective_mems(&self) -> IndexMask {
        *self.effective_mems.read()
    }

    /// Stores a new `cpuset.cpus` request and recomputes the effective mask.
    ///
    /// Callers must validate the mask against the parent's effective mask
    /// first; this method only performs the inheritance rule.
    pub fn set_cpus(&self, cpus: IndexMask, parent_effective: IndexMask) {
        *self.cpus.write() = cpus;
        let effective = if cpus.is_empty() {
            parent_effective
        } else {
            cpus.intersection(&parent_effective)
        };
        *self.effective_cpus.write() = effective;
    }

    /// Stores a new `cpuset.mems` request and recomputes the effective mask.
    pub fn set_mems(&self, mems: IndexMask, parent_effective: IndexMask) {
        *self.mems.write() = mems;
        let effective = if mems.is_empty() {
            parent_effective
        } else {
            mems.intersection(&parent_effective)
        };
        *self.effective_mems.write() = effective;
    }

    /// Recomputes the effective masks after an ancestor changed.
    pub fn refresh_effective(&self, parent_cpus: IndexMask, parent_mems: IndexMask) {
        let cpus = self.cpus.read();
        let mems = self.mems.read();
        let effective_cpus = if cpus.is_empty() {
            parent_cpus
        } else {
            cpus.intersection(&parent_cpus)
        };
        let effective_mems = if mems.is_empty() {
            parent_mems
        } else {
            mems.intersection(&parent_mems)
        };
        let mut current_cpus = self.effective_cpus.write();
        let mut current_mems = self.effective_mems.write();
        *current_cpus = effective_cpus;
        *current_mems = effective_mems;
    }

    /// Restores a previously captured effective mask pair during rollback.
    pub(crate) fn restore_effective(&self, cpus: IndexMask, mems: IndexMask) {
        *self.effective_cpus.write() = cpus;
        *self.effective_mems.write() = mems;
    }

    /// Resets the controller to an inactive-equivalent state.
    pub fn clear(&self) {
        *self.cpus.write() = IndexMask::empty();
        *self.mems.write() = IndexMask::empty();
        *self.effective_cpus.write() = IndexMask::empty();
        *self.effective_mems.write() = IndexMask::empty();
    }
}

impl Controller for CpusetController {
    fn kind(&self) -> ControllerKind {
        ControllerKind::Cpuset
    }

    fn deactivate(&self) {
        self.clear();
    }
}

impl BitAnd for IndexMask {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        self.intersection(&rhs)
    }
}

impl BitAndAssign for IndexMask {
    fn bitand_assign(&mut self, rhs: Self) {
        *self = self.intersection(&rhs);
    }
}

impl BitOrAssign for IndexMask {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = self.union(&rhs);
    }
}
