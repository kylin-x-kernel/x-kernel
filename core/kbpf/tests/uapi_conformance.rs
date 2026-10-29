// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Independent conformance check for the generated uapi table.
//!
//! These expectations are transcribed by hand from the Linux uapi headers
//! (`include/uapi/linux/bpf.h`, `include/uapi/linux/bpf_common.h`) and from
//! `kernel/bpf/cgroup.c`. They deliberately do NOT come from
//! `core/kbpf/src/uapi.rs`: the generator is code, and code can be wrong.
//! A mismatch means either the table drifted from the uapi ABI or the
//! generator mis-evaluated an expression — both are release blockers.
//!
//! Run with `cargo test -p kbpf --test uapi_conformance`.
//!
//! Re-check this list whenever the uapi baseline moves; a value that
//! changes upstream (never for uapi, but a *renamed* enum can shift
//! position) must be re-transcribed here, not regenerated.

use kbpf::uapi::{
    bpf_attach_type, bpf_cmd, bpf_devcg_acc, bpf_devcg_dev, bpf_func_id, bpf_map_type,
    bpf_map_update_flags, bpf_prog_type, defines,
};

/// `enum bpf_cmd` — position in `include/uapi/linux/bpf.h`.
#[test]
fn bpf_cmd_matches_linux() {
    let expected: &[(&str, u32)] = &[
        ("BPF_MAP_CREATE", 0),
        ("BPF_MAP_LOOKUP_ELEM", 1),
        ("BPF_MAP_UPDATE_ELEM", 2),
        ("BPF_MAP_DELETE_ELEM", 3),
        ("BPF_MAP_GET_NEXT_KEY", 4),
        ("BPF_PROG_LOAD", 5),
        ("BPF_OBJ_PIN", 6),
        ("BPF_OBJ_GET", 7),
        ("BPF_PROG_ATTACH", 8),
        ("BPF_PROG_DETACH", 9),
        ("BPF_PROG_TEST_RUN", 10),
        ("BPF_PROG_GET_NEXT_ID", 11),
        ("BPF_MAP_GET_NEXT_ID", 12),
        ("BPF_PROG_GET_FD_BY_ID", 13),
        ("BPF_MAP_GET_FD_BY_ID", 14),
        ("BPF_OBJ_GET_INFO_BY_FD", 15),
        ("BPF_PROG_QUERY", 16),
        ("BPF_RAW_TRACEPOINT_OPEN", 17),
        ("BPF_BTF_LOAD", 18),
        ("BPF_BTF_GET_FD_BY_ID", 19),
        ("BPF_TASK_FD_QUERY", 20),
        ("BPF_MAP_LOOKUP_AND_DELETE_ELEM", 21),
        ("BPF_MAP_FREEZE", 22),
        ("BPF_BTF_GET_NEXT_ID", 23),
        ("BPF_MAP_LOOKUP_BATCH", 24),
        ("BPF_MAP_LOOKUP_AND_DELETE_BATCH", 25),
        ("BPF_MAP_UPDATE_BATCH", 26),
        ("BPF_MAP_DELETE_BATCH", 27),
        ("BPF_LINK_CREATE", 28),
        ("BPF_LINK_UPDATE", 29),
        ("BPF_LINK_GET_FD_BY_ID", 30),
        ("BPF_LINK_GET_NEXT_ID", 31),
        ("BPF_ENABLE_STATS", 32),
        ("BPF_ITER_CREATE", 33),
        ("BPF_LINK_DETACH", 34),
        ("BPF_PROG_BIND_MAP", 35),
    ];
    let actual: &[(&str, u32)] = &[
        ("BPF_MAP_CREATE", bpf_cmd::BPF_MAP_CREATE),
        ("BPF_MAP_LOOKUP_ELEM", bpf_cmd::BPF_MAP_LOOKUP_ELEM),
        ("BPF_MAP_UPDATE_ELEM", bpf_cmd::BPF_MAP_UPDATE_ELEM),
        ("BPF_MAP_DELETE_ELEM", bpf_cmd::BPF_MAP_DELETE_ELEM),
        ("BPF_MAP_GET_NEXT_KEY", bpf_cmd::BPF_MAP_GET_NEXT_KEY),
        ("BPF_PROG_LOAD", bpf_cmd::BPF_PROG_LOAD),
        ("BPF_OBJ_PIN", bpf_cmd::BPF_OBJ_PIN),
        ("BPF_OBJ_GET", bpf_cmd::BPF_OBJ_GET),
        ("BPF_PROG_ATTACH", bpf_cmd::BPF_PROG_ATTACH),
        ("BPF_PROG_DETACH", bpf_cmd::BPF_PROG_DETACH),
        ("BPF_PROG_TEST_RUN", bpf_cmd::BPF_PROG_TEST_RUN),
        ("BPF_PROG_GET_NEXT_ID", bpf_cmd::BPF_PROG_GET_NEXT_ID),
        ("BPF_MAP_GET_NEXT_ID", bpf_cmd::BPF_MAP_GET_NEXT_ID),
        ("BPF_PROG_GET_FD_BY_ID", bpf_cmd::BPF_PROG_GET_FD_BY_ID),
        ("BPF_MAP_GET_FD_BY_ID", bpf_cmd::BPF_MAP_GET_FD_BY_ID),
        ("BPF_OBJ_GET_INFO_BY_FD", bpf_cmd::BPF_OBJ_GET_INFO_BY_FD),
        ("BPF_PROG_QUERY", bpf_cmd::BPF_PROG_QUERY),
        ("BPF_RAW_TRACEPOINT_OPEN", bpf_cmd::BPF_RAW_TRACEPOINT_OPEN),
        ("BPF_BTF_LOAD", bpf_cmd::BPF_BTF_LOAD),
        ("BPF_BTF_GET_FD_BY_ID", bpf_cmd::BPF_BTF_GET_FD_BY_ID),
        ("BPF_TASK_FD_QUERY", bpf_cmd::BPF_TASK_FD_QUERY),
        (
            "BPF_MAP_LOOKUP_AND_DELETE_ELEM",
            bpf_cmd::BPF_MAP_LOOKUP_AND_DELETE_ELEM,
        ),
        ("BPF_MAP_FREEZE", bpf_cmd::BPF_MAP_FREEZE),
        ("BPF_BTF_GET_NEXT_ID", bpf_cmd::BPF_BTF_GET_NEXT_ID),
        ("BPF_MAP_LOOKUP_BATCH", bpf_cmd::BPF_MAP_LOOKUP_BATCH),
        (
            "BPF_MAP_LOOKUP_AND_DELETE_BATCH",
            bpf_cmd::BPF_MAP_LOOKUP_AND_DELETE_BATCH,
        ),
        ("BPF_MAP_UPDATE_BATCH", bpf_cmd::BPF_MAP_UPDATE_BATCH),
        ("BPF_MAP_DELETE_BATCH", bpf_cmd::BPF_MAP_DELETE_BATCH),
        ("BPF_LINK_CREATE", bpf_cmd::BPF_LINK_CREATE),
        ("BPF_LINK_UPDATE", bpf_cmd::BPF_LINK_UPDATE),
        ("BPF_LINK_GET_FD_BY_ID", bpf_cmd::BPF_LINK_GET_FD_BY_ID),
        ("BPF_LINK_GET_NEXT_ID", bpf_cmd::BPF_LINK_GET_NEXT_ID),
        ("BPF_ENABLE_STATS", bpf_cmd::BPF_ENABLE_STATS),
        ("BPF_ITER_CREATE", bpf_cmd::BPF_ITER_CREATE),
        ("BPF_LINK_DETACH", bpf_cmd::BPF_LINK_DETACH),
        ("BPF_PROG_BIND_MAP", bpf_cmd::BPF_PROG_BIND_MAP),
    ];
    assert_eq!(actual, expected);
}

