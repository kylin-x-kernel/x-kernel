// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process resource-limit types.

#![no_std]

use core::ops::{Index, IndexMut};

use linux_raw_sys::general::{
    RLIM_NLIMITS, RLIMIT_CORE, RLIMIT_MEMLOCK, RLIMIT_MSGQUEUE, RLIMIT_NICE, RLIMIT_NOFILE,
    RLIMIT_NPROC, RLIMIT_RTPRIO, RLIMIT_SIGPENDING, RLIMIT_STACK,
};

const RLIM_INFINITY: u64 = u64::MAX;
const MLOCK_LIMIT_BYTES: u64 = 8 * 1024 * 1024;
const MSGQUEUE_LIMIT_BYTES: u64 = 819_200;

/// The maximum number of open files supported by the current fd table.
pub const FILE_LIMIT: usize = 1024;

/// The limit for a specific resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rlimit {
    /// The current limit for the resource (soft).
    pub current: u64,
    /// The maximum limit for the resource (hard).
    pub max: u64,
}

impl Rlimit {
    /// The unlimited soft/hard limit pair.
    pub const INFINITY: Self = Self::new(RLIM_INFINITY, RLIM_INFINITY);

    /// Creates a new `Rlimit` with the specified soft and hard limits.
    pub const fn new(soft: u64, hard: u64) -> Self {
        Self {
            current: soft,
            max: hard,
        }
    }
}

impl From<u64> for Rlimit {
    fn from(value: u64) -> Self {
        Self {
            current: value,
            max: value,
        }
    }
}

/// Process resource limits.
pub struct Rlimits([Rlimit; RLIM_NLIMITS as usize]);

impl Rlimits {
    /// Creates a new limit table with Linux-like defaults.
    pub fn new(user_stack_size: usize) -> Self {
        let mut result = Self([Rlimit::INFINITY; RLIM_NLIMITS as usize]);

        // x-kernel currently maps a fixed-size user stack and uses a fixed-capacity
        // fd table, so keep those hard caps at the kernel-supported maximum instead
        // of Linux's larger growable defaults.
        //
        // If stack growth or a resizable fd table is added, revisit these two
        // entries and align their hard limits with the defaults reported
        // through `prlimit64`.
        result[RLIMIT_STACK] = (user_stack_size as u64).into();
        result[RLIMIT_CORE] = Rlimit::new(0, RLIM_INFINITY);
        result[RLIMIT_NPROC] = Rlimit::new(0, 0);
        result[RLIMIT_NOFILE] = (FILE_LIMIT as u64).into();
        result[RLIMIT_MEMLOCK] = Rlimit::new(MLOCK_LIMIT_BYTES, MLOCK_LIMIT_BYTES);
        result[RLIMIT_MSGQUEUE] = Rlimit::new(MSGQUEUE_LIMIT_BYTES, MSGQUEUE_LIMIT_BYTES);
        result[RLIMIT_SIGPENDING] = Rlimit::new(0, 0);
        result[RLIMIT_NICE] = Rlimit::new(0, 0);
        result[RLIMIT_RTPRIO] = Rlimit::new(0, 0);
        result
    }
}

impl Index<u32> for Rlimits {
    type Output = Rlimit;

    fn index(&self, index: u32) -> &Self::Output {
        &self.0[index as usize]
    }
}

impl IndexMut<u32> for Rlimits {
    fn index_mut(&mut self, index: u32) -> &mut Self::Output {
        &mut self.0[index as usize]
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_rlimit_new() {
        let r = Rlimit::new(1, 2);
        assert_eq!(r.current, 1);
        assert_eq!(r.max, 2);
    }

    #[def_test]
    fn test_rlimit_from() {
        let r: Rlimit = 3_u64.into();
        assert_eq!(r.current, 3);
        assert_eq!(r.max, 3);
    }

    #[def_test]
    fn test_rlimits_new() {
        let limits = Rlimits::new(0x80000);
        assert_eq!(limits[RLIMIT_STACK].current, 0x80000);
        assert_eq!(limits[RLIMIT_NOFILE].current, FILE_LIMIT as u64);
        assert_eq!(limits[linux_raw_sys::general::RLIMIT_CPU], Rlimit::INFINITY);
        assert_eq!(limits[RLIMIT_CORE], Rlimit::new(0, RLIM_INFINITY));
    }

    #[def_test]
    fn test_rlimits_index_mut_updates_selected_limit() {
        let mut limits = Rlimits::new(0x80000);
        limits[RLIMIT_NOFILE] = Rlimit::new(128, 256);

        assert_eq!(limits[RLIMIT_NOFILE].current, 128);
        assert_eq!(limits[RLIMIT_NOFILE].max, 256);
        assert_eq!(limits[RLIMIT_STACK].current, 0x80000);
    }

    #[def_test]
    fn test_rlimits_preserve_defaults() {
        let limits = Rlimits::new(0x80000);
        assert_eq!(limits[RLIMIT_STACK].max, 0x80000);
        assert_eq!(limits[RLIMIT_NOFILE].max, FILE_LIMIT as u64);
        assert_eq!(
            limits[RLIMIT_MEMLOCK],
            Rlimit::new(MLOCK_LIMIT_BYTES, MLOCK_LIMIT_BYTES)
        );
        assert_eq!(
            limits[RLIMIT_MSGQUEUE],
            Rlimit::new(MSGQUEUE_LIMIT_BYTES, MSGQUEUE_LIMIT_BYTES)
        );
    }
}
