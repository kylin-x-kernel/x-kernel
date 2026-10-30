// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Atomic entry counters and plain values for IPv4 filter statistics.
//!
//! Concurrent updates and reads use independent atomics. Samples of a live
//! table need not keep packet/byte pairs or separate rules mutually consistent.
//! Once the owner retires a table and waits for its updates, its totals are final.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

/// Packet and byte totals for one semantic rule or chain completion.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct FilterCounter {
    pub(crate) packets: u64,
    pub(crate) bytes: u64,
}

/// Counters ordered by rule position, followed by the chain-completion counter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChainCounters {
    pub(crate) rules: Vec<FilterCounter>,
    pub(crate) end: FilterCounter,
}

/// A counter update whose chain or rule counts differ from the table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CounterShapeError;

/// Independent atomic packet and byte totals for one rule or chain completion.
#[derive(Debug)]
pub(super) struct EntryCounter {
    packets: AtomicU64,
    bytes: AtomicU64,
}

impl EntryCounter {
    /// Creates zeroed packet and byte totals.
    pub(super) fn new() -> Self {
        Self {
            packets: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }

    /// Samples the packet and byte fields independently.
    pub(super) fn load(&self) -> FilterCounter {
        FilterCounter {
            packets: self.packets.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }

    /// Adds packet and byte totals independently.
    pub(super) fn add(&self, packets: u64, bytes: u64) {
        // Counters publish no other state; the owner waits for updates before retiring the table.
        self.packets.fetch_add(packets, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}
