// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::LinuxError;
use khal::{mem::VirtAddr, paging::MappingFlags};
use kprocess::{current_user_process_address_space, current_user_thread};
use ktime_types::{MonotonicInstant, TimeSpan};
use linux_raw_sys::general::{
    FUTEX_OP_ADD, FUTEX_OP_CMP_EQ, FUTEX_PRIVATE_FLAG, FUTEX_WAIT, FUTEX_WAIT_BITSET,
    FUTEX_WAKE_OP, MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE, timespec,
};
use posix_types::UserPtr;
use unittest::{assert_eq, def_test};
use unittest_support::TestUserValue;

use super::{FutexWaitRestartBlock, arm_futex_wait_restart, sys_futex};
use crate::task::sys_restart_syscall;

const WAIT_BITSET_MASK: u32 = 0x1;

fn reset_restart_state() {
    ktask::current().clear_interrupt();
    current_user_thread().clear_restart_block();
}

fn take_futex_restart() -> Option<FutexWaitRestartBlock> {
    current_user_thread().take_restart_block::<FutexWaitRestartBlock>()
}

#[cfg(target_arch = "x86_64")]
fn restart_errno(err: LinuxError) -> usize {
    -(err.into_raw() as isize) as usize
}

#[def_test(user, serial)]
fn test_restart_syscall_without_block_returns_eintr() {
    reset_restart_state();
    let err = sys_restart_syscall().expect_err("empty restart-block must be EINTR");
    assert_eq!(LinuxError::from(err), LinuxError::EINTR);
}

#[def_test(user, serial)]
fn test_untimed_futex_wait_interrupt_returns_restartsys() {
    reset_restart_state();
    let word = TestUserValue::<u32>::from_value(1).unwrap();

    ktask::current().interrupt();
    let err = sys_futex(
        UserPtr::from(word.as_user_ptr()),
        FUTEX_WAIT | FUTEX_PRIVATE_FLAG,
        1,
        0,
        UserPtr::from(0usize),
        0,
    )
    .expect_err("interrupted untimed wait must not succeed");
    ktask::current().clear_interrupt();

    assert_eq!(LinuxError::from(err), LinuxError::ERESTARTSYS);
    assert!(take_futex_restart().is_none());
}

#[def_test(user, serial)]
fn test_timed_futex_wait_interrupt_returns_restartblock() {
    reset_restart_state();
    let word = TestUserValue::<u32>::from_value(1).unwrap();
    let timeout = TestUserValue::from_value(timespec {
        tv_sec: 30,
        tv_nsec: 0,
    })
    .unwrap();

    ktask::current().interrupt();
    let err = sys_futex(
        UserPtr::from(word.as_user_ptr()),
        FUTEX_WAIT | FUTEX_PRIVATE_FLAG,
        1,
        timeout.as_user_ptr() as usize,
        UserPtr::from(0usize),
        0,
    )
    .expect_err("interrupted timed wait must not succeed");
    ktask::current().clear_interrupt();

    assert_eq!(LinuxError::from(err), LinuxError::ERESTART_RESTARTBLOCK);
    assert!(take_futex_restart().is_some());
}

#[def_test(user, serial)]
fn test_timed_wait_bitset_interrupt_returns_restartblock() {
    reset_restart_state();
    let word = TestUserValue::<u32>::from_value(1).unwrap();
    let now = khal::time::monotonic_time().span_since_origin();
    let timeout = TestUserValue::from_value(timespec {
        tv_sec: (now.as_secs() + 30) as _,
        tv_nsec: 0,
    })
    .unwrap();

    ktask::current().interrupt();
    let err = sys_futex(
        UserPtr::from(word.as_user_ptr()),
        FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG,
        1,
        timeout.as_user_ptr() as usize,
        UserPtr::from(0usize),
        WAIT_BITSET_MASK,
    )
    .expect_err("interrupted WAIT_BITSET must not succeed");
    ktask::current().clear_interrupt();

    assert_eq!(LinuxError::from(err), LinuxError::ERESTART_RESTARTBLOCK);
    let stored = take_futex_restart().expect("timed WAIT_BITSET must arm a restart-block");
    assert_eq!(stored.bitset(), WAIT_BITSET_MASK);
}

