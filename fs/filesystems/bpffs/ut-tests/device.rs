// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cgroup-v2 device controller semantics.

use alloc::{sync::Arc, vec, vec::Vec};

use unittest::{assert, assert_eq, def_test};

use kbpf::vm::InitialRegs;
use kbpf::{ReadOnlyMemory, Vm};

use super::*;
use kcgroup::Cgroup;

fn root_cgroup() -> Arc<Cgroup> {
    kcgroup::CgroupNamespace::initial().root()
}

fn load_program(allow_minor_three: bool) -> Arc<BpfProgram> {
    // A runc `devicefilter`-shaped program: mask
    // (type | access), JNE32 -> deny, then major/minor checks. This one
    // allows exactly `write` access to char device 1:3 (or 1:9 when
    // `allow_minor_three` is false).
    let slot = |opc: u8, dst: u8, src: u8, off: i16, imm: i32| {
        let mut bytes = [0u8; 8];
        bytes[0] = opc;
        bytes[1] = (src << 4) | dst;
        bytes[2..4].copy_from_slice(&off.to_le_bytes());
        bytes[4..8].copy_from_slice(&imm.to_le_bytes());
        bytes
    };
    // Linux encodes access_type as (access << 16) | device type; the mask
    // selects char device (2) with write access (4 << 16).
    let access_mask: i32 = 0x0004_0002; // char | write
    let insns: Vec<[u8; 8]> = vec![
        slot(0x61, 3, 1, 0, 0),           // r3 = ctx->access_type
        slot(0xbc, 2, 3, 0, 0),           // mov32 r2, r3
        slot(0x54, 2, 0, 0, access_mask), // and32 r2, mask
        slot(0x56, 2, 0, 6, access_mask), // jne32 r2, mask -> deny
        slot(0x61, 2, 1, 4, 0),           // r2 = major
        slot(0x55, 2, 0, 3, 1),           // jne r2, 1 -> deny
        slot(0x61, 2, 1, 8, 0),           // r2 = minor
        slot(0x55, 2, 0, 1, if allow_minor_three { 3 } else { 9 }),
        slot(0xb4, 0, 0, 0, 1), // allow
        slot(0x95, 0, 0, 0, 0),
        slot(0xb4, 0, 0, 0, 0), // deny
        slot(0x95, 0, 0, 0, 0),
    ];
    Arc::new(BpfProgram::new(
        insns.concat().into_boxed_slice().into(),
        15, // BPF_PROG_TYPE_CGROUP_DEVICE (enum bpf_prog_type)
        Vec::new().into_boxed_slice().into(),
    ))
}

/// `(access, major, minor, dev_type)` — the parameter order of
/// `device::device_allowed`.
fn write_char_1_3() -> (u32, u32, u32, u32) {
    (
        device::DEVICE_ACC_WRITE,
        1,
        3,
        device::DEVICE_TYPE_CHAR,
    )
}

/// A single-attach replaces the cgroup's program, and the interpreted policy
/// decides device access for the cgroup, its parent (inheritance), and leaves
/// sibling subtrees untouched.
#[def_test]
fn attached_program_enforces_device_access() {
    let root = root_cgroup();
    let child = root.create_child("bpftest-enforce").unwrap();
    let program = load_program(true);

    // No attachments: everything allowed.
    let (access, major, minor, dev_type) = write_char_1_3();
    assert!(device::device_allowed(&child, access, major, minor, dev_type));

    device::attach(&child, &program, device::ATTACH_NONE).unwrap();

    let verdict = device::device_program_verdict(
        &program.insns_shared(),
        access,
        major,
        minor,
        dev_type,
    );
    assert_eq!(
        verdict,
        Ok(1),
        "the matching write must be allowed by the attached program"
    );
    assert!(device::device_allowed(&child, access, major, minor, dev_type));
    assert!(
        !device::device_allowed(&child, device::DEVICE_ACC_READ, major, minor, dev_type),
        "a non-matching access bit is denied"
    );
    assert!(
        device::device_allowed(&root, access, major, minor, dev_type),
        "a sibling subtree keeps its unrestricted policy"
    );

    // Single-attach semantics: the second attach replaces the first.
    let replacement = load_program(false);
    device::attach(&child, &replacement, device::ATTACH_NONE).unwrap();
    assert_eq!(device::attached_program_ids(&child).len(), 1);
    assert!(
        !device::device_allowed(&child, access, 1, 3, dev_type),
        "the replacement denies minor 3"
    );

    // Detach removes the policy again.
    let id = device::attached_program_ids(&child)[0];
    device::detach(&child, id).unwrap();
    assert!(
        device::device_allowed(&child, access, major, minor, dev_type),
        "no policy means allowed"
    );
}