/// The program and attach types the cgroup-v2 device controller uses.
///
/// These two were transcribed wrong once (6 and 28 instead of 15 and 6),
/// which made every `runc` attach fail with EINVAL — hence the explicit
/// positional check against the enum order.
#[test]
fn device_controller_prog_and_attach_types() {
    // `enum bpf_prog_type`: UNSPEC, SOCKET_FILTER, KPROBE, SCHED_CLS,
    // SCHED_ACT, TRACEPOINT, XDP, PERF_EVENT, CGROUP_SKB, CGROUP_SOCK,
    // LWT_IN, LWT_OUT, LWT_XMIT, SOCK_OPS, SK_SKB, CGROUP_DEVICE -> 15.
    assert_eq!(bpf_prog_type::BPF_PROG_TYPE_UNSPEC, 0);
    assert_eq!(bpf_prog_type::BPF_PROG_TYPE_SOCKET_FILTER, 1);
    assert_eq!(bpf_prog_type::BPF_PROG_TYPE_XDP, 6);
    assert_eq!(bpf_prog_type::BPF_PROG_TYPE_CGROUP_DEVICE, 15);

    // `enum bpf_attach_type`: INET_INGRESS, INET_EGRESS, INET_SOCK_CREATE,
    // SOCK_OPS, SK_SKB_STREAM_PARSER, SK_SKB_STREAM_VERDICT,
    // CGROUP_DEVICE -> 6.
    assert_eq!(bpf_attach_type::BPF_CGROUP_INET_INGRESS, 0);
    assert_eq!(bpf_attach_type::BPF_CGROUP_DEVICE, 6);
}