#[def_test(user, serial)]
fn test_wait_bitset_past_absolute_deadline_returns_etimedout() {
    reset_restart_state();
    let word = TestUserValue::<u32>::from_value(1).unwrap();
    let timeout = TestUserValue::from_value(timespec {
        tv_sec: 0,
        tv_nsec: 0,
    })
    .unwrap();

    let err = sys_futex(
        UserPtr::from(word.as_user_ptr()),
        FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG,
        1,
        timeout.as_user_ptr() as usize,
        UserPtr::from(0usize),
        WAIT_BITSET_MASK,
    )
    .expect_err("past WAIT_BITSET deadline must time out");
    assert_eq!(LinuxError::from(err), LinuxError::ETIMEDOUT);
}

#[def_test(user, serial)]
fn test_restart_syscall_expired_futex_wait_returns_etimedout() {
    reset_restart_state();
    let word = TestUserValue::<u32>::from_value(1).unwrap();
    arm_futex_wait_restart(
        word.as_user_ptr() as usize,
        1,
        u32::MAX,
        true,
        MonotonicInstant::ORIGIN,
    );

    let err = sys_restart_syscall().expect_err("expired restart must time out");
    assert_eq!(LinuxError::from(err), LinuxError::ETIMEDOUT);
    let again = sys_restart_syscall().expect_err("consumed block must be EINTR");
    assert_eq!(LinuxError::from(again), LinuxError::EINTR);
}

#[def_test(user, serial)]
fn test_restart_syscall_expired_wait_bitset_returns_etimedout() {
    reset_restart_state();
    let word = TestUserValue::<u32>::from_value(1).unwrap();
    arm_futex_wait_restart(
        word.as_user_ptr() as usize,
        1,
        WAIT_BITSET_MASK,
        true,
        MonotonicInstant::ORIGIN,
    );

    let err = sys_restart_syscall().expect_err("expired WAIT_BITSET restart must time out");
    assert_eq!(LinuxError::from(err), LinuxError::ETIMEDOUT);
}

#[def_test(user, serial)]
fn test_resume_futex_wait_preserves_deadline_across_interrupt() {
    reset_restart_state();
    let word = TestUserValue::<u32>::from_value(1).unwrap();
    let deadline = khal::time::monotonic_time()
        .checked_add(TimeSpan::from_secs(30))
        .expect("monotonic deadline must be representable");
    arm_futex_wait_restart(word.as_user_ptr() as usize, 1, u32::MAX, true, deadline);

    ktask::current().interrupt();
    let err = sys_restart_syscall().expect_err("interrupted resume must not succeed");
    ktask::current().clear_interrupt();

    assert_eq!(LinuxError::from(err), LinuxError::ERESTART_RESTARTBLOCK);
    let stored = take_futex_restart().expect("interrupted resume must re-arm the same deadline");
    assert_eq!(stored.deadline(), deadline);
}

fn wake_op_add_one(addr: usize) -> kerrno::KResult<isize> {
    let ptr = UserPtr::from(addr as *mut u32);
    // ADD 1, then compare the old value with 0. With no waiters queued, success
    // returns 0 and leaves `*addr + 1`.
    let operation = (FUTEX_OP_ADD << 28) | (FUTEX_OP_CMP_EQ << 24) | (1 << 12);
    sys_futex(
        ptr,
        FUTEX_WAKE_OP | FUTEX_PRIVATE_FLAG,
        0,
        0,
        ptr,
        operation,
    )
}

fn map_private_anonymous_word(value: u32) -> usize {
    let page = posix_mm::sys_mmap(
        0,
        khal::mem::PAGE_SIZE_4K,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    )
    .expect("private anonymous page") as usize;
    let (stored, observed) =
        kuaccess::atomic_cmpxchg_u32(page, 0, value).expect("first write must fault the page in");
    assert!(
        stored && observed == 0,
        "fresh anonymous word must start at zero"
    );
    page
}

