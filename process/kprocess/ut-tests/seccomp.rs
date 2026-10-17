// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Seccomp context behavior: strict allowlist, mode transitions, and the
//! `TSYNC` no-loosening contract. Registered from `src/thread/seccomp.rs`.

use alloc::{sync::Arc, vec};

use kseccomp::{Decision, SeccompData, SeccompFilter, SockFilter};
use linux_sysno::Sysno;
use unittest::{assert, assert_eq, def_test};

use super::{SeccompContext, SeccompMode};

fn snapshot(nr: i32) -> SeccompData {
    SeccompData::new(nr, 0xc000_00b7, 0, [0; 6])
}

/// Linux UAPI `SECCOMP_RET_*` action constants (`linux/seccomp.h`).
const SECCOMP_RET_KILL_THREAD: u32 = 0x0000_0000;
const SECCOMP_RET_TRAP: u32 = 0x0003_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;

fn ret_program(ret: u32) -> Arc<SeccompFilter> {
    Arc::new(
        SeccompFilter::new(vec![SockFilter {
            code: 0x06, // BPF_RET | BPF_K
            jt: 0,
            jf: 0,
            k: ret,
        }])
        .unwrap(),
    )
}

fn getpid() -> i32 {
    Sysno::getpid.id()
}

#[def_test]
fn disabled_mode_allows() {
    let ctx = SeccompContext::new();
    assert_eq!(ctx.evaluate(&snapshot(getpid())), Decision::Allow);
}

/// Linux `SECCOMP_MODE_STRICT` permits exactly `read`, `write`, `_exit`,
/// `rt_sigreturn`. The allowlist yields `KillThread` for anything else; the
/// syscall layer turns a strict-mode denial into `SIGKILL`.
#[def_test]
fn strict_allowlist_permits_the_four_syscalls() {
    let ctx = SeccompContext::new();
    ctx.set_strict().unwrap();
    for sysno in [Sysno::read, Sysno::write, Sysno::exit, Sysno::rt_sigreturn] {
        assert_eq!(
            ctx.evaluate(&snapshot(sysno.id())),
            Decision::Allow,
            "strict mode must allow its allowlisted syscall"
        );
    }
    assert_eq!(ctx.evaluate(&snapshot(getpid())), Decision::KillThread);
}

/// A `SECCOMP_RET_TRAP` result (and any unrecognised action) must never be
/// silently allowed: Linux terminates the thread with `SIGSYS`.
#[def_test]
fn unrecognised_action_fails_closed() {
    let ctx = SeccompContext::new();
    ctx.append_filter(ret_program(SECCOMP_RET_TRAP)).unwrap();
    assert_eq!(
        ctx.evaluate(&snapshot(getpid())),
        Decision::Unsupported,
        "a TRAP action must fail closed"
    );

    let ctx = SeccompContext::new();
    ctx.append_filter(ret_program(0x1234_0000)).unwrap();
    assert_eq!(
        ctx.evaluate(&snapshot(getpid())),
        Decision::Unsupported,
        "an unrecognised action must fail closed"
    );
}

/// A more permissive filter may not be installed over strict mode: Linux
/// refuses the mode change with `EINVAL`.
#[def_test]
fn strict_mode_rejects_a_later_filter() {
    let ctx = SeccompContext::new();
    ctx.set_strict().unwrap();
    assert!(ctx.append_filter(ret_program(SECCOMP_RET_ALLOW)).is_err());
    assert_eq!(
        ctx.evaluate(&snapshot(getpid())),
        Decision::KillThread,
        "strict mode must survive a rejected filter install"
    );
}

/// The reverse direction: strict mode may not replace an existing filter
/// chain, because the strict allowlist is less restrictive than e.g. a
/// deny-all chain.
#[def_test]
fn filter_chain_rejects_strict_mode() {
    let ctx = SeccompContext::new();
    ctx.append_filter(ret_program(SECCOMP_RET_KILL_THREAD))
        .unwrap();
    assert!(ctx.set_strict().is_err());
    assert_eq!(
        ctx.evaluate(&snapshot(Sysno::read.id())),
        Decision::KillThread,
        "strict mode must not drop an existing deny-all filter"
    );
}

/// `TSYNC` copies the caller's chain to siblings only when the sibling's chain
/// is a prefix of the caller's, so restrictions can never be removed.
#[def_test]
fn tsync_rejects_a_non_ancestral_sibling_chain() {
    let caller = SeccompContext::new();
    let sibling = SeccompContext::new();

    // The sibling denies everything; the caller allows everything.
    sibling.append_filter(ret_program(SECCOMP_RET_KILL_THREAD)).unwrap();
    caller.append_filter(ret_program(SECCOMP_RET_ALLOW)).unwrap();

    assert!(
        !sibling.is_syncable_with(&caller),
        "a sibling with a non-ancestral chain must not be synchronisable"
    );
}

#[def_test]
fn tsync_accepts_a_prefix_chain() {
    let caller = SeccompContext::new();
    let sibling = SeccompContext::new();
    let shared = ret_program(SECCOMP_RET_KILL_THREAD);
    sibling.append_filter(shared.clone()).unwrap();

    // The caller's chain is the sibling's chain plus one more filter.
    caller.append_filter(shared).unwrap();
    caller.append_filter(ret_program(SECCOMP_RET_ALLOW)).unwrap();

    assert!(sibling.is_syncable_with(&caller));
    sibling.sync_from(&caller);
    assert_eq!(sibling.mode(), SeccompMode::Filter);
    assert_eq!(sibling.evaluate(&snapshot(getpid())), Decision::KillThread);
}

/// A disabled sibling is picked up by `TSYNC` and moves to filter mode.
#[def_test]
fn tsync_upgrades_a_disabled_sibling() {
    let caller = SeccompContext::new();
    let sibling = SeccompContext::new();
    caller
        .append_filter(ret_program(SECCOMP_RET_KILL_THREAD))
        .unwrap();

    assert!(sibling.is_syncable_with(&caller));
    sibling.sync_from(&caller);
    assert_eq!(sibling.mode(), SeccompMode::Filter);
    assert_eq!(sibling.evaluate(&snapshot(getpid())), Decision::KillThread);
}

/// `fork`/`clone` inheritance copies the parent's mode, including strict mode.
#[def_test]
fn inheritance_copies_strict_mode() {
    let parent = SeccompContext::new();
    parent.set_strict().unwrap();
    let child = SeccompContext::new();
    child.inherit_from(&parent);
    assert_eq!(child.mode(), SeccompMode::Strict);
    assert_eq!(child.evaluate(&snapshot(getpid())), Decision::KillThread);
}
/// A sibling that installs a stricter chain *after* the caller's pre-check must
/// not be stripped by the broadcast: `sync_from` re-validates under the
/// sibling's own lock and refuses.
#[def_test]
fn tsync_revalidates_before_replacing_a_chain() {
    let caller = SeccompContext::new();
    let sibling = SeccompContext::new();

    let shared = ret_program(SECCOMP_RET_ALLOW);
    caller.append_filter(shared.clone()).unwrap();
    sibling.append_filter(shared).unwrap();
    assert!(sibling.is_syncable_with(&caller));

    // The sibling installs an extra deny-all filter after that check.
    sibling
        .append_filter(ret_program(SECCOMP_RET_KILL_THREAD))
        .unwrap();

    assert!(
        !sibling.sync_from(&caller),
        "sync_from must refuse once the chain is no longer a prefix"
    );
    assert_eq!(
        sibling.evaluate(&snapshot(getpid())),
        Decision::KillThread,
        "the sibling's own restriction must survive"
    );
}
