// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn overlapping_io_counts_busy_time_once_and_queue_time_per_request() {
    let mut accounting = IoAccounting::default();
    accounting.start(Some(100));
    accounting.start(Some(150));
    let pending = accounting.snapshot(Some(200));
    assert_eq!(pending.in_flight, 2);
    assert_eq!(pending.read_ios, 0);
    assert_eq!(pending.io_time_ns, 100);
    assert_eq!(pending.weighted_time_ns, 150);

    accounting.complete(BioOperation::Read, 8, Some(100), Some(250));
    accounting.complete(BioOperation::Write, 16, Some(150), Some(300));
    let completed = accounting.snapshot(Some(50_000));
    assert_eq!(completed.in_flight, 0);
    assert_eq!(completed.read_ios, 1);
    assert_eq!(completed.read_sectors, 8);
    assert_eq!(completed.write_ios, 1);
    assert_eq!(completed.write_sectors, 16);
    assert_eq!(completed.read_time_ns, 150);
    assert_eq!(completed.write_time_ns, 150);
    assert_eq!(completed.io_time_ns, 200);
    assert_eq!(completed.weighted_time_ns, 300);
}

#[def_test]
fn retiring_an_abandoned_io_stops_charging_its_queue_time() {
    let mut accounting = IoAccounting::default();
    accounting.start(Some(100));
    accounting.start(Some(150));
    accounting.retire_in_flight(Some(200));
    accounting.complete(BioOperation::Read, 1, Some(150), Some(250));

    assert_eq!(
        accounting.snapshot(Some(1000)),
        BlockIoStats {
            read_ios: 1,
            read_sectors: 1,
            read_time_ns: 100,
            io_time_ns: 150,
            weighted_time_ns: 200,
            ..BlockIoStats::default()
        }
    );
}

#[def_test]
fn sub_millisecond_io_accumulates_without_charging_idle_time() {
    let mut accounting = IoAccounting::default();
    for started_ns in [0, 1_000_000, 2_000_000] {
        accounting.start(Some(started_ns));
        accounting.complete(
            BioOperation::Write,
            1,
            Some(started_ns),
            Some(started_ns + 400_000),
        );
    }
    let completed = accounting.snapshot(Some(10_000_000));
    assert_eq!(completed.write_time_ns, 1_200_000);
    assert_eq!(completed.io_time_ns, 1_200_000);
    assert_eq!(completed.weighted_time_ns, 1_200_000);
    assert_eq!(completed.write_sectors, 3);
}

#[def_test]
fn failed_io_and_flush_retire_without_inventing_sectors_or_time() {
    let mut accounting = IoAccounting::default();
    accounting.start(None);
    accounting.complete(BioOperation::Write, 0, None, None);
    accounting.start(None);
    accounting.complete(BioOperation::Flush, 0, None, None);
    let stats = accounting.snapshot(None);
    assert_eq!(stats.write_ios, 1);
    assert_eq!(stats.write_sectors, 0);
    assert_eq!(stats.flush_ios, 1);
    assert_eq!(stats.in_flight, 0);
    assert_eq!(stats.io_time_ns, 0);
}
