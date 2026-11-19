// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Disk-owned I/O accounting, shared by direct-bio and request backends.

use driver_os_api::TimeOp;
use klazy::Once;

use crate::BioOperation;

static IO_CLOCK: Once<&'static dyn TimeOp> = Once::new();

/// Installs the host monotonic clock before block I/O consumers start.
///
/// The provider must support the non-sleeping IRQ/softirq context of bio
/// completion. Without a provider, operation/sector counts remain available
/// but timing is unavailable and remains zero; no synthetic time is added.
pub fn init_io_clock(clock: &'static dyn TimeOp) {
    IO_CLOCK.call_once(|| clock);
}

pub(crate) fn now_ns() -> Option<u64> {
    IO_CLOCK
        .get()
        .map(|clock| clock.monotonic_time().as_nanos_u64_saturating())
}

/// One consistent snapshot of a disk's accepted I/O, including all its views.
///
/// Counts advance on terminal completion, including failed operations. Sector
/// counts advance only on success because backends report no partial progress.
/// Sectors are always 512 bytes. Times accumulate in nanoseconds so sub-ms
/// operations are retained when a consumer converts the totals to milliseconds.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BlockIoStats {
    /// Completed read operations.
    pub read_ios: u64,
    /// Successfully read 512-byte sectors.
    pub read_sectors: u64,
    /// Cumulative read latency, in nanoseconds.
    pub read_time_ns: u64,
    /// Completed write operations.
    pub write_ios: u64,
    /// Successfully written 512-byte sectors.
    pub write_sectors: u64,
    /// Cumulative write latency, in nanoseconds.
    pub write_time_ns: u64,
    /// Accepted operations whose bio is still owned and awaiting completion.
    pub in_flight: u64,
    /// Time with at least one outstanding operation, in nanoseconds.
    pub io_time_ns: u64,
    /// Integral of outstanding-operation count over time, in nanoseconds.
    pub weighted_time_ns: u64,
    /// Completed explicit flush operations.
    pub flush_ios: u64,
    /// Cumulative explicit flush latency, in nanoseconds.
    pub flush_time_ns: u64,
}

#[derive(Default)]
pub(crate) struct IoAccounting {
    values: BlockIoStats,
    updated_ns: Option<u64>,
}

impl IoAccounting {
    fn update_time(&mut self, now_ns: Option<u64>) {
        if let Some(now_ns) = now_ns {
            if let Some(previous_ns) = self.updated_ns {
                let elapsed_ns = now_ns.saturating_sub(previous_ns);
                if self.values.in_flight != 0 {
                    self.values.io_time_ns = self.values.io_time_ns.saturating_add(elapsed_ns);
                    self.values.weighted_time_ns = self
                        .values
                        .weighted_time_ns
                        .saturating_add(elapsed_ns.saturating_mul(self.values.in_flight));
                }
            }
            self.updated_ns = Some(now_ns);
        }
    }

    pub(crate) fn start(&mut self, now_ns: Option<u64>) {
        self.update_time(now_ns);
        self.values.in_flight += 1;
    }

    pub(crate) fn retire_in_flight(&mut self, now_ns: Option<u64>) {
        self.update_time(now_ns);
        self.values.in_flight -= 1;
    }

    pub(crate) fn complete(
        &mut self,
        operation: BioOperation,
        sectors: u64,
        started_ns: Option<u64>,
        now_ns: Option<u64>,
    ) {
        self.retire_in_flight(now_ns);
        let elapsed_ns = now_ns
            .zip(started_ns)
            .map_or(0, |(now_ns, started_ns)| now_ns.saturating_sub(started_ns));
        let (ios, completed_sectors, time_ns) = match operation {
            BioOperation::Read => (
                &mut self.values.read_ios,
                Some(&mut self.values.read_sectors),
                &mut self.values.read_time_ns,
            ),
            BioOperation::Write => (
                &mut self.values.write_ios,
                Some(&mut self.values.write_sectors),
                &mut self.values.write_time_ns,
            ),
            BioOperation::Flush => (
                &mut self.values.flush_ios,
                None,
                &mut self.values.flush_time_ns,
            ),
        };
        *ios = ios.wrapping_add(1);
        if let Some(completed_sectors) = completed_sectors {
            *completed_sectors = completed_sectors.wrapping_add(sectors);
        }
        *time_ns = time_ns.saturating_add(elapsed_ns);
    }

    pub(crate) fn snapshot(&mut self, now_ns: Option<u64>) -> BlockIoStats {
        self.update_time(now_ns);
        self.values
    }
}

unittest::path_mod_test!(tests, "../ut-tests/stats.rs");
