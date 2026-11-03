// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kbpf::uapi::bpf_prog_type::BPF_PROG_TYPE_SOCKET_FILTER;
use kcred::{
    CAP_BPF, CAP_NET_ADMIN, CAP_PERFMON, CAP_SYS_ADMIN, CapSet, CapSets, Cred,
};
use kerrno::KError;
use unittest::{TestResult, assert_eq, def_test};
use unittest_support::TestUserValue;

use super::*;

fn with_cred<R>(cred: Cred, body: impl FnOnce() -> R) -> R {
    let caller = kprocess::current_user_thread();
    let saved = caller.cred();
    caller.commit_creds(cred);
    let result = body();
    caller.commit_creds((*saved).clone());
    result
}

fn cred_with_caps(caps: &[u32]) -> Cred {
    let mut cred = Cred::root();
    let set = caps
        .iter()
        .fold(CapSet::EMPTY, |set, cap| set.union(CapSet::from_bits(1u64 << cap)));
    cred.set_capabilities(CapSets::new(set, set, CapSet::EMPTY))
        .expect("requested caps may be retained from root permitted capabilities");
    cred
}

fn cap_bpf_cred() -> Cred {
    cred_with_caps(&[CAP_BPF])
}

fn load_attr(prog_type: u32) -> TestUserValue<BpfProgLoadAttrPrefix> {
    load_attr_with_insn_count(prog_type, 0)
}

fn load_attr_with_insn_count(
    prog_type: u32,
    insn_cnt: u32,
) -> TestUserValue<BpfProgLoadAttrPrefix> {
    TestUserValue::from_value(BpfProgLoadAttrPrefix {
        prog_type,
        insn_cnt,
        insns: 0,
        license: 0,
        log_level: 0,
        log_size: 0,
        log_buf: 0,
        kern_version: 0,
        prog_flags: 0,
    })
    .expect("allocate BPF_PROG_LOAD attribute")
}

#[def_test]
fn test_net_admin_program_types_match_linux() -> TestResult {
    // Enumerate every program type through Linux's current maximum, not only
    // the types this implementation currently returns `true` for. That makes
    // an accidentally added gate fail at the type Linux leaves ungated.
    for prog_type in 0..=BPF_PROG_TYPE_NETFILTER {
        let expected = matches!(
            prog_type,
            BPF_PROG_TYPE_SCHED_CLS
                | BPF_PROG_TYPE_SCHED_ACT
                | BPF_PROG_TYPE_XDP
                | BPF_PROG_TYPE_LWT_IN
                | BPF_PROG_TYPE_LWT_OUT
                | BPF_PROG_TYPE_LWT_XMIT
                | BPF_PROG_TYPE_LWT_SEG6LOCAL
                | BPF_PROG_TYPE_SK_SKB
                | BPF_PROG_TYPE_SK_MSG
                | BPF_PROG_TYPE_FLOW_DISSECTOR
                | BPF_PROG_TYPE_CGROUP_DEVICE
                | BPF_PROG_TYPE_CGROUP_SOCK
                | BPF_PROG_TYPE_CGROUP_SOCK_ADDR
                | BPF_PROG_TYPE_CGROUP_SOCKOPT
                | BPF_PROG_TYPE_CGROUP_SYSCTL
                | BPF_PROG_TYPE_SOCK_OPS
                | BPF_PROG_TYPE_EXT
                | BPF_PROG_TYPE_NETFILTER
        );
        assert_eq!(
            is_net_admin_prog_type(prog_type),
            expected,
            "net-admin program type {prog_type}"
        );
    }
    for prog_type in (BPF_PROG_TYPE_NETFILTER + 1)..=(BPF_PROG_TYPE_NETFILTER + 32) {
        assert!(!is_net_admin_prog_type(prog_type), "{prog_type}");
    }
    assert!(!is_net_admin_prog_type(u32::MAX));
    TestResult::Ok
}

#[def_test(user, serial)]
fn test_bpf_rejects_negative_commands_before_capability_checks() -> TestResult {
    with_cred(Cred::new(1000, 1000), || {
        assert_eq!(sys_bpf(-1, 0, 0), Err(KError::InvalidInput));
        TestResult::Ok
    })
}

#[def_test(user, serial)]
fn test_bpf_requires_bpf_capability_for_load_before_net_admin() -> TestResult {
    let attr = load_attr(BPF_PROG_TYPE_CGROUP_DEVICE);
    with_cred(Cred::new(1000, 1000), || {
        assert_eq!(
            sys_bpf(BPF_PROG_LOAD as i32, attr.as_user_ptr() as usize, core::mem::size_of::<BpfProgLoadAttrPrefix>() as u32),
            Err(KError::OperationNotPermitted)
        );
        TestResult::Ok
    })
}

#[def_test(user, serial)]
fn test_bpf_requires_cap_bpf_for_non_net_admin_load() -> TestResult {
    // SOCKET_FILTER is neither net-admin nor perfmon gated, so the entry
    // bpf_capable() check is the only thing that can reject this load. The
    // CGROUP_DEVICE case above would still pass if that entry gate vanished.
    let attr = load_attr(BPF_PROG_TYPE_SOCKET_FILTER);
    with_cred(Cred::new(1000, 1000), || {
        assert_eq!(
            sys_bpf(
                BPF_PROG_LOAD as i32,
                attr.as_user_ptr() as usize,
                core::mem::size_of::<BpfProgLoadAttrPrefix>() as u32
            ),
            Err(KError::OperationNotPermitted)
        );
        TestResult::Ok
    })
}

