// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn test_name() {
    assert_eq!(Sysno::write.name(), "write");
    assert_eq!(Sysno::fsopen.name(), "fsopen");
}

#[cfg(target_arch = "x86_64")]
#[def_test]
fn test_syscallno() {
    assert_eq!(Sysno::from(2), Sysno::open);
    assert_eq!(Sysno::new(2), Some(Sysno::open));
    assert_eq!(Sysno::new(-1i32 as usize), None);
    assert_eq!(Sysno::new(1024), None);
}

#[def_test]
fn test_first() {
    #[cfg(target_arch = "x86_64")]
    assert_eq!(Sysno::first(), Sysno::read);

    #[cfg(target_arch = "x86")]
    assert_eq!(Sysno::first(), Sysno::restart_syscall);
}

#[def_test]
fn test_syscall_len() {
    assert!(Sysno::table_size() > 300);
    assert!(Sysno::table_size() < 1000);
}

#[def_test]
fn test_last_is_the_numeric_maximum() {
    let start = Sysno::first().id() as usize;
    let end = start + Sysno::table_size();
    let mut maximum = start;

    for sysno in Sysno::ALL {
        let id = sysno.id() as usize;
        assert!(id >= start, "{sysno:?} is below the first syscall");
        assert!(id < end, "{sysno:?} is outside the table span");
        maximum = maximum.max(id);
    }

    assert_eq!(Sysno::last().id() as usize, maximum);
    assert_eq!(Sysno::max_id(), maximum);
    assert_eq!(Sysno::new(maximum), Some(Sysno::last()));
    assert_eq!(Sysno::new(maximum + 1), None);
}