fn pte_flags(addr: usize) -> MappingFlags {
    let aspace = current_user_process_address_space();
    let mm = aspace.lock();
    let (_paddr, flags, _size) = mm
        .page_table()
        .query(VirtAddr::from_usize(addr))
        .expect("futex word must be present");
    flags
}

/// `FUTEX_WAKE_OP` must update a private anonymous word after `fork` has made
/// its PTE write-protected. The VMA still allows the store.
#[def_test(user, serial)]
fn test_wake_op_updates_word_on_fork_cow_page() {
    let page = map_private_anonymous_word(1);
    let writable = wake_op_add_one(page).expect("writable FUTEX_WAKE_OP");
    assert_eq!(writable, 0);
    assert_eq!(kuaccess::atomic_load_u32(page).expect("read back"), 2);
    let (reset, old) = kuaccess::atomic_cmpxchg_u32(page, 2, 1).expect("reset word");
    assert!(reset);
    assert_eq!(old, 2);

    // The cloned MM keeps the anonymous page shared, so the parent's PTE stays
    // write-protected until a write fault copies it.
    let cow_share = current_user_process_address_space()
        .lock()
        .try_clone()
        .expect("fork must clone the private anonymous mapping");
    let aspace = current_user_process_address_space();
    let mm = aspace.lock();
    let vma_flags = mm
        .find_vma(VirtAddr::from_usize(page))
        .expect("anonymous mapping must survive fork")
        .flags();
    drop(mm);
    assert!(vma_flags.contains(MappingFlags::READ | MappingFlags::WRITE));
    let after_fork = pte_flags(page);
    assert!(after_fork.contains(MappingFlags::READ));
    assert!(!after_fork.contains(MappingFlags::WRITE));

    let woken = wake_op_add_one(page).expect("COW FUTEX_WAKE_OP must fault in the write");
    assert_eq!(woken, 0);
    assert_eq!(
        kuaccess::atomic_load_u32(page).expect("word after COW wake_op"),
        2
    );
    drop(cow_share);
}

/// An invalid `uaddr2` is reported before an illegal `value3` is decoded.
#[def_test(user, serial)]
fn test_wake_op_invalid_address_precedes_illegal_encoding() {
    let word = TestUserValue::<u32>::from_value(1).unwrap();
    // Operation field 7 is not a defined `FUTEX_OP_*` value.
    let unsupported = 7 << 28;
    // Address 0 is 4-byte aligned and below `USER_SPACE_BASE` (`0x1000`), so
    // key resolution reports `EFAULT` rather than the alignment `EINVAL`.
    let err = sys_futex(
        UserPtr::from(word.as_user_ptr()),
        FUTEX_WAKE_OP | FUTEX_PRIVATE_FLAG,
        0,
        0,
        UserPtr::from(0usize),
        unsupported,
    )
    .expect_err("invalid uaddr2 must fail before decoding value3");
    assert_eq!(LinuxError::from(err), LinuxError::EFAULT);

    let err = sys_futex(
        UserPtr::from(word.as_user_ptr()),
        FUTEX_WAKE_OP | FUTEX_PRIVATE_FLAG,
        0,
        0,
        UserPtr::from(word.as_user_ptr()),
        unsupported,
    )
    .expect_err("a valid address still rejects an undefined operation");
    assert_eq!(LinuxError::from(err), LinuxError::ENOSYS);
}

/// A mapping the VMA does not allow to be written stays `EFAULT` and unchanged.
#[def_test(user, serial)]
fn test_wake_op_readonly_mapping_returns_efault() {
    let page = map_private_anonymous_word(1);
    posix_mm::sys_mprotect(page, khal::mem::PAGE_SIZE_4K, PROT_READ).expect("mprotect read-only");

    let err = wake_op_add_one(page).expect_err("read-only FUTEX_WAKE_OP");
    assert_eq!(LinuxError::from(err), LinuxError::EFAULT);
    assert_eq!(
        kuaccess::atomic_load_u32(page).expect("word after EFAULT"),
        1
    );
}