/// Maps and the array-map type this kernel implements.
#[test]
fn map_types_and_update_flags() {
    assert_eq!(bpf_map_type::BPF_MAP_TYPE_UNSPEC, 0);
    assert_eq!(bpf_map_type::BPF_MAP_TYPE_HASH, 1);
    assert_eq!(bpf_map_type::BPF_MAP_TYPE_ARRAY, 2);

    assert_eq!(bpf_map_update_flags::BPF_ANY, 0);
    assert_eq!(bpf_map_update_flags::BPF_NOEXIST, 1);
    assert_eq!(bpf_map_update_flags::BPF_EXIST, 2);
}

/// `bpf_cgroup_dev_ctx::access_type = (BPF_DEVCG_ACC_* << 16) | BPF_DEVCG_DEV_*`
/// (`kernel/bpf/cgroup.c` composes it that way; the uapi enums carry the
/// un-shifted bits).
#[test]
fn devcg_encoding() {
    assert_eq!(bpf_devcg_acc::BPF_DEVCG_ACC_MKNOD, 1 << 0);
    assert_eq!(bpf_devcg_acc::BPF_DEVCG_ACC_READ, 1 << 1);
    assert_eq!(bpf_devcg_acc::BPF_DEVCG_ACC_WRITE, 1 << 2);
    assert_eq!(bpf_devcg_dev::BPF_DEVCG_DEV_BLOCK, 1 << 0);
    assert_eq!(bpf_devcg_dev::BPF_DEVCG_DEV_CHAR, 1 << 1);
}

/// Attach flags and limits used by `BPF_PROG_ATTACH`/`BPF_PROG_QUERY` and by
/// `kernel/bpf/cgroup.c` (`BPF_CGROUP_MAX_PROGS`).
#[test]
fn attach_flags_and_limits() {
    assert_eq!(defines::BPF_F_ALLOW_OVERRIDE, 1 << 0);
    assert_eq!(defines::BPF_F_ALLOW_MULTI, 1 << 1);
    assert_eq!(defines::BPF_F_REPLACE, 1 << 2);
    assert_eq!(defines::BPF_F_QUERY_EFFECTIVE, 1 << 0);
    assert_eq!(defines::BPF_CGROUP_MAX_PROGS, 64);

    // `lddw` source markers.
    assert_eq!(defines::BPF_PSEUDO_MAP_FD, 1);
    assert_eq!(defines::BPF_PSEUDO_MAP_VALUE, 2);
    // `BPF_LD | BPF_DW | BPF_IMM` — the `lddw` slot prefix the loader scans.
    assert_eq!(defines::BPF_LD | defines::BPF_DW | defines::BPF_IMM, 0x18);
}