/// Multi-attach keeps both programs; every one of them must allow.
/// An already-attached program re-attach is rejected.
#[def_test]
fn multi_attach_accumulates_and_one_deny_wins() {
    let root = root_cgroup();
    let target = root.create_child("bpftest-multi").unwrap();
    // First program: allows writes to 1:3. Second: allows writes to 1:9.
    let first = load_program(true);
    device::attach(&target, &first, device::ATTACH_ALLOW_MULTI).unwrap();
    let second = load_program(false);
    let second_id = second.id();
    device::attach(&target, &second, device::ATTACH_ALLOW_MULTI).unwrap();
    assert_eq!(device::attached_program_ids(&target).len(), 2);

    // Attaching the already-attached program again is EINVAL.
    assert_eq!(
        device::attach(&target, &second, device::ATTACH_ALLOW_MULTI).err(),
        Some(kerrno::KError::InvalidInput)
    );

    // Effective evaluation: every attached program must allow. The second
    // allows only minor 9, so the write to 1:3 is denied even though the
    // first would allow it — Linux's all-programs-must-allow semantics.
    let (access, major, minor, dev_type) = write_char_1_3();
    assert!(!device::device_allowed(&target, access, major, minor, dev_type));

    // Removing the denying program restores the first's verdict.
    device::detach(&target, second_id).unwrap();
    assert!(device::device_allowed(&target, access, major, minor, dev_type));
}

/// The interpreter must decide the same program the same way the host-side
/// kbpf tests do; this isolates a guest-side VM failure from an attach-path
/// one.
#[def_test]
fn run_device_program_decides_directly() {
    let program = load_program(true);
    let insns = program.insns_shared();

    // char write to 1:3: allowed by the rule set.
    let mut ctx = [0u8; 12];
    ctx[0..4].copy_from_slice(&0x0004_0002u32.to_le_bytes());
    ctx[4..8].copy_from_slice(&1u32.to_le_bytes());
    ctx[8..12].copy_from_slice(&3u32.to_le_bytes());
    let (ctx_memory, ctx_base) = ReadOnlyMemory::context(Arc::new(ctx));
    let mut vm = Vm::new();
    vm.add_read_only_memory(ctx_memory);
    let allowed = vm.run_with_initial_regs(
        &insns,
        InitialRegs {
            r1: ctx_base,
            ..InitialRegs::default()
        },
    );
    assert_eq!(allowed, Ok(1));

    // char read (access 2 << 16): the rule's access mask does not match -> deny.
    let mut ctx = [0u8; 12];
    ctx[0..4].copy_from_slice(&0x0002_0002u32.to_le_bytes());
    ctx[4..8].copy_from_slice(&1u32.to_le_bytes());
    ctx[8..12].copy_from_slice(&3u32.to_le_bytes());
    let (ctx_memory, ctx_base) = ReadOnlyMemory::context(Arc::new(ctx));
    let mut vm = Vm::new();
    vm.add_read_only_memory(ctx_memory);
    let denied = vm.run_with_initial_regs(
        &insns,
        InitialRegs {
            r1: ctx_base,
            ..InitialRegs::default()
        },
    );
    assert_eq!(denied, Ok(0));
}

/// A link attachment lives with the link file, not with the creating
/// descriptor or the program object: releasing the last link reference is
/// what removes the policy.
#[def_test]
fn link_attachment_lives_with_the_link_file() {
    let root = root_cgroup();
    let target = root.create_child("bpftest-link").unwrap();
    let (access, major, minor, dev_type) = write_char_1_3();
    // The denying program makes every transition observable.
    let program = load_program(false);

    let file = device::link_file(&target, &program, kcred::initial_cred()).unwrap();
    assert_eq!(device::attached_program_ids(&target).len(), 1);
    assert!(!device::device_allowed(&target, access, major, minor, dev_type));

    // Dropping the creating program object must not end the policy: the
    // attached entry owns the instruction bytes.
    drop(program);
    assert!(
        !device::device_allowed(&target, access, major, minor, dev_type),
        "the policy outlives the program object"
    );

    // Releasing the link descriptor detaches and restores access.
    drop(file);
    assert!(device::attached_program_ids(&target).is_empty());
    assert!(
        device::device_allowed(&target, access, major, minor, dev_type),
        "no policy means allowed"
    );
}