// x86 publishes syscall-origin state through `set_sysno`. Other architectures
// keep that flag private to trap entry, so these tests do not add an arch API.
#[cfg(target_arch = "x86_64")]
mod user_return_restart {
    use khal::uspace::UserContext;
    use ksignal::{
        SignalAction, SignalActionFlags, SignalDisposition, SignalInfo, SignalSet, Signo,
    };
    use linux_sysno::Sysno;
    use unittest::{assert_eq, def_test};
    use unittest_support::{TestUserBuffer, TestUserValue};

    use super::{
        LinuxError, MonotonicInstant, arm_futex_wait_restart, current_user_thread,
        reset_restart_state, restart_errno, take_futex_restart,
    };

    /// Keeps the mapped stack alive for the signal frame written during delivery.
    fn syscall_context(errno: LinuxError) -> (TestUserBuffer, UserContext) {
        let stack = TestUserBuffer::new(16 * 1024).expect("restart test stack");
        let sp = stack.as_user_ptr::<u8>() as usize + stack.len();
        let mut uctx = UserContext::new(0x4000, sp.into(), 0);
        uctx.save_syscall_args();
        uctx.set_sysno(Sysno::futex as usize);
        uctx.set_retval(restart_errno(errno));
        (stack, uctx)
    }

    struct SignalActionGuard {
        signo: Signo,
        blocked: SignalSet,
    }

    impl SignalActionGuard {
        fn install(signo: Signo, action: SignalAction) -> Self {
            let thread = current_user_thread();
            let blocked = thread.signal_manager().blocked();
            thread
                .process()
                .signal_manager()
                .expect("user test thread must have a signal manager")
                .set_signal_action(signo, action)
                .expect("user test thread must still own signal actions");
            Self { signo, blocked }
        }
    }

    impl Drop for SignalActionGuard {
        fn drop(&mut self) {
            let thread = current_user_thread();
            if let Ok(signals) = thread.process().signal_manager() {
                // Drop restores the previous action best-effort. The process
                // may already have released signal configuration.
                let _ = signals.set_signal_action(self.signo, SignalAction::default());
            }
            thread.signal_manager().set_blocked(self.blocked);
        }
    }

    extern "C" fn restart_test_handler(_: i32) {}

    #[def_test(user, serial)]
    fn test_default_sigcont_rewrites_restartblock_without_dropping_slot() {
        reset_restart_state();
        let word = TestUserValue::<u32>::from_value(1).unwrap();
        let uaddr = word.as_user_ptr() as usize;
        arm_futex_wait_restart(uaddr, 1, u32::MAX, true, MonotonicInstant::ORIGIN);

        let signal = current_user_thread().signal_manager().clone();
        let old_blocked = signal.blocked();
        let mut blocked = old_blocked;
        blocked.add(Signo::SIGCONT);
        signal.set_blocked(blocked);
        assert!(!signal.send_signal(SignalInfo::new_kernel(Signo::SIGCONT)));
        signal.set_blocked(old_blocked);

        assert!(signal.pending().has(Signo::SIGCONT));
        let (stack, mut uctx) = syscall_context(LinuxError::ERESTART_RESTARTBLOCK);
        let ip = uctx.ip();
        let thread = current_user_thread();
        posix_process::replay_user_return_restart_for_test(&thread, &mut uctx, false);
        drop(stack);

        assert!(!signal.pending().has(Signo::SIGCONT));
        assert!(uctx.syscall_restart_error().is_none());
        assert!(uctx.ip() != ip);
        assert_eq!(uctx.retval(), Sysno::restart_syscall as usize);
        let stored = take_futex_restart().expect("SIGCONT must not drop the armed deadline");
        assert_eq!(stored.uaddr(), uaddr);
    }