#[def_test(user, serial)]
fn test_net_admin_program_type_still_requires_net_admin_with_cap_bpf() -> TestResult {
    let attr = load_attr(BPF_PROG_TYPE_CGROUP_DEVICE);
    with_cred(cap_bpf_cred(), || {
        assert_eq!(
            sys_bpf(BPF_PROG_LOAD as i32, attr.as_user_ptr() as usize, core::mem::size_of::<BpfProgLoadAttrPrefix>() as u32),
            Err(KError::OperationNotPermitted)
        );
        TestResult::Ok
    })
}

#[def_test(user, serial)]
fn test_bpf_id_commands_require_sys_admin_not_just_cap_bpf() -> TestResult {
    with_cred(cap_bpf_cred(), || {
        for command in [BPF_PROG_GET_NEXT_ID, BPF_PROG_GET_FD_BY_ID] {
            assert_eq!(
                sys_bpf(command as i32, 0, 0),
                Err(KError::OperationNotPermitted)
            );
        }
        TestResult::Ok
    })
}

#[def_test]
fn test_perfmon_program_types_match_linux() -> TestResult {
    for prog_type in 0..=BPF_PROG_TYPE_NETFILTER {
        let expected = matches!(
            prog_type,
            BPF_PROG_TYPE_KPROBE
                | BPF_PROG_TYPE_TRACEPOINT
                | BPF_PROG_TYPE_PERF_EVENT
                | BPF_PROG_TYPE_RAW_TRACEPOINT
                | BPF_PROG_TYPE_RAW_TRACEPOINT_WRITABLE
                | BPF_PROG_TYPE_TRACING
                | BPF_PROG_TYPE_LSM
                | BPF_PROG_TYPE_STRUCT_OPS
                | BPF_PROG_TYPE_EXT
        );
        assert_eq!(
            is_perfmon_prog_type(prog_type),
            expected,
            "perfmon program type {prog_type}"
        );
    }
    for prog_type in (BPF_PROG_TYPE_NETFILTER + 1)..=(BPF_PROG_TYPE_NETFILTER + 32) {
        assert!(!is_perfmon_prog_type(prog_type), "{prog_type}");
    }
    assert!(!is_perfmon_prog_type(u32::MAX));
    TestResult::Ok
}

#[def_test(user, serial)]
fn test_perfmon_program_type_requires_perfmon_with_cap_bpf() -> TestResult {
    let attr = load_attr(BPF_PROG_TYPE_KPROBE);
    with_cred(cap_bpf_cred(), || {
        assert_eq!(
            sys_bpf(
                BPF_PROG_LOAD as i32,
                attr.as_user_ptr() as usize,
                core::mem::size_of::<BpfProgLoadAttrPrefix>() as u32,
            ),
            Err(KError::OperationNotPermitted)
        );
        TestResult::Ok
    })
}

#[def_test(user, serial)]
fn test_perfmon_program_type_accepts_perfmon_or_sys_admin() -> TestResult {
    // A one-instruction program with a null instruction pointer fails while
    // copying the program after the capability gates, so reaching
    // `BadAddress` proves the credential was accepted without first
    // attempting a large allocation.
    let attr = load_attr_with_insn_count(BPF_PROG_TYPE_KPROBE, 1);
    for cred in [
        cred_with_caps(&[CAP_BPF, CAP_PERFMON]),
        cred_with_caps(&[CAP_SYS_ADMIN]),
    ] {
        with_cred(cred, || {
            assert_eq!(
                sys_bpf(
                    BPF_PROG_LOAD as i32,
                    attr.as_user_ptr() as usize,
                    core::mem::size_of::<BpfProgLoadAttrPrefix>() as u32,
                ),
                Err(KError::BadAddress)
            );
            TestResult::Ok
        });
    }
    TestResult::Ok
}

#[cfg(feature = "cgroup2")]
#[def_test(user, serial)]
fn test_bpf_prog_query_is_gated_by_net_capability() -> TestResult {
    with_cred(Cred::new(1000, 1000), || {
        assert_eq!(
            sys_bpf(BPF_PROG_QUERY as i32, 0, 0),
            Err(KError::OperationNotPermitted)
        );
        TestResult::Ok
    });
    with_cred(cap_bpf_cred(), || {
        assert_eq!(
            sys_bpf(BPF_PROG_QUERY as i32, 0, 0),
            Err(KError::OperationNotPermitted)
        );
        TestResult::Ok
    });
    for cred in [
        cred_with_caps(&[CAP_NET_ADMIN]),
        cred_with_caps(&[CAP_SYS_ADMIN]),
    ] {
        with_cred(cred, || {
            assert_eq!(
                sys_bpf(BPF_PROG_QUERY as i32, 0, 0),
                Err(KError::InvalidInput)
            );
            TestResult::Ok
        });
    }
    TestResult::Ok
}
