// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test(user, serial)]
fn network_owner_failure_precedes_child_relationships() {
    let process = current_user_thread().process().clone();
    let children: alloc::vec::Vec<_> = process.children().iter().map(|child| child.pid()).collect();
    let threads = process.threads();
    let context = UserContext::new(0x1234, 0x8000usize.into(), 0x5678);
    let mut request = CloneRequest::new();
    request.set_flags(CloneFlags::NEWNET);
    let mut called = false;
    let result = request.do_clone_with_preparation(
        &context,
        |_, _| ktask::PreparedTaskEntry::new(|| {}),
        || {
            called = true;
            Err(KError::NoMemory)
        },
    );
    assert_eq!(result, Err(KError::NoMemory));
    assert!(called);
    let after: alloc::vec::Vec<_> = process.children().iter().map(|child| child.pid()).collect();
    assert_eq!(after, children);
    assert_eq!(process.threads(), threads);
}

#[def_test(user, serial)]
fn entry_allocation_failure_precedes_process_and_thread_preparation() {
    let process = current_user_thread().process().clone();
    let children: alloc::vec::Vec<_> = process.children().iter().map(|child| child.pid()).collect();
    let threads = process.threads();
    let context = UserContext::new(0x1234, 0x8000usize.into(), 0x5678);
    for flags in [
        CloneFlags::empty(),
        CloneFlags::THREAD | CloneFlags::VM | CloneFlags::SIGHAND,
    ] {
        let called = core::cell::Cell::new(false);
        let mut request = CloneRequest::new();
        request.set_flags(flags);
        let result = request.do_clone_with_entry(&context, |_, _| {
            called.set(true);
            Err(KError::NoMemory)
        });
        assert_eq!(result, Err(KError::NoMemory));
        assert!(called.get());
        let after: alloc::vec::Vec<_> =
            process.children().iter().map(|child| child.pid()).collect();
        assert_eq!(after, children);
        assert_eq!(process.threads(), threads);
    }
}

#[def_test(user, serial)]
fn invalid_exit_signal_is_rejected_before_entry_allocation() {
    let called = core::cell::Cell::new(false);
    let mut request = CloneRequest::new();
    request.set_exit_signal(u64::MAX);
    let context = UserContext::new(0x1234, 0x8000usize.into(), 0x5678);
    let result = request.do_clone_with_entry(&context, |_, _| {
        called.set(true);
        Err(KError::NoMemory)
    });
    assert_eq!(result, Err(KError::InvalidInput));
    assert!(!called.get());
}

#[def_test]
fn test_validate_rejects_newns_and_fs() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::NEWNS | CloneFlags::FS);
    assert!(req.validate_namespace_flags().is_err());
}

#[def_test]
fn test_validate_rejects_newipc_and_sysvsem() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::NEWIPC | CloneFlags::SYSVSEM);
    assert!(req.validate_namespace_flags().is_err());
}

#[def_test]
fn test_validate_rejects_newpid_and_thread() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::NEWPID | CloneFlags::THREAD);
    assert!(req.validate_namespace_flags().is_err());
}

#[def_test]
fn test_validate_rejects_newpid_and_parent() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::NEWPID | CloneFlags::PARENT);
    assert!(req.validate_namespace_flags().is_err());
}

#[def_test]
fn test_validate_accepts_newpid() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::NEWPID);
    assert!(req.validate_namespace_flags().is_ok());
}

#[def_test]
fn test_validate_accepts_every_namespace_flag() {
    for flags in [
        CloneFlags::NEWNS,
        CloneFlags::NEWUTS,
        CloneFlags::NEWIPC,
        CloneFlags::NEWUSER,
        CloneFlags::NEWPID,
        CloneFlags::NEWNET,
        CloneFlags::NEWCGROUP,
        CloneFlags::NEWTIME,
    ] {
        let mut req = CloneRequest::new();
        req.set_flags(flags);
        assert!(
            req.validate_namespace_flags().is_ok(),
            "{flags:?} is implemented and must pass flag validation"
        );
        let expected = if flags.bits() == CloneFlags::NEWNS.bits() {
            NamespaceFlags::NEWNS
        } else if flags.bits() == CloneFlags::NEWUTS.bits() {
            NamespaceFlags::NEWUTS
        } else if flags.bits() == CloneFlags::NEWIPC.bits() {
            NamespaceFlags::NEWIPC
        } else if flags.bits() == CloneFlags::NEWUSER.bits() {
            NamespaceFlags::NEWUSER
        } else if flags.bits() == CloneFlags::NEWPID.bits() {
            NamespaceFlags::NEWPID
        } else if flags.bits() == CloneFlags::NEWNET.bits() {
            NamespaceFlags::NEWNET
        } else if flags.bits() == CloneFlags::NEWCGROUP.bits() {
            NamespaceFlags::NEWCGROUP
        } else {
            NamespaceFlags::NEWTIME
        };
        assert_eq!(req.extract_namespace_flags(), expected);
    }
}

