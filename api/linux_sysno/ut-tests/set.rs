// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;
use alloc::{format, vec, vec::Vec};

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn test_words() {
    assert_eq!(words::<u64>(42), 1);
    assert_eq!(words::<u64>(0), 0);
    assert_eq!(words::<u32>(42), 2);
    assert_eq!(words::<()>(42), 0);
}

#[def_test]
fn test_bits_per() {
    assert_eq!(bits_per::<()>(), 0);
    assert_eq!(bits_per::<u8>(), 8);
    assert_eq!(bits_per::<u32>(), 32);
    assert_eq!(bits_per::<u64>(), 64);
}

#[def_test]
fn test_default() {
    assert_eq!(SysnoSet::default(), SysnoSet::empty());
}

#[def_test]
fn test_const_new() {
    static SYSCALLS: SysnoSet = SysnoSet::new(&[Sysno::openat, Sysno::read, Sysno::close]);

    assert!(SYSCALLS.contains(Sysno::openat));
    assert!(SYSCALLS.contains(Sysno::read));
    assert!(SYSCALLS.contains(Sysno::close));
    assert!(!SYSCALLS.contains(Sysno::write));
}

#[def_test]
fn test_contains() {
    let set = SysnoSet::empty();
    assert!(!set.contains(Sysno::openat));
    assert!(!set.contains(Sysno::first()));
    assert!(!set.contains(Sysno::last()));

    let set = SysnoSet::all();
    assert!(set.contains(Sysno::openat));
    assert!(set.contains(Sysno::first()));
    assert!(set.contains(Sysno::last()));
}

#[def_test]
fn test_is_empty() {
    let mut set = SysnoSet::empty();
    assert!(set.is_empty());
    assert!(set.insert(Sysno::openat));
    assert!(!set.is_empty());
    assert!(set.remove(Sysno::openat));
    assert!(set.is_empty());
    assert!(set.insert(Sysno::last()));
    assert!(!set.is_empty());
}

#[def_test]
fn test_count() {
    let mut set = SysnoSet::empty();
    assert_eq!(set.count(), 0);
    assert!(set.insert(Sysno::openat));
    assert!(set.insert(Sysno::last()));
    assert_eq!(set.count(), 2);
}

#[def_test]
fn test_insert() {
    let mut set = SysnoSet::empty();
    assert!(set.insert(Sysno::openat));
    assert!(set.insert(Sysno::read));
    assert!(set.insert(Sysno::close));
    assert!(set.contains(Sysno::openat));
    assert!(set.contains(Sysno::read));
    assert!(set.contains(Sysno::close));
    assert_eq!(set.count(), 3);
}

#[def_test]
fn test_remove() {
    let mut set = SysnoSet::all();
    assert!(set.remove(Sysno::openat));
    assert!(!set.contains(Sysno::openat));
    assert!(set.contains(Sysno::close));
}

#[def_test]
fn test_from_iter() {
    let set = SysnoSet::from_iter(vec![Sysno::openat, Sysno::read, Sysno::close]);
    assert!(set.contains(Sysno::openat));
    assert!(set.contains(Sysno::read));
    assert!(set.contains(Sysno::close));
    assert_eq!(set.count(), 3);
}

#[def_test]
fn test_all() {
    let mut all = SysnoSet::all();
    assert_eq!(all.count(), Sysno::count());

    all.contains(Sysno::openat);
    all.contains(Sysno::first());
    all.contains(Sysno::last());

    all.clear();

    assert_eq!(all.count(), 0);
}

#[def_test]
fn test_union() {
    let a = SysnoSet::new(&[Sysno::read, Sysno::openat, Sysno::close]);
    let b = SysnoSet::new(&[Sysno::write, Sysno::openat, Sysno::close]);
    assert_eq!(
        a.union(&b),
        SysnoSet::new(&[Sysno::read, Sysno::write, Sysno::openat, Sysno::close])
    );
}

#[def_test]
fn test_bitorassign() {
    let mut a = SysnoSet::new(&[Sysno::read, Sysno::openat, Sysno::close]);
    let b = SysnoSet::new(&[Sysno::write, Sysno::openat, Sysno::close]);
    a |= &b;
    a |= b;
    a |= Sysno::openat;

    assert_eq!(
        a,
        SysnoSet::new(&[Sysno::read, Sysno::write, Sysno::close, Sysno::openat,])
    );
}

#[def_test]
fn test_bitor() {
    let a = SysnoSet::new(&[Sysno::read, Sysno::openat, Sysno::close]);
    let b = SysnoSet::new(&[Sysno::write, Sysno::openat, Sysno::close]);
    assert_eq!(
        a | b,
        SysnoSet::new(&[Sysno::read, Sysno::write, Sysno::openat, Sysno::close,])
    );
}

#[def_test]
fn test_intersection() {
    let a = SysnoSet::new(&[Sysno::read, Sysno::openat, Sysno::close]);
    let b = SysnoSet::new(&[Sysno::write, Sysno::openat, Sysno::close]);
    assert_eq!(
        a.intersection(&b),
        SysnoSet::new(&[Sysno::openat, Sysno::close])
    );
}

#[def_test]
fn test_difference() {
    let a = SysnoSet::new(&[Sysno::read, Sysno::openat, Sysno::close]);
    let b = SysnoSet::new(&[Sysno::write, Sysno::openat, Sysno::close]);
    assert_eq!(a.difference(&b), SysnoSet::new(&[Sysno::read]));
}

#[def_test]
fn test_symmetric_difference() {
    let a = SysnoSet::new(&[Sysno::read, Sysno::openat, Sysno::close]);
    let b = SysnoSet::new(&[Sysno::write, Sysno::openat, Sysno::close]);
    assert_eq!(
        a.symmetric_difference(&b),
        SysnoSet::new(&[Sysno::read, Sysno::write])
    );
}

#[def_test]
fn test_iter() {
    let syscalls = &[Sysno::read, Sysno::openat, Sysno::close];
    let set = SysnoSet::new(syscalls);

    assert_eq!(set.iter().collect::<Vec<_>>().len(), 3);
}

#[def_test]
fn test_iter_full() {
    assert_eq!(SysnoSet::all().iter().count(), Sysno::count());
}

#[def_test]
fn test_into_iter() {
    let syscalls = &[Sysno::read, Sysno::openat, Sysno::close];
    let set = SysnoSet::new(syscalls);

    assert_eq!(set.into_iter().count(), 3);
}

#[def_test]
fn test_debug() {
    let syscalls = &[Sysno::openat, Sysno::read];
    let set = SysnoSet::new(syscalls);
    // The order of the debug output is not guaranteed, so we can't do an exact match
    let result = format!("{:?}", set);
    assert_eq!(result.len(), "{read, openat}".len());
    assert!(result.starts_with('{'));
    assert!(result.ends_with('}'));
    assert!(result.contains(", "));
    assert!(result.contains("read"));
    assert!(result.contains("openat"));
}

#[def_test]
fn test_iter_empty() {
    assert_eq!(SysnoSet::empty().iter().collect::<Vec<_>>(), &[]);
}