/// The attachment mode is sticky while a cgroup carries programs
/// (`cgrp->bpf.flags`): a single attach onto a multi-attached cgroup is
/// `EPERM`, the legacy fd-less detach is `EINVAL` there, and both reset once
/// the last attachment goes away.
#[def_test]
fn attach_mode_is_sticky_until_the_last_detach() {
    let root = root_cgroup();
    let target = root.create_child("bpftest-mode").unwrap();
    let first = load_program(true);
    let second = load_program(false);

    device::attach(&target, &first, device::ATTACH_ALLOW_MULTI).unwrap();
    assert_eq!(
        device::attach(&target, &second, device::ATTACH_NONE).err(),
        Some(kerrno::KError::OperationNotPermitted),
        "a single attach onto a multi cgroup is EPERM"
    );
    assert_eq!(
        device::attach(&target, &second, device::ATTACH_ALLOW_OVERRIDE).err(),
        Some(kerrno::KError::OperationNotPermitted),
        "any non-multi mode mismatches a multi cgroup"
    );
    assert_eq!(
        device::detach_legacy(&target).err(),
        Some(kerrno::KError::InvalidInput),
        "the fd-less detach is EINVAL in multi mode"
    );

    // Removing the last multi attachment resets the mode, so a single attach
    // works again and the fd-less detach removes it.
    device::detach(&target, first.id()).unwrap();
    device::attach(&target, &second, device::ATTACH_NONE).unwrap();
    device::detach_legacy(&target).unwrap();
    assert!(device::attached_program_ids(&target).is_empty());
}

/// Link attachments are identified by the link, not the program: a link
/// fd's release removes exactly its own entry, a multi-mode PROG_DETACH
/// never touches link-owned entries, and the attached program object
/// outlives its last user-space descriptor.
#[def_test]
fn link_identity_and_program_lifetime() {
    let root = root_cgroup();
    let target = root.create_child("bpftest-linkid").unwrap();
    let (access, major, minor, dev_type) = write_char_1_3();

    // Links are multi attachments: #1 allows 1:3, #2 denies it, both stay.
    let allowing = load_program(true);
    let denying = load_program(false);
    let link_one = device::link_file(&target, &allowing, kcred::initial_cred()).unwrap();
    let link_two = device::link_file(&target, &denying, kcred::initial_cred()).unwrap();
    assert_eq!(device::attached_program_ids(&target).len(), 2);
    // Multi semantics: every program must allow, so the denying link wins.
    assert!(!device::device_allowed(&target, access, major, minor, dev_type));

    // The entry holds the program object: dropping every user-space
    // descriptor of it keeps the attachment queryable.
    drop(allowing);
    drop(denying);
    assert!(!device::device_allowed(&target, access, major, minor, dev_type));

    // Releasing link #1 must not detach link #2's entry.
    drop(link_one);
    assert_eq!(device::attached_program_ids(&target).len(), 1);
    assert!(!device::device_allowed(&target, access, major, minor, dev_type));

    // Close link #2 and confirm the policy is gone.
    drop(link_two);
    assert!(device::attached_program_ids(&target).is_empty());
    assert!(device::device_allowed(&target, access, major, minor, dev_type));

    // A link on a multi-attached cgroup appends; the recorded mode reports
    // ALLOW_MULTI to a non-effective query.
    let program = load_program(true);
    device::attach(&target, &program, device::ATTACH_ALLOW_MULTI).unwrap();
    let link_three = device::link_file(&target, &program, kcred::initial_cred()).unwrap();
    assert_eq!(device::attached_program_ids(&target).len(), 2);
    assert_eq!(device::attachment_flags(&target), device::ATTACH_ALLOW_MULTI);
    drop(link_three);
    device::detach(&target, program.id()).unwrap();
    assert!(device::attached_program_ids(&target).is_empty());
}

/// Linux's hierarchy rules: a NONE-attached parent forbids child attaches
/// (`hierarchy_allows_attach`), an OVERRIDE parent permits them, and once a
/// level contributes programs its non-multi ancestors are shadowed — the
/// child's policy decides even though the parent would deny.
#[def_test]
fn hierarchy_gates_attach_and_single_shadowing() {
    let root = root_cgroup();
    let parent = root.create_child("bpftest-hier").unwrap();
    let child = parent.create_child("bpftest-hier-child").unwrap();
    let (access, major, minor, dev_type) = write_char_1_3();
    let denying = load_program(false);
    let allowing = load_program(true);

    // A NONE-attached parent forbids child attaches.
    device::attach(&parent, &denying, device::ATTACH_NONE).unwrap();
    assert_eq!(
        device::attach(&child, &allowing, device::ATTACH_ALLOW_MULTI).err(),
        Some(kerrno::KError::OperationNotPermitted),
        "NONE parent blocks child attaches"
    );

    // OVERRIDE parent: the child may attach, and its single attachment
    // shadows the parent's would-be-deny.
    device::detach(&parent, denying.id()).unwrap();
    device::attach(&parent, &denying, device::ATTACH_ALLOW_OVERRIDE).unwrap();
    device::attach(&child, &allowing, device::ATTACH_ALLOW_OVERRIDE).unwrap();
    assert!(
        device::device_allowed(&child, access, major, minor, dev_type),
        "the child's program shadows the OVERRIDE parent"
    );
    assert!(
        !device::device_allowed(&parent, access, major, minor, dev_type),
        "the parent itself keeps its deny"
    );
}
