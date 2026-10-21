// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, vec, vec::Vec};

use unittest::def_test;

use super::*;

#[derive(Default)]
struct NameCollector(Vec<(Ext4XattrNamespace, Vec<u8>)>);

impl Ext4XattrNameSink for NameCollector {
    fn emit(&mut self, name: Ext4XattrNameRef<'_>) -> Ext4Result<()> {
        self.0
            .push((name.namespace(), Vec::from(name.name_bytes())));
        Ok(())
    }
}

fn put_xattr_entry(
    bytes: &mut [u8],
    offset: usize,
    name_index: u8,
    name: &[u8],
    value_offs: u16,
    value_size: u32,
) -> usize {
    bytes[offset] = u8::try_from(name.len()).unwrap();
    bytes[offset + 1] = name_index;
    bytes[offset + 2..offset + 4].copy_from_slice(&value_offs.to_le_bytes());
    bytes[offset + 4..offset + 8].copy_from_slice(&0u32.to_le_bytes());
    bytes[offset + 8..offset + 12].copy_from_slice(&value_size.to_le_bytes());
    bytes[offset + 12..offset + 16].copy_from_slice(&0u32.to_le_bytes());
    let name_start = offset + disk_xattr::XATTR_ENTRY_HEADER_SIZE;
    let name_end = name_start + name.len();
    bytes[name_start..name_end].copy_from_slice(name);
    offset + disk_xattr::entry_len(name.len()).unwrap()
}

#[def_test]
fn external_xattr_encode_matches_linux_hashes_for_multiple_entries() {
    // Non-repeating values of lengths 301 and 7 exercise both multi-word
    // folding and zero-padded final words.
    let user_value: Vec<u8> = (0u8..=250).cycle().take(301).collect();
    let security_value = vec![0x10, 0x21, 0x32, 0x43, 0x54, 0x65, 0x76];
    let xattrs = vec![
        Ext4Xattr {
            namespace: Ext4XattrNamespace::Security,
            name: Vec::from(&b"token"[..]),
            value: security_value,
        },
        Ext4Xattr {
            namespace: Ext4XattrNamespace::User,
            name: Vec::from(&b"key"[..]),
            value: user_value,
        },
    ];
    let mut external = vec![0; 4096];

    encode_external_xattr_block(&xattrs, 42, 0, false, &mut external)
        .expect("encode external xattr block");

    // These values were cross-checked with e2fsprogs
    // `ext2fs_ext_attr_hash_entry()` and `ext2fs_ext_attr_block_rehash()`.
    const USER_HASH: u32 = 0xffff_9614;
    const SECURITY_HASH: u32 = 0x2610_06a8;
    const BLOCK_HASH: u32 = 0xb004_f957;
    let user_entry_offset = disk_xattr::XATTR_HEADER_SIZE;
    let security_entry_offset = user_entry_offset + disk_xattr::entry_len(3).unwrap();
    assert_eq!(codec::le_u32(&external, 0x0c).unwrap(), BLOCK_HASH);
    assert_eq!(
        codec::le_u32(&external, user_entry_offset + 0x0c).unwrap(),
        USER_HASH
    );
    assert_eq!(
        codec::le_u32(&external, security_entry_offset + 0x0c).unwrap(),
        SECURITY_HASH
    );

    let mut inline = vec![0; 512];
    encode_inline_xattrs(&xattrs, &mut inline).expect("encode inline xattrs");
    let inline_user_offset = disk_xattr::XATTR_IBODY_HEADER_SIZE;
    let inline_security_offset = inline_user_offset + disk_xattr::entry_len(3).unwrap();
    assert_eq!(
        codec::le_u32(&inline, inline_user_offset + 0x0c).unwrap(),
        0
    );
    assert_eq!(
        codec::le_u32(&inline, inline_security_offset + 0x0c).unwrap(),
        0
    );
}

#[def_test]
fn external_xattr_zero_entry_hash_disables_block_hash() {
    let mut block_hasher = ExternalXattrBlockHasher::new();
    block_hasher.update(0x1122_3344);
    block_hasher.update(0);
    block_hasher.update(0x5566_7788);

    assert_eq!(block_hasher.finish(), 0);
}

