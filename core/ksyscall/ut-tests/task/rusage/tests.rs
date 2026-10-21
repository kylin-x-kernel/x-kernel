// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktime_types::TimeSpan;
use unittest::def_test;

use super::Rusage;

#[def_test]
fn rusage_collate_adds_user_and_system_time() {
    let a = Rusage {
        utime: TimeSpan::new(1, 500_000_000),
        stime: TimeSpan::new(2, 0),
    };
    let b = Rusage {
        utime: TimeSpan::new(0, 500_000_000),
        stime: TimeSpan::new(3, 0),
    };

    let c = a.collate(b);

    assert_eq!(c.utime, TimeSpan::new(2, 0));
    assert_eq!(c.stime, TimeSpan::new(5, 0));
}

#[def_test]
fn rusage_collate_default_is_identity() {
    let a = Rusage {
        utime: TimeSpan::new(7, 0),
        stime: TimeSpan::new(8, 0),
    };

    let c = a.collate(Rusage::default());

    assert_eq!(c.utime, TimeSpan::new(7, 0));
    assert_eq!(c.stime, TimeSpan::new(8, 0));
}

#[def_test]
fn rusage_collate_normalizes_nanoseconds_into_seconds() {
    let a = Rusage {
        utime: TimeSpan::new(0, 600_000_000),
        stime: TimeSpan::new(0, 0),
    };
    let b = Rusage {
        utime: TimeSpan::new(0, 600_000_000),
        stime: TimeSpan::new(0, 0),
    };

    let c = a.collate(b);

    assert_eq!(c.utime, TimeSpan::new(1, 200_000_000));
    assert_eq!(c.stime, TimeSpan::new(0, 0));
}