    #[def_test(user, serial)]
    fn test_handler_turns_restartblock_into_eintr_and_clears_slot() {
        reset_restart_state();
        let word = TestUserValue::<u32>::from_value(1).unwrap();
        arm_futex_wait_restart(
            word.as_user_ptr() as usize,
            1,
            u32::MAX,
            true,
            MonotonicInstant::ORIGIN,
        );
        let _guard = SignalActionGuard::install(
            Signo::SIGUSR1,
            SignalAction {
                disposition: SignalDisposition::Handler(restart_test_handler),
                ..SignalAction::default()
            },
        );
        assert!(
            current_user_thread()
                .signal_manager()
                .send_signal(SignalInfo::new_kernel(Signo::SIGUSR1))
        );

        let (stack, mut uctx) = syscall_context(LinuxError::ERESTART_RESTARTBLOCK);
        let thread = current_user_thread();
        posix_process::replay_user_return_restart_for_test(&thread, &mut uctx, false);
        // The handler entry overwrites the return register with the signal
        // number. The interrupted syscall's `-EINTR` is the value saved in the
        // signal frame, which `sigreturn` restores.
        assert!(stack_contains_errno(&stack, LinuxError::EINTR));
        assert!(!stack_contains_errno(
            &stack,
            LinuxError::ERESTART_RESTARTBLOCK
        ));
        assert_eq!(uctx.ip(), restart_test_handler as *const () as usize);
        drop(stack);

        assert!(take_futex_restart().is_none());
    }

    #[def_test(user, serial)]
    fn test_handler_queued_between_scans_still_drops_slot() {
        reset_restart_state();
        let word = TestUserValue::<u32>::from_value(1).unwrap();
        arm_futex_wait_restart(
            word.as_user_ptr() as usize,
            1,
            u32::MAX,
            true,
            MonotonicInstant::ORIGIN,
        );
        let _guard = SignalActionGuard::install(
            Signo::SIGUSR1,
            SignalAction {
                disposition: SignalDisposition::Handler(restart_test_handler),
                ..SignalAction::default()
            },
        );
        let signal = current_user_thread().signal_manager().clone();

        let (stack, mut uctx) = syscall_context(LinuxError::ERESTART_RESTARTBLOCK);
        let thread = current_user_thread();
        posix_process::replay_user_return_restart_with_gap_for_test(
            &thread,
            &mut uctx,
            false,
            || {
                assert!(signal.send_signal(SignalInfo::new_kernel(Signo::SIGUSR1)));
                ktask::current().interrupt();
            },
        );
        assert!(stack_contains_errno(&stack, LinuxError::EINTR));
        assert!(!stack_contains_errno(
            &stack,
            LinuxError::ERESTART_RESTARTBLOCK
        ));
        assert_eq!(uctx.ip(), restart_test_handler as *const () as usize);
        drop(stack);

        assert!(take_futex_restart().is_none());
    }

    #[def_test(user, serial)]
    fn test_sa_restart_rolls_back_restartsys() {
        reset_restart_state();
        let _guard = SignalActionGuard::install(
            Signo::SIGUSR2,
            SignalAction {
                disposition: SignalDisposition::Handler(restart_test_handler),
                flags: SignalActionFlags::RESTART,
                ..SignalAction::default()
            },
        );
        assert!(
            current_user_thread()
                .signal_manager()
                .send_signal(SignalInfo::new_kernel(Signo::SIGUSR2))
        );

        let (stack, mut uctx) = syscall_context(LinuxError::ERESTARTSYS);
        let thread = current_user_thread();
        posix_process::replay_user_return_restart_for_test(&thread, &mut uctx, false);
        assert!(!stack_contains_errno(&stack, LinuxError::ERESTARTSYS));
        assert_eq!(uctx.ip(), restart_test_handler as *const () as usize);
        drop(stack);
    }

    fn stack_contains_errno(stack: &TestUserBuffer, err: LinuxError) -> bool {
        let needle = restart_errno(err).to_le_bytes();
        let bytes = stack
            .read_bytes(stack.len())
            .expect("read restart test stack");
        bytes.windows(needle.len()).any(|window| window == needle)
    }
}