#[def_test]
fn external_xattr_header_allows_nonzero_reserved_words() {
    let mut bytes = vec![0; disk_xattr::XATTR_HEADER_SIZE];
    put_u32(&mut bytes, 0x00, disk_xattr::XATTR_MAGIC).unwrap();
    put_u32(&mut bytes, 0x04, 1).unwrap();
    put_u32(&mut bytes, 0x08, 1).unwrap();
    put_u32(&mut bytes, 0x14, 0x1122_3344).unwrap();
    put_u32(&mut bytes, 0x18, 0x5566_7788).unwrap();

    let header =
        disk_xattr::XattrBlockHeader::decode(&bytes).expect("decode external xattr header");

    validate_xattr_block_header(header)
        .expect("reserved words are not an ext4 xattr validity requirement");
}

#[def_test]
fn xattr_value_ranges_allow_shared_value() {
    let mut ranges = [(32, 48), (32, 48), (48, 52)];

    validate_non_overlapping_value_ranges(&mut ranges).unwrap();
}

#[def_test]
fn xattr_value_ranges_reject_partial_overlap() {
    let mut ranges = [(32, 48), (40, 52)];

    assert_eq!(
        validate_non_overlapping_value_ranges(&mut ranges),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr))
    );
}

#[def_test]
fn external_xattr_entries_must_be_sorted() {
    let mut bytes = vec![0; 64];
    let next = put_xattr_entry(&mut bytes, 0, 1, b"z", 0, 0);
    put_xattr_entry(&mut bytes, next, 1, b"a", 0, 0);

    assert_eq!(
        decode_xattr_entries(&bytes, 0, XattrEntryOrder::RequireSorted, &mut Vec::new()),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr))
    );
    decode_xattr_entries(&bytes, 0, XattrEntryOrder::Unchecked, &mut Vec::new()).unwrap();
}

#[def_test]
fn xattr_entry_names_reject_embedded_nul() {
    let mut bytes = vec![0; 64];
    put_xattr_entry(&mut bytes, 0, 1, b"a\0b", 0, 0);

    assert_eq!(
        decode_xattr_entries(&bytes, 0, XattrEntryOrder::Unchecked, &mut Vec::new()),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidXattr))
    );
}

#[def_test]
fn inline_xattr_set_replace_and_remove_round_trips() {
    let mut bytes = vec![0; 128];
    let mut xattrs = Vec::new();

    set_xattr_value(&mut xattrs, Ext4XattrNamespace::User, b"beta", b"two")
        .expect("insert first xattr");
    set_xattr_value(&mut xattrs, Ext4XattrNamespace::Security, b"alpha", b"one")
        .expect("insert second xattr");
    set_xattr_value(&mut xattrs, Ext4XattrNamespace::User, b"beta", b"replaced")
        .expect("replace first xattr");
    encode_inline_xattrs(&xattrs, &mut bytes).expect("encode inline xattrs");

    let decoded = read_inline_xattrs_from_bytes(&bytes).expect("decode inline xattrs");
    assert_eq!(decoded.len(), 2);
    assert!(decoded.iter().any(|xattr| {
        xattr.namespace() == Ext4XattrNamespace::Security
            && xattr.name_bytes() == b"alpha"
            && xattr.value() == b"one"
    }));
    assert!(decoded.iter().any(|xattr| {
        xattr.namespace() == Ext4XattrNamespace::User
            && xattr.name_bytes() == b"beta"
            && xattr.value() == b"replaced"
    }));

    let mut decoded = decoded;
    remove_xattr_value(&mut decoded, Ext4XattrNamespace::User, b"beta")
        .expect("remove user xattr");
    encode_inline_xattrs(&decoded, &mut bytes).expect("encode after remove");
    let decoded = read_inline_xattrs_from_bytes(&bytes).expect("decode after remove");
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].namespace(), Ext4XattrNamespace::Security);
    assert_eq!(decoded[0].name_bytes(), b"alpha");
}

#[def_test]
fn inline_xattr_listing_emits_only_borrowed_names() {
    let mut bytes = vec![0; 128];
    let xattrs = vec![
        Ext4Xattr {
            namespace: Ext4XattrNamespace::User,
            name: Vec::from(&b"alpha"[..]),
            value: Vec::from(&b"large-value-is-not-returned"[..]),
        },
        Ext4Xattr {
            namespace: Ext4XattrNamespace::Security,
            name: Vec::from(&b"beta"[..]),
            value: Vec::new(),
        },
    ];
    encode_inline_xattrs(&xattrs, &mut bytes).expect("encode inline xattrs");
    let mut names = NameCollector::default();

    list_inline_xattr_names_from_bytes(&bytes, &mut names).expect("list inline xattrs");

    assert_eq!(
        names.0,
        vec![
            (Ext4XattrNamespace::User, Vec::from(&b"alpha"[..])),
            (Ext4XattrNamespace::Security, Vec::from(&b"beta"[..])),
        ]
    );
}