/// ISA bit layout (`include/uapi/linux/bpf_common.h` + `bpf.h`), which the
/// interpreter's class/operation dispatch derives every opcode from.
#[test]
fn isa_bit_layout() {
    // Classes (low 3 bits).
    assert_eq!(defines::BPF_LD, 0x00);
    assert_eq!(defines::BPF_LDX, 0x01);
    assert_eq!(defines::BPF_ST, 0x02);
    assert_eq!(defines::BPF_STX, 0x03);
    assert_eq!(defines::BPF_ALU, 0x04);
    assert_eq!(defines::BPF_JMP, 0x05);
    assert_eq!(defines::BPF_JMP32, 0x06);
    assert_eq!(defines::BPF_ALU64, 0x07);

    // Sizes (bits 3-4).
    assert_eq!(defines::BPF_W, 0x00);
    assert_eq!(defines::BPF_H, 0x08);
    assert_eq!(defines::BPF_B, 0x10);
    assert_eq!(defines::BPF_DW, 0x18);

    // Source bit.
    assert_eq!(defines::BPF_K, 0x00);
    assert_eq!(defines::BPF_X, 0x08);

    // ALU operations (bits 4-7).
    assert_eq!(defines::BPF_ADD, 0x00);
    assert_eq!(defines::BPF_SUB, 0x10);
    assert_eq!(defines::BPF_MUL, 0x20);
    assert_eq!(defines::BPF_DIV, 0x30);
    assert_eq!(defines::BPF_OR, 0x40);
    assert_eq!(defines::BPF_AND, 0x50);
    assert_eq!(defines::BPF_LSH, 0x60);
    assert_eq!(defines::BPF_RSH, 0x70);
    assert_eq!(defines::BPF_NEG, 0x80);
    assert_eq!(defines::BPF_MOD, 0x90);
    assert_eq!(defines::BPF_XOR, 0xa0);
    assert_eq!(defines::BPF_MOV, 0xb0);
    assert_eq!(defines::BPF_ARSH, 0xc0);
    assert_eq!(defines::BPF_END, 0xd0);

    // Jump conditions.
    assert_eq!(defines::BPF_JA, 0x00);
    assert_eq!(defines::BPF_JEQ, 0x10);
    assert_eq!(defines::BPF_JGT, 0x20);
    assert_eq!(defines::BPF_JGE, 0x30);
    assert_eq!(defines::BPF_JSET, 0x40);
    assert_eq!(defines::BPF_JNE, 0x50);
    assert_eq!(defines::BPF_JSGT, 0x60);
    assert_eq!(defines::BPF_JSGE, 0x70);
    assert_eq!(defines::BPF_CALL, 0x80);
    assert_eq!(defines::BPF_EXIT, 0x90);
    assert_eq!(defines::BPF_JLT, 0xa0);
    assert_eq!(defines::BPF_JLE, 0xb0);
    assert_eq!(defines::BPF_JSLT, 0xc0);
    assert_eq!(defines::BPF_JSLE, 0xd0);

    // Endianness selectors.
    assert_eq!(defines::BPF_TO_LE, 0x00);
    assert_eq!(defines::BPF_TO_BE, 0x08);
}

