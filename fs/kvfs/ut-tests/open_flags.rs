// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn exclusive_without_create_does_not_set_create_intent() {
    let flags = OpenHow::from_legacy(O_EXCL, NodePermission::empty())
        .into_open_params()
        .unwrap();

    assert!(!flags.will_create());
    assert!(!flags.is_exclusive_create());
}

#[def_test]
fn exclusive_create_sets_intent_and_disables_final_symlink_following() {
    let flags = OpenHow::from_legacy(O_CREAT | O_EXCL, NodePermission::empty())
        .into_open_params()
        .unwrap();

    assert!(flags.will_create());
    assert!(flags.is_exclusive_create());
    assert!(!flags.lookup_flags().follows_final());
    assert!(flags.flags.contains(OpenFlags::NO_FOLLOW));
}

#[def_test]
fn openat2_rejects_unknown_open_flag_bits() {
    // Bit 63 is outside every `O_*` value this kernel accepts.
    assert!(OpenHow::from_openat2(1 << 63, 0, 0).is_err());
}

#[def_test]
fn openat2_rejects_non_zero_mode_without_create() {
    assert!(OpenHow::from_openat2(u64::from(O_RDONLY), 0o644, 0).is_err());
    assert!(OpenHow::from_openat2(u64::from(O_CREAT | O_RDWR), 0o644, 0).is_ok());
}

#[def_test]
fn openat2_rejects_unknown_resolve_bits() {
    // Bit 63 is outside VALID_RESOLVE_FLAGS (`uapi/linux/openat2.h`).
    assert!(OpenHow::from_openat2(u64::from(O_RDONLY), 0, 1 << 63).is_err());
}

#[def_test]
fn openat2_rejects_beneath_with_in_root() {
    let both = ResolveFlags::BENEATH.bits() | ResolveFlags::IN_ROOT.bits();
    assert!(OpenHow::from_openat2(u64::from(O_RDONLY), 0, both).is_err());
}

#[def_test]
fn openat2_translates_resolve_bits_to_lookup_constraints() {
    let params = OpenHow::from_openat2(
        u64::from(O_RDONLY),
        0,
        ResolveFlags::NO_XDEV.bits()
            | ResolveFlags::NO_MAGICLINKS.bits()
            | ResolveFlags::NO_SYMLINKS.bits()
            | ResolveFlags::BENEATH.bits(),
    )
    .unwrap()
    .into_open_params()
    .unwrap();
    let lookup = params.lookup_flags();

    assert!(lookup.contains(LookupFlags::NO_XDEV));
    assert!(lookup.contains(LookupFlags::NO_MAGIC_LINKS));
    assert!(lookup.contains(LookupFlags::NO_SYMLINKS));
    assert!(lookup.contains(LookupFlags::BENEATH));
}

#[def_test]
fn legacy_open_never_requests_resolve_constraints() {
    let lookup = OpenHow::from_legacy(O_RDONLY, NodePermission::empty())
        .into_open_params()
        .unwrap()
        .lookup_flags();

    assert!(!lookup.contains(LookupFlags::NO_XDEV));
    assert!(!lookup.contains(LookupFlags::NO_SYMLINKS));
    assert!(!lookup.contains(LookupFlags::BENEATH));
}

#[def_test]
fn openat2_rejects_mode_bits_outside_permission_mask() {
    // 0o10000 is not a permission bit; truncating it used to create a
    // zero-permission file instead of reporting EINVAL.
    assert!(OpenHow::from_openat2(u64::from(O_CREAT | O_RDWR), 0o10000, 0).is_err());
    assert!(OpenHow::from_openat2(u64::from(O_CREAT | O_RDWR), 0o7777, 0).is_ok());
}

#[def_test]
fn openat2_accepts_and_records_regular_constraint() {
    let params = OpenHow::from_openat2(u64::from(O_RDONLY) | OPENAT2_REGULAR, 0, 0)
        .unwrap()
        .into_open_params()
        .unwrap();
    assert!(params.requires_regular());
}

#[def_test]
fn openat2_translates_in_root_and_cached() {
    let params = OpenHow::from_openat2(
        u64::from(O_RDONLY),
        0,
        ResolveFlags::IN_ROOT.bits() | ResolveFlags::CACHED.bits(),
    )
    .unwrap()
    .into_open_params()
    .unwrap();
    let lookup = params.lookup_flags();

    assert!(lookup.contains(LookupFlags::IN_ROOT));
    assert!(lookup.contains(LookupFlags::CACHED));
}

#[def_test]
fn openat2_rejects_regular_with_path_or_directory() {
    use linux_raw_sys::general::{O_DIRECTORY, O_PATH};

    assert!(OpenHow::from_openat2(u64::from(O_PATH) | OPENAT2_REGULAR, 0, 0).is_err());
    assert!(OpenHow::from_openat2(u64::from(O_DIRECTORY) | OPENAT2_REGULAR, 0, 0).is_err());
}

#[def_test]
fn openat2_cached_with_create_or_truncate_is_eagain() {
    let cached = ResolveFlags::CACHED.bits();
    assert_eq!(
        OpenHow::from_openat2(u64::from(O_CREAT | O_RDWR), 0, cached).unwrap_err(),
        VfsError::WouldBlock
    );
    assert_eq!(
        OpenHow::from_openat2(u64::from(O_TRUNC), 0, cached).unwrap_err(),
        VfsError::WouldBlock
    );
}

#[def_test]
fn openat2_contradictory_flags_are_einval_before_cached_eagain() {
    use linux_raw_sys::general::{O_CREAT, O_DIRECTORY, O_PATH, O_RDWR};

    let cached = ResolveFlags::CACHED.bits();
    // O_DIRECTORY|O_CREAT and O_PATH|O_CREAT are EINVAL in Linux
    // build_open_flags, which runs before the RESOLVE_CACHED rule.
    assert_eq!(
        OpenHow::from_openat2(u64::from(O_RDWR | O_CREAT | O_DIRECTORY), 0o644, cached)
            .unwrap_err(),
        VfsError::InvalidInput
    );
    assert_eq!(
        OpenHow::from_openat2(u64::from(O_PATH | O_CREAT), 0, cached).unwrap_err(),
        VfsError::InvalidInput
    );
}