#[def_test]
fn xattr_set_mode_enforces_create_and_replace_atomically() {
    let mut xattrs = Vec::new();
    set_xattr_value_with_mode(
        &mut xattrs,
        Ext4XattrNamespace::User,
        b"key",
        b"initial",
        Ext4XattrSetMode::Create,
    )
    .expect("create missing xattr");
    assert_eq!(
        set_xattr_value_with_mode(
            &mut xattrs,
            Ext4XattrNamespace::User,
            b"key",
            b"duplicate",
            Ext4XattrSetMode::Create,
        ),
        Err(Ext4Error::AlreadyExists)
    );
    assert_eq!(xattrs[0].value(), b"initial");

    set_xattr_value_with_mode(
        &mut xattrs,
        Ext4XattrNamespace::User,
        b"key",
        b"replacement",
        Ext4XattrSetMode::Replace,
    )
    .expect("replace existing xattr");
    assert_eq!(xattrs[0].value(), b"replacement");
    assert_eq!(
        set_xattr_value_with_mode(
            &mut xattrs,
            Ext4XattrNamespace::User,
            b"missing",
            b"value",
            Ext4XattrSetMode::Replace,
        ),
        Err(Ext4Error::NotFound)
    );

    assert_eq!(
        set_xattr_value_with_mode(
            &mut xattrs,
            Ext4XattrNamespace::User,
            b"key",
            b"value",
            Ext4XattrSetMode::CreateAndReplace,
        ),
        Err(Ext4Error::AlreadyExists)
    );
    assert_eq!(
        set_xattr_value_with_mode(
            &mut xattrs,
            Ext4XattrNamespace::User,
            b"missing",
            b"value",
            Ext4XattrSetMode::CreateAndReplace,
        ),
        Err(Ext4Error::NotFound)
    );
}

#[def_test]
fn replacing_an_identical_xattr_value_is_unchanged() {
    let mut xattrs = vec![Ext4Xattr {
        namespace: Ext4XattrNamespace::User,
        name: Vec::from(&b"key"[..]),
        value: Vec::from(&b"value"[..]),
    }];

    assert_eq!(
        set_xattr_value_with_mode(
            &mut xattrs,
            Ext4XattrNamespace::User,
            b"key",
            b"value",
            Ext4XattrSetMode::Replace,
        ),
        Ok(XattrMutation::Unchanged)
    );
    assert_eq!(xattrs.len(), 1);
    assert_eq!(xattrs[0].value(), b"value");
}

#[def_test]
fn inline_xattr_encode_orders_entries_for_linux_lookup() {
    let mut bytes = vec![0; 128];
    let xattrs = vec![
        Ext4Xattr {
            namespace: Ext4XattrNamespace::Security,
            name: Vec::from(&b"b"[..]),
            value: Vec::from(&b"2"[..]),
        },
        Ext4Xattr {
            namespace: Ext4XattrNamespace::User,
            name: Vec::from(&b"zz"[..]),
            value: Vec::from(&b"3"[..]),
        },
        Ext4Xattr {
            namespace: Ext4XattrNamespace::User,
            name: Vec::from(&b"a"[..]),
            value: Vec::from(&b"1"[..]),
        },
    ];

    encode_inline_xattrs(&xattrs, &mut bytes).expect("encode inline xattrs");
    let mut parsed = Vec::new();
    decode_xattr_entries(
        &bytes,
        disk_xattr::XATTR_IBODY_HEADER_SIZE,
        XattrEntryOrder::RequireSorted,
        &mut parsed,
    )
    .expect("encoded entries are sorted");
    assert_eq!(
        parsed
            .iter()
            .map(|entry| (entry.namespace, entry.name))
            .collect::<Vec<_>>(),
        vec![
            (Ext4XattrNamespace::User, &b"a"[..]),
            (Ext4XattrNamespace::User, &b"zz"[..]),
            (Ext4XattrNamespace::Security, &b"b"[..]),
        ]
    );
}

