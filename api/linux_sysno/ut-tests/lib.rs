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