/// A thread clone cannot switch namespaces: the namespace bundle hangs off
/// the process, so namespace flags combined with `CLONE_THREAD` are rejected
/// with `EINVAL` (Linux `copy_process()`) instead of being silently ignored.
#[def_test]
fn test_clone_rejects_namespace_flags_with_thread() {
    // NEWPID | THREAD already fails flag validation with EINVAL; the
    // thread-clone gate must cover the flags validation alone accepts.
    for flags in [
        CloneFlags::NEWUTS,
        CloneFlags::NEWIPC,
        CloneFlags::NEWNS,
        CloneFlags::NEWUSER,
        CloneFlags::NEWNET,
        CloneFlags::NEWCGROUP,
        CloneFlags::NEWTIME,
    ] {
        let mut req = CloneRequest::new();
        req.set_flags(flags | CloneFlags::THREAD | CloneFlags::VM | CloneFlags::SIGHAND);
        assert!(
            req.validate_namespace_flags().is_ok(),
            "{flags:?} alone passes flag validation"
        );
        let uctx = UserContext::new(0x1234, 0x8000usize.into(), 0x5678);
        let err = req.do_clone(&uctx).unwrap_err();
        assert_eq!(
            LinuxError::from(err),
            LinuxError::EINVAL,
            "CLONE_THREAD | {flags:?} must be EINVAL"
        );
    }
}

#[def_test]
fn test_extract_namespace_flags_includes_newtime() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::NEWNS | CloneFlags::NEWUTS | CloneFlags::NEWTIME);
    let ns = req.extract_namespace_flags();
    assert!(ns.contains(NamespaceFlags::NEWNS));
    assert!(ns.contains(NamespaceFlags::NEWUTS));
    assert!(ns.contains(NamespaceFlags::NEWTIME));
}

#[def_test]
fn test_parse_exit_signal_rejects_invalid_signal_number() {
    assert!(CloneRequest::parse_exit_signal(0).unwrap().is_none());
    assert_eq!(
        CloneRequest::parse_exit_signal(SIGCHLD as u64).unwrap(),
        Some(Signo::SIGCHLD)
    );
    assert!(CloneRequest::parse_exit_signal(65).is_err());
    assert!(CloneRequest::parse_exit_signal(1 << 32).is_err());
}

#[def_test]
fn test_clone3_validation_rejects_exit_signal_with_shared_parent() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::PARENT)
        .set_exit_signal(SIGCHLD as u64)
        .reject_exit_signal_with_shared_parent();

    assert!(req.validate_exit_signal_policy().is_err());
}

#[def_test]
fn test_validate_rejects_clear_sighand_with_shared_sighand() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::CLEAR_SIGHAND | CloneFlags::SIGHAND | CloneFlags::VM);
    let err = req.validate_clear_sighand_flag().unwrap_err();
    assert_eq!(LinuxError::from(err), LinuxError::EINVAL);
}

#[def_test]
fn test_validate_accepts_clear_sighand_without_shared_sighand() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::CLEAR_SIGHAND | CloneFlags::VM);
    assert!(req.validate_clear_sighand_flag().is_ok());
}

#[def_test]
fn test_validate_rejects_into_cgroup_with_thread() {
    // The INTO_CGROUP|THREAD rejection lives in do_clone (after the
    // THREAD-needs-SIGHAND check and the clone3 fd resolution), not in the
    // early CLEAR_SIGHAND combination check.
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::INTO_CGROUP | CloneFlags::THREAD | CloneFlags::VM | CloneFlags::SIGHAND);
    let uctx = UserContext::new(0x1234, 0x8000usize.into(), 0x5678);
    let err = req.do_clone(&uctx).unwrap_err();
    assert_eq!(LinuxError::from(err), LinuxError::EOPNOTSUPP);
}

#[def_test]
fn test_thread_without_sighand_einval_precedes_into_cgroup() {
    // Linux copy_process rejects CLONE_THREAD without CLONE_SIGHAND (EINVAL)
    // before the cgroup fd lookup and before the INTO_CGROUP|THREAD rejection,
    // so this combination answers EINVAL, not EOPNOTSUPP or a descriptor error.
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::INTO_CGROUP | CloneFlags::THREAD | CloneFlags::VM);
    let uctx = UserContext::new(0x1234, 0x8000usize.into(), 0x5678);
    let err = req.do_clone(&uctx).unwrap_err();
    assert_eq!(LinuxError::from(err), LinuxError::EINVAL);
}

#[def_test]
fn test_validate_rejects_into_cgroup_without_target() {
    // The resolved-target invariant is enforced by do_clone (the clone3
    // adapter resolves the target before calling it), not by the flag-only
    // combination check that runs before descriptor resolution.
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::INTO_CGROUP);
    let uctx = UserContext::new(0x1234, 0x8000usize.into(), 0x5678);
    let err = req.do_clone(&uctx).unwrap_err();
    assert_eq!(LinuxError::from(err), LinuxError::EINVAL);
}

#[def_test]
fn test_validate_accepts_plain_fork_and_plain_thread() {
    let mut req = CloneRequest::new();
    assert!(req.validate_clear_sighand_flag().is_ok());
    req.set_flags(CloneFlags::THREAD | CloneFlags::VM | CloneFlags::SIGHAND);
    assert!(req.validate_clear_sighand_flag().is_ok());
}

#[def_test]
fn test_clone_rejects_newuser_with_shared_fs() {
    let mut req = CloneRequest::new();
    req.set_flags(CloneFlags::NEWUSER | CloneFlags::FS);
    assert_eq!(
        LinuxError::from(req.validate_namespace_flags().unwrap_err()),
        LinuxError::EINVAL
    );
}