#[def_test]
fn inline_xattr_encode_reports_external_block_requirement() {
    let xattrs = vec![Ext4Xattr {
        namespace: Ext4XattrNamespace::User,
        name: Vec::from(&b"large"[..]),
        value: Vec::from(&b"value-too-large-for-inline-body"[..]),
    }];
    let mut bytes = vec![0; 32];
    let error = encode_inline_xattrs(&xattrs, &mut bytes)
        .expect_err("large value requires external xattr block");

    assert_eq!(
        error,
        Ext4Error::Unsupported(UnsupportedKind::ExternalXattrBlock)
    );
}

fn numbered_xattrs(count: usize) -> Vec<Ext4Xattr> {
    (0..count)
        .map(|index| Ext4Xattr {
            namespace: Ext4XattrNamespace::User,
            name: format!("{index}").into_bytes(),
            value: Vec::from(&b"aa"[..]),
        })
        .collect()
}

#[def_test]
fn xattr_set_uses_inode_and_external_block_without_rebalancing() {
    let inline = numbered_xattrs(10);
    let inline_capacity = inline_xattr_encoded_len(&inline).unwrap();

    let PlannedXattrMutation::Changed(plan) = plan_xattr_set_storage(
        &inline,
        &[],
        Ext4XattrNamespace::Security,
        b"new",
        b"value",
        Ext4XattrSetMode::Create,
        0,
        inline_capacity,
        4096,
        XattrStorageLayout::Inline,
        1,
    )
    .expect("plan mixed xattr storage") else {
        panic!("new xattr must change storage");
    };

    assert_eq!(plan.extra_isize, 0);
    assert_eq!(plan.storage.inline, inline);
    assert_eq!(plan.storage.external.len(), 1);
    assert_eq!(
        plan.external_block_action,
        ExternalXattrBlockAction::Allocate
    );
}

#[def_test]
fn xattr_set_keeps_current_extra_isize_when_value_moves_external() {
    let inline = vec![Ext4Xattr {
        namespace: Ext4XattrNamespace::User,
        name: Vec::from(&b"key"[..]),
        value: Vec::from(&b"small"[..]),
    }];
    let PlannedXattrMutation::Changed(plan) = plan_xattr_set_storage(
        &inline,
        &[],
        Ext4XattrNamespace::User,
        b"key",
        &[0x5a; 88],
        Ext4XattrSetMode::Replace,
        16,
        inline_xattr_encoded_len(&inline).unwrap(),
        4096,
        XattrStorageLayout::Inline,
        1,
    )
    .expect("replacement can move to an external block") else {
        panic!("different value must change storage");
    };

    assert_eq!(plan.extra_isize, 16);
    assert!(plan.storage.inline.is_empty());
    assert_eq!(plan.storage.external.len(), 1);
    assert_eq!(
        plan.external_block_action,
        ExternalXattrBlockAction::Allocate
    );
}

#[def_test]
fn xattr_mutation_reuses_unchanged_shared_external_block_when_disk_is_full() {
    let old_external = vec![Ext4Xattr {
        namespace: Ext4XattrNamespace::Security,
        name: Vec::from(&b"external"[..]),
        value: vec![0x45; 200],
    }];
    let updated_inline = Ext4Xattr {
        namespace: Ext4XattrNamespace::User,
        name: Vec::from(&b"inline"[..]),
        value: Vec::from(&b"updated"[..]),
    };
    let inline = vec![Ext4Xattr {
        namespace: Ext4XattrNamespace::User,
        name: Vec::from(&b"inline"[..]),
        value: Vec::from(&b"old"[..]),
    }];
    let PlannedXattrMutation::Changed(plan) = plan_xattr_set_storage(
        &inline,
        &old_external,
        Ext4XattrNamespace::User,
        b"inline",
        b"updated",
        Ext4XattrSetMode::Replace,
        0,
        256,
        4096,
        XattrStorageLayout::External { shared: true },
        0,
    )
    .expect("unchanged shared external block does not need allocation") else {
        panic!("different value must change storage");
    };

    assert_eq!(plan.storage.inline, vec![updated_inline]);
    assert_eq!(plan.storage.external, old_external);
    assert_eq!(plan.external_block_action, ExternalXattrBlockAction::Reuse);
}