/// The opcode bytes the interpreter decodes, composed from the bit layout the
/// way `kernel/bpf/core.c`'s jump table does. A wrong composition here is
/// exactly the class of bug the table exists to prevent.
#[test]
fn composed_opcodes() {
    let opcode = |class: u32, src: u32, op: u32| (class | src | op) as u8;

    // ALU32 (class 0x04).
    assert_eq!(
        opcode(defines::BPF_ALU, defines::BPF_K, defines::BPF_MOV),
        0xb4
    );
    assert_eq!(
        opcode(defines::BPF_ALU, defines::BPF_X, defines::BPF_MOV),
        0xbc
    );
    assert_eq!(
        opcode(defines::BPF_ALU, defines::BPF_K, defines::BPF_AND),
        0x54
    );
    // ALU64 (class 0x07).
    assert_eq!(
        opcode(defines::BPF_ALU64, defines::BPF_K, defines::BPF_MOV),
        0xb7
    );
    assert_eq!(
        opcode(defines::BPF_ALU64, defines::BPF_X, defines::BPF_MOV),
        0xbf
    );
    // JMP (class 0x05).
    assert_eq!(
        opcode(defines::BPF_JMP, defines::BPF_K, defines::BPF_JA),
        0x05
    );
    assert_eq!(
        opcode(defines::BPF_JMP, defines::BPF_K, defines::BPF_JEQ),
        0x15
    );
    assert_eq!(
        opcode(defines::BPF_JMP, defines::BPF_K, defines::BPF_JSET),
        0x45
    );
    assert_eq!(
        opcode(defines::BPF_JMP, defines::BPF_K, defines::BPF_EXIT),
        0x95
    );
    assert_eq!(
        opcode(defines::BPF_JMP, defines::BPF_K, defines::BPF_CALL),
        0x85
    );
    // JMP32 (class 0x06).
    assert_eq!(
        opcode(defines::BPF_JMP32, defines::BPF_K, defines::BPF_JNE),
        0x56
    );
    assert_eq!(
        opcode(defines::BPF_JMP32, defines::BPF_K, defines::BPF_JSLT),
        0xc6
    );
    // END: source bit selects the byte order, so ALU|END|K/X are 0xd4/0xdc
    // and ALU64|END|K/X are 0xd7/0xdf.
    assert_eq!(
        opcode(defines::BPF_ALU, defines::BPF_K, defines::BPF_END),
        0xd4
    );
    assert_eq!(
        opcode(defines::BPF_ALU, defines::BPF_X, defines::BPF_END),
        0xdc
    );
    assert_eq!(
        opcode(defines::BPF_ALU64, defines::BPF_K, defines::BPF_END),
        0xd7
    );
    assert_eq!(
        opcode(defines::BPF_ALU64, defines::BPF_X, defines::BPF_END),
        0xdf
    );
    // Loads/stores.
    assert_eq!(opcode(defines::BPF_LDX, defines::BPF_W, 0x60), 0x61);
    assert_eq!(opcode(defines::BPF_LD, defines::BPF_DW, 0x00), 0x18);
    assert_eq!(opcode(defines::BPF_STX, defines::BPF_DW, 0x60), 0x7b);
}

/// A few helper ids the interpreter dispatches on.
#[test]
fn helper_ids() {
    assert_eq!(bpf_func_id::BPF_FUNC_unspec, 0);
    assert_eq!(bpf_func_id::BPF_FUNC_map_lookup_elem, 1);
    assert_eq!(bpf_func_id::BPF_FUNC_ktime_get_ns, 5);
    assert_eq!(bpf_func_id::BPF_FUNC_trace_printk, 6);
    assert_eq!(bpf_func_id::BPF_FUNC_get_prandom_u32, 7);
}

/// The table is meant to be complete, not a curated subset: every enum and
/// every integer `#define` in the uapi headers must be present. These counts
/// are the header totals at the recorded baseline; a change means the
/// baseline moved (expected — re-check the diff) or the generator started
/// dropping declarations (a bug).
#[test]
fn table_is_complete() {
    // 61 enums in `bpf.h`; `bpf_func_id` is emitted from its macro mapper,
    // so 60 enum modules plus the `defines` and `bpf_func_id` modules.
    assert_eq!(kbpf::uapi::ENUM_MODULES.len(), 60);
    // Nothing in either header is silently dropped: the generator reports
    // anything it cannot fold here.
    assert!(
        kbpf::uapi::NOT_CONSTANTS.is_empty(),
        "unfolded uapi declarations: {:?}",
        kbpf::uapi::NOT_CONSTANTS
    );
}

/// Values that live outside the uapi headers are few, named with their
/// source, and pinned here: the expectation is written by hand, so adding
/// another such value to the generator fails this test until someone decides
/// it is acceptable and updates the list.
#[test]
fn non_uapi_values_are_declared_explicitly() {
    // `BPF_CGROUP_MAX_PROGS` (`kernel/bpf/cgroup.c`) is the only one today.
    assert_eq!(
        kbpf::uapi::NON_UAPI_VALUES,
        &["BPF_CGROUP_MAX_PROGS"],
        "a value outside the uapi headers was added or removed"
    );
    assert_eq!(kbpf::uapi::defines::BPF_CGROUP_MAX_PROGS, 64);
}
