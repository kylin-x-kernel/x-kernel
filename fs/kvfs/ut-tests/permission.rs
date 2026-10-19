// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use kcred::Cred;
use ktime_types::SystemTime;
use unittest::def_test;

use super::{Permission, check_dac_permission};
use crate::{DeviceId, Metadata, NodePermission, NodeType, Umode};

fn creds(uid: u32, gid: u32, groups: &[u32]) -> Cred {
    let mut cred = Cred::new(uid, gid);
    cred.set_supplementary_groups(Vec::from(groups));
    cred
}

fn meta(uid: u32, gid: u32, mode: u16) -> Metadata {
    Metadata {
        device: 0,
        inode: 1,
        nlink: 1,
        mode: Umode::new(
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(mode),
        ),
        uid,
        gid,
        size: 0,
        block_size: 512,
        blocks: 1,
        rdev: DeviceId::default(),
        atime: SystemTime::UNIX_EPOCH,
        mtime: SystemTime::UNIX_EPOCH,
        ctime: SystemTime::UNIX_EPOCH,
    }
}

#[def_test]
fn test_owner_read_allowed() {
    let m = meta(1000, 1000, 0o400);
    assert!(check_dac_permission(&m, Permission::MAY_READ, &creds(1000, 2000, &[])).is_ok());
}

#[def_test]
fn test_owner_read_denied_when_bit_clear() {
    let m = meta(1000, 1000, 0o000);
    assert!(check_dac_permission(&m, Permission::MAY_READ, &creds(1000, 2000, &[])).is_err());
}

#[def_test]
fn test_owner_write_allowed() {
    let m = meta(1000, 1000, 0o200);
    assert!(check_dac_permission(&m, Permission::MAY_WRITE, &creds(1000, 2000, &[])).is_ok());
}

#[def_test]
fn test_owner_exec_allowed() {
    let m = meta(1000, 1000, 0o100);
    assert!(check_dac_permission(&m, Permission::MAY_EXEC, &creds(1000, 2000, &[])).is_ok());
}

#[def_test]
fn test_group_read_allowed_via_gid() {
    let m = meta(0, 2000, 0o040);
    assert!(check_dac_permission(&m, Permission::MAY_READ, &creds(1000, 2000, &[])).is_ok());
}

#[def_test]
fn test_group_read_allowed_via_supplementary() {
    let m = meta(0, 3000, 0o040);
    assert!(check_dac_permission(&m, Permission::MAY_READ, &creds(1000, 2000, &[3000])).is_ok());
}

#[def_test]
fn test_group_read_denied_when_not_member() {
    let m = meta(0, 3000, 0o040);
    assert!(check_dac_permission(&m, Permission::MAY_READ, &creds(1000, 2000, &[])).is_err());
}

#[def_test]
fn test_other_read_allowed() {
    let m = meta(0, 0, 0o004);
    assert!(check_dac_permission(&m, Permission::MAY_READ, &creds(1000, 2000, &[])).is_ok());
}

#[def_test]
fn test_other_write_denied_when_bit_clear() {
    let m = meta(0, 0, 0o004);
    assert!(check_dac_permission(&m, Permission::MAY_WRITE, &creds(1000, 2000, &[])).is_err());
}

#[def_test]
fn test_root_read_bypasses_zero_mode() {
    let m = meta(1000, 1000, 0o000);
    assert!(check_dac_permission(&m, Permission::MAY_READ, &creds(0, 0, &[])).is_ok());
}

#[def_test]
fn test_root_write_bypasses_zero_mode() {
    let m = meta(1000, 1000, 0o000);
    assert!(check_dac_permission(&m, Permission::MAY_WRITE, &creds(0, 0, &[])).is_ok());
}

#[def_test]
fn test_root_exec_denied_when_no_exec_bit() {
    let m = meta(1000, 1000, 0o666);
    assert!(check_dac_permission(&m, Permission::MAY_EXEC, &creds(0, 0, &[])).is_err());
}

#[def_test]
fn test_root_dir_search_bypasses_missing_exec_bit() {
    let mut m = meta(1000, 1000, 0o000);
    m.mode = m.mode.with_node_type(NodeType::Directory);
    assert!(check_dac_permission(&m, Permission::MAY_EXEC, &creds(0, 0, &[])).is_ok());
}

#[def_test]
fn test_root_exec_allowed_when_any_exec_bit_set() {
    let m = meta(1000, 1000, 0o711);
    assert!(check_dac_permission(&m, Permission::MAY_EXEC, &creds(0, 0, &[])).is_ok());
}

#[def_test]
fn test_owner_rw_allowed() {
    let m = meta(1000, 1000, 0o600);
    assert!(
        check_dac_permission(
            &m,
            Permission::MAY_READ | Permission::MAY_WRITE,
            &creds(1000, 2000, &[])
        )
        .is_ok()
    );
}

#[def_test]
fn test_empty_request_always_allowed() {
    let m = meta(1000, 1000, 0o000);
    assert!(check_dac_permission(&m, Permission::empty(), &creds(1000, 2000, &[])).is_ok());
}

#[def_test]
fn test_group_write_allowed_via_gid() {
    let m = meta(0, 2000, 0o020);
    assert!(check_dac_permission(&m, Permission::MAY_WRITE, &creds(1000, 2000, &[])).is_ok());
}

#[def_test]
fn test_group_exec_allowed_via_supplementary() {
    let m = meta(0, 3000, 0o010);
    assert!(check_dac_permission(&m, Permission::MAY_EXEC, &creds(1000, 2000, &[3000])).is_ok());
}

#[def_test]
fn test_other_exec_allowed() {
    let m = meta(0, 0, 0o001);
    assert!(check_dac_permission(&m, Permission::MAY_EXEC, &creds(1000, 2000, &[])).is_ok());
}

#[def_test]
fn test_other_exec_denied_when_bit_clear() {
    let m = meta(0, 0, 0o006);
    assert!(check_dac_permission(&m, Permission::MAY_EXEC, &creds(1000, 2000, &[])).is_err());
}

#[def_test]
fn test_dir_search_allowed_for_owner() {
    let mut m = meta(1000, 1000, 0o100);
    m.mode = m.mode.with_node_type(NodeType::Directory);
    assert!(check_dac_permission(&m, Permission::MAY_EXEC, &creds(1000, 2000, &[])).is_ok());
}

#[def_test]
fn test_dir_search_denied_when_exec_bit_clear() {
    let mut m = meta(1000, 1000, 0o600);
    m.mode = m.mode.with_node_type(NodeType::Directory);
    assert!(check_dac_permission(&m, Permission::MAY_EXEC, &creds(1000, 2000, &[])).is_err());
}

#[def_test]
fn test_owner_bits_used_when_uid_matches_even_if_group_denies() {
    let m = meta(1000, 1000, 0o040);
    assert!(check_dac_permission(&m, Permission::MAY_READ, &creds(1000, 1000, &[])).is_err());
}