#[def_test]
fn xattr_remove_preserves_unrelated_storage_regions() {
    let removed = Ext4Xattr {
        namespace: Ext4XattrNamespace::User,
        name: Vec::from(&b"removed"[..]),
        value: Vec::from(&b"value"[..]),
    };
    let retained_inline = Ext4Xattr {
        namespace: Ext4XattrNamespace::User,
        name: Vec::from(&b"inline"[..]),
        value: Vec::from(&b"value"[..]),
    };
    let retained_external = Ext4Xattr {
        namespace: Ext4XattrNamespace::Security,
        name: Vec::from(&b"external"[..]),
        value: vec![0x45; 200],
    };

    let plan = plan_xattr_remove_storage(
        &[removed, retained_inline.clone()],
        core::slice::from_ref(&retained_external),
        Ext4XattrNamespace::User,
        b"removed",
        16,
        256,
        4096,
        XattrStorageLayout::External { shared: true },
        0,
    )
    .expect("removing an inline entry leaves the shared block unchanged");

    assert_eq!(plan.extra_isize, 16);
    assert_eq!(plan.storage.inline, vec![retained_inline]);
    assert_eq!(plan.storage.external, vec![retained_external]);
    assert_eq!(plan.external_block_action, ExternalXattrBlockAction::Reuse);
}

#[def_test]
fn extra_isize_layout_falls_back_to_minimum_then_current_size() {
    let candidates = extra_isize_layout_candidates(256, 0, 80, 16).unwrap();
    assert_eq!(candidates[0].unwrap().extra_isize, 80);
    assert_eq!(candidates[0].unwrap().inline_capacity, 48);
    assert_eq!(candidates[1].unwrap().extra_isize, 16);
    assert_eq!(candidates[1].unwrap().inline_capacity, 112);
    assert_eq!(candidates[2].unwrap().extra_isize, 0);
    assert_eq!(candidates[2].unwrap().inline_capacity, 128);
}

#[def_test]
fn extra_isize_expansion_does_not_rebalance_existing_external_xattrs() {
    let inline = numbered_xattrs(4);
    let external = vec![Ext4Xattr {
        namespace: Ext4XattrNamespace::User,
        name: Vec::from(&b"external"[..]),
        value: Vec::from(&b"value"[..]),
    }];

    let plan = plan_xattr_expansion_storage(&inline, &external, 176, 4096)
        .unwrap()
        .expect("inline entries already fit expanded inode");

    assert_eq!(plan.inline, inline);
    assert_eq!(plan.external, external);
}

#[def_test]
fn extra_isize_expansion_moves_the_smallest_entry_that_frees_enough_space() {
    let small = Ext4Xattr {
        namespace: Ext4XattrNamespace::User,
        name: Vec::from(&b"small"[..]),
        value: vec![0x11; 4],
    };
    let sufficient = Ext4Xattr {
        namespace: Ext4XattrNamespace::User,
        name: Vec::from(&b"sufficient"[..]),
        value: vec![0x22; 24],
    };
    let large = Ext4Xattr {
        namespace: Ext4XattrNamespace::User,
        name: Vec::from(&b"large"[..]),
        value: vec![0x33; 80],
    };
    let inline = vec![small.clone(), sufficient.clone(), large.clone()];
    let capacity = inline_xattr_encoded_len(&inline).unwrap()
        - xattr_encoded_entry_len(&sufficient).unwrap();

    let plan = plan_xattr_expansion_storage(&inline, &[], capacity, 4096)
        .unwrap()
        .expect("one inode-body entry can move externally");

    assert_eq!(plan.external, vec![sufficient]);
    assert!(plan.inline.contains(&small));
    assert!(plan.inline.contains(&large));
}

#[def_test]
fn extra_isize_expansion_uses_linux_last_entry_fallback() {
    let entries = [b"aaa", b"bbb", b"ccc"].map(|name| Ext4Xattr {
        namespace: Ext4XattrNamespace::User,
        name: Vec::from(&name[..]),
        value: vec![0x66; 8],
    });
    let entry_size = xattr_encoded_entry_len(&entries[0]).unwrap();
    let capacity = inline_xattr_encoded_len(&entries).unwrap() - 2 * entry_size;

    let plan = plan_xattr_expansion_storage(&entries, &[], capacity, 4096)
        .unwrap()
        .expect("moving two entries makes enough inode-body space");

    assert_eq!(
        plan.external
            .iter()
            .map(|xattr| xattr.name.as_slice())
            .collect::<Vec<_>>(),
        vec![&b"aaa"[..], &b"ccc"[..]]
    );
}

#[def_test]
fn extra_isize_expansion_keeps_system_data_in_the_inode_body() {
    let system_data = Ext4Xattr {
        namespace: Ext4XattrNamespace::System,
        name: Vec::from(&b"data"[..]),
        value: vec![0x44; 24],
    };
    let user = Ext4Xattr {
        namespace: Ext4XattrNamespace::User,
        name: Vec::from(&b"movable"[..]),
        value: vec![0x55; 24],
    };
    let capacity = inline_xattr_encoded_len(core::slice::from_ref(&system_data)).unwrap();

    let plan =
        plan_xattr_expansion_storage(&[system_data.clone(), user.clone()], &[], capacity, 4096)
            .unwrap()
            .expect("the movable entry can leave system.data inline");

    assert_eq!(plan.inline, vec![system_data]);
    assert_eq!(plan.external, vec![user]);
}

#[def_test]
fn storage_plan_rejects_internal_extra_isize_shrink() {
    assert_eq!(
        validate_nonshrinking_extra_isize(32, 16),
        Err(Ext4Error::InvalidInodeState)
    );
    validate_nonshrinking_extra_isize(32, 32).unwrap();
    validate_nonshrinking_extra_isize(32, 64).unwrap();
}

#[def_test]
fn xattr_mutation_credits_follow_storage_transition() {
    assert_eq!(
        xattr_mutation_credit_count(
            XattrStorageLayout::Empty,
            XattrStorageLayout::Inline,
            ExternalXattrBlockAction::None,
        ),
        XATTR_INODE_UPDATE_CREDITS
    );
    assert_eq!(
        xattr_mutation_credit_count(
            XattrStorageLayout::Inline,
            XattrStorageLayout::External { shared: false },
            ExternalXattrBlockAction::Allocate,
        ),
        XATTR_INODE_UPDATE_CREDITS + XATTR_EXTERNAL_ALLOC_CREDITS
    );
    assert_eq!(
        xattr_mutation_credit_count(
            XattrStorageLayout::External { shared: false },
            XattrStorageLayout::External { shared: false },
            ExternalXattrBlockAction::Rewrite,
        ),
        XATTR_INODE_UPDATE_CREDITS + XATTR_EXTERNAL_REWRITE_CREDITS
    );
    assert_eq!(
        xattr_mutation_credit_count(
            XattrStorageLayout::External { shared: false },
            XattrStorageLayout::Inline,
            ExternalXattrBlockAction::None,
        ),
        XATTR_INODE_UPDATE_CREDITS + XATTR_EXTERNAL_RELEASE_CREDITS
    );
    assert_eq!(
        xattr_mutation_credit_count(
            XattrStorageLayout::External { shared: true },
            XattrStorageLayout::External { shared: false },
            ExternalXattrBlockAction::Allocate,
        ),
        XATTR_INODE_UPDATE_CREDITS
            + XATTR_EXTERNAL_ALLOC_CREDITS
            + XATTR_EXTERNAL_SHARED_REFCOUNT_CREDITS
    );
    assert_eq!(
        xattr_mutation_credit_count(
            XattrStorageLayout::External { shared: true },
            XattrStorageLayout::External { shared: false },
            ExternalXattrBlockAction::Reuse,
        ),
        XATTR_INODE_UPDATE_CREDITS
    );
}

#[def_test]
fn settable_xattr_validation_rejects_unsupported_namespace() {
    validate_settable_xattr(Ext4XattrNamespace::PosixAclAccess, b"").unwrap();
    validate_settable_xattr(Ext4XattrNamespace::PosixAclDefault, b"").unwrap();
    assert_eq!(
        validate_settable_xattr(Ext4XattrNamespace::PosixAclAccess, b"name"),
        Err(Ext4Error::InvalidName)
    );
    assert_eq!(
        validate_settable_xattr(Ext4XattrNamespace::System, b"posix_acl_access"),
        Err(Ext4Error::InvalidName)
    );
    validate_settable_xattr(Ext4XattrNamespace::Trusted, b"name").unwrap();
}
