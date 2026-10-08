// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Futex and robust-list syscall adapters.

use core::mem::size_of;

use kerrno::{KError, KResult, LinuxError};
use kfutex::{FutexKey, FutexWakeOp, global_table};
use kprocess::{
    AsThread, current_user_mm_id, current_user_process_address_space, current_user_thread,
};
use ktime_types::{MonotonicInstant, TimeSpan};
use linux_raw_sys::general::{
    FUTEX_CLOCK_REALTIME, FUTEX_CMD_MASK, FUTEX_CMP_REQUEUE, FUTEX_PRIVATE_FLAG, FUTEX_REQUEUE,
    FUTEX_WAIT, FUTEX_WAIT_BITSET, FUTEX_WAKE, FUTEX_WAKE_BITSET, FUTEX_WAKE_OP, robust_list_head,
    timespec,
};
use osvm::VirtPtr;
use posix_types::{TimeSpanLike, UserConstPtr, UserPtr};

/// Converts a wake-style count from the syscall `u32` ABI.
///
/// Linux passes this as `int nr_wake`. A value that is negative as `i32`
/// must not yield `EINVAL` for `FUTEX_WAKE` / `FUTEX_WAKE_OP`; clamp to 0
/// so the call succeeds without waking anyone. (`futex_requeue` is
/// different — see [`validate_requeue_count`].)
fn as_wake_count(value: u32) -> usize {
    match value as i32 {
        n if n <= 0 => 0,
        n => n as usize,
    }
}

/// Validates a requeue count against Linux `futex_requeue` ABI.
///
/// Linux rejects negative `nr_wake` / `nr_requeue` with `EINVAL`.
fn validate_requeue_count(value: u32) -> KResult<usize> {
    match value as i32 {
        n if n < 0 => Err(KError::InvalidInput),
        n => Ok(n as usize),
    }
}

/// Observes the actual private syscall resolver for the current user image.
#[cfg(unittest)]
pub(crate) fn observe_private_futex_key(address: usize) -> KResult<FutexKey> {
    resolve_key(address, true)
}

fn resolve_key(address: usize, is_private: bool) -> KResult<FutexKey> {
    if is_private {
        // Private keys need only the bound MM identity and VA, not the
        // mmap/munmap mutex. The binding lock selects the identity: an MM's
        // identity is immutable, but exec replaces the process's bound MM.
        return FutexKey::resolve_private(current_user_mm_id(), address);
    }
    let address_space = current_user_process_address_space();
    let address_space = address_space.lock();
    FutexKey::resolve(&address_space, address, false)
}

/// Resolves two futex keys under one address-space lock when needed.
///
/// Shared (non-private) compound operations must not resolve `uaddr` and
/// `uaddr2` separately; a concurrent `mmap`/`munmap` could otherwise observe
/// inconsistent backing metadata within a single syscall. Private keys skip
/// that MM mutex, but capture one identity under the process binding lock for
/// both keys. Exec changes the binding, not an existing MM's identity.
fn resolve_key_pair(
    first: usize,
    second: usize,
    is_private: bool,
) -> KResult<(FutexKey, FutexKey)> {
    if is_private {
        let mm_id = current_user_mm_id();
        let first_key = FutexKey::resolve_private(mm_id, first)?;
        let second_key = FutexKey::resolve_private(mm_id, second)?;
        return Ok((first_key, second_key));
    }
    let address_space = current_user_process_address_space();
    let address_space = address_space.lock();
    let first_key = FutexKey::resolve(&address_space, first, false)?;
    let second_key = FutexKey::resolve(&address_space, second, false)?;
    Ok((first_key, second_key))
}

fn parse_timeout(
    command: u32,
    is_realtime: bool,
    timeout_address: usize,
) -> KResult<Option<TimeSpan>> {
    let Some(timeout) = UserConstPtr::<timespec>::from(timeout_address).check_non_null() else {
        return Ok(None);
    };
    let timeout = timeout.read_vm()?;
    // `FUTEX_WAIT` is always relative; `FUTEX_CLOCK_REALTIME` only selects the
    // clock used to measure that relative interval. `FUTEX_WAIT_BITSET` is
    // always an absolute deadline on the selected clock.
    Ok(Some(match command {
        FUTEX_WAIT => timeout.try_into_time_span()?,
        FUTEX_WAIT_BITSET => {
            if is_realtime {
                posix_types::try_into_realtime_deadline(timeout)?
                    .duration_since(ktime::realtime())
                    .unwrap_or(TimeSpan::ZERO)
            } else {
                timeout
                    .try_into_time_span()?
                    .saturating_sub(khal::time::monotonic_time().span_since_origin())
            }
        }
        _ => return Err(KError::InvalidInput),
    }))
}

/// Fast userspace mutex (futex) system call.
///
/// Linux semantics follow `futex(2)`.
///
/// A blocking `FUTEX_WAIT` / `FUTEX_WAIT_BITSET` interrupted by a signal
/// returns a kernel restart code here, not a userspace `EINTR`:
///
/// - no timeout: [`LinuxError::ERESTARTSYS`], so `SA_RESTART` may replay the
///   original syscall;
/// - with a timeout: [`LinuxError::ERESTART_RESTARTBLOCK`], with the absolute
///   monotonic deadline stored on the current thread for `restart_syscall(2)`.
///
/// When a handler runs, `ERESTART_RESTARTBLOCK` becomes `EINTR` and the
/// restart-block is dropped, so a later `restart_syscall` cannot resume that
/// wait. `SA_RESTART` does not restart a timed futex wait. When no handler is
/// delivered, the user-return path rewrites the syscall number to
/// `restart_syscall` and leaves the block armed. Default `SIGCONT` is the
/// third case: it is dequeued as `SignalOSAction::Continue`, not as a
/// handler, so it neither converts this code to `EINTR` nor suppresses the
/// rewrite to `restart_syscall`.
pub fn sys_futex(
    uaddr: UserPtr<u32>,
    futex_op: u32,
    value: u32,
    timeout_or_value2: usize,
    uaddr2: UserPtr<u32>,
    value3: u32,
) -> KResult<isize> {
    debug!(
        "sys_futex <= uaddr: {:?}, futex_op: {futex_op}, value: {value}, uaddr2: {:?}, value3: \
         {value3}",
        uaddr.as_ptr(),
        uaddr2.as_ptr(),
    );

    let command = futex_op & (FUTEX_CMD_MASK as u32);
    let is_private = futex_op & FUTEX_PRIVATE_FLAG != 0;
    let is_realtime = futex_op & FUTEX_CLOCK_REALTIME != 0;
    if is_realtime && !matches!(command, FUTEX_WAIT | FUTEX_WAIT_BITSET) {
        return Err(KError::Unsupported);
    }

    let uaddr_usize = uaddr.as_ptr() as usize;
    match command {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let bitset = if command == FUTEX_WAIT_BITSET {
                if value3 == 0 {
                    return Err(KError::InvalidInput);
                }
                value3
            } else {
                u32::MAX
            };
            let timeout = parse_timeout(command, is_realtime, timeout_or_value2)?;
            wait_futex(uaddr_usize, value, bitset, is_private, timeout, None)
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            let key = resolve_key(uaddr_usize, is_private)?;
            let count = as_wake_count(value);
            let bitset = if command == FUTEX_WAKE_BITSET {
                if value3 == 0 {
                    return Err(KError::InvalidInput);
                }
                value3
            } else {
                u32::MAX
            };
            Ok(global_table().wake(key, count, bitset) as isize)
        }
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            let uaddr2_usize = uaddr2.as_ptr() as usize;
            let (key, key2) = resolve_key_pair(uaddr_usize, uaddr2_usize, is_private)?;
            let wake_count = validate_requeue_count(value)?;
            let requeue_count = validate_requeue_count(timeout_or_value2 as u32)?;
            let compare = (command == FUTEX_CMP_REQUEUE).then_some((uaddr_usize, value3));
            global_table()
                .requeue(key, key2, wake_count, requeue_count, compare)
                .map(|count| count as isize)
        }
        FUTEX_WAKE_OP => {
            let uaddr2_usize = uaddr2.as_ptr() as usize;
            let (key, key2) = resolve_key_pair(uaddr_usize, uaddr2_usize, is_private)?;
            let source_count = as_wake_count(value);
            let target_count = as_wake_count(timeout_or_value2 as u32);
            let operation = FutexWakeOp::decode(value3)?;
            global_table()
                .wake_op(
                    key,
                    key2,
                    uaddr2_usize,
                    source_count,
                    target_count,
                    operation,
                )
                .map(|count| count as isize)
        }
        _ => Err(KError::Unsupported),
    }
}

/// Kernel-owned arguments needed to resume a timed futex wait.
///
/// `FUTEX_WAIT` stores `u32::MAX` in `bitset`. The value lives only in this
/// crate; `kprocess` stores it in an opaque per-thread slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FutexWaitRestartBlock {
    uaddr: usize,
    expected: u32,
    bitset: u32,
    is_private: bool,
    deadline: MonotonicInstant,
}

impl FutexWaitRestartBlock {
    const fn new(
        uaddr: usize,
        expected: u32,
        bitset: u32,
        is_private: bool,
        deadline: MonotonicInstant,
    ) -> Self {
        Self {
            uaddr,
            expected,
            bitset,
            is_private,
            deadline,
        }
    }

    /// User virtual address of the futex word.
    pub(crate) const fn uaddr(self) -> usize {
        self.uaddr
    }

    /// Expected value compared before enqueueing.
    pub(crate) const fn expected(self) -> u32 {
        self.expected
    }

    /// Wait bitset. `FUTEX_WAIT` uses `u32::MAX`.
    pub(crate) const fn bitset(self) -> u32 {
        self.bitset
    }

    /// Whether the original operation used `FUTEX_PRIVATE_FLAG`.
    pub(crate) const fn is_private(self) -> bool {
        self.is_private
    }

    /// Absolute monotonic deadline captured when the wait was first armed.
    pub(crate) const fn deadline(self) -> MonotonicInstant {
        self.deadline
    }

    /// Remaining wait at `now`.
    ///
    /// A deadline already in the past yields [`TimeSpan::ZERO`], so the
    /// restarted wait returns `ETIMEDOUT` instead of sleeping again.
    const fn remaining_timeout(self, now: MonotonicInstant) -> TimeSpan {
        self.deadline.saturating_duration_since(now)
    }
}

fn arm_futex_wait_restart(
    uaddr: usize,
    expected: u32,
    bitset: u32,
    is_private: bool,
    deadline: MonotonicInstant,
) {
    current_user_thread().arm_restart_block(FutexWaitRestartBlock::new(
        uaddr, expected, bitset, is_private, deadline,
    ));
}

/// Resumes a timed futex wait from `sys_restart_syscall`.
///
/// Remaining time is recomputed from the stored monotonic deadline. A later
/// interrupt arms that same deadline again, so the original interval is not
/// extended.
pub(crate) fn resume_futex_wait(wait: FutexWaitRestartBlock) -> KResult<isize> {
    let remaining = wait.remaining_timeout(khal::time::monotonic_time());
    wait_futex(
        wait.uaddr(),
        wait.expected(),
        wait.bitset(),
        wait.is_private(),
        Some(remaining),
        Some(wait.deadline()),
    )
}

fn wait_futex(
    uaddr: usize,
    expected: u32,
    bitset: u32,
    is_private: bool,
    timeout: Option<TimeSpan>,
    stored_deadline: Option<MonotonicInstant>,
) -> KResult<isize> {
    let deadline = stored_deadline.or_else(|| {
        timeout.map(|span| {
            khal::time::monotonic_time()
                .checked_add(span)
                .unwrap_or_else(|| MonotonicInstant::from_span_since_origin(TimeSpan::MAX))
        })
    });
    let key = resolve_key(uaddr, is_private)?;
    match global_table().wait(key, uaddr, expected, bitset, timeout) {
        Ok(false) => Err(KError::WouldBlock),
        Ok(true) => Ok(0),
        Err(err) if LinuxError::from(err) == LinuxError::EINTR => {
            if let Some(deadline) = deadline {
                arm_futex_wait_restart(uaddr, expected, bitset, is_private, deadline);
                Err(KError::from(LinuxError::ERESTART_RESTARTBLOCK))
            } else {
                Err(KError::from(LinuxError::ERESTARTSYS))
            }
        }
        Err(err) => Err(err),
    }
}

/// Returns the robust-list head of the selected thread.
pub fn sys_get_robust_list(
    tid: u32,
    head: UserPtr<*const robust_list_head>,
    size: UserPtr<usize>,
) -> KResult<isize> {
    let task = kprocess::pidfd::robust_list_task(tid)?;
    let current = kprocess::current_user_thread();
    let target = task.as_thread();
    kprocess::ptrace::check_read_real_creds_access(&current, target)?;
    head.write_vm(target.robust_list_head() as _)?;
    size.write_vm(size_of::<robust_list_head>())?;
    Ok(0)
}

/// Sets the robust-list head for the current thread.
pub fn sys_set_robust_list(head: UserConstPtr<robust_list_head>, size: usize) -> KResult<isize> {
    if size != size_of::<robust_list_head>() {
        return Err(KError::InvalidInput);
    }
    kprocess::current_user_thread().set_robust_list_head(head.as_ptr() as usize);
    Ok(0)
}

#[cfg(unittest)]
mod tests {
    use kerrno::LinuxError;
    use kprocess::current_user_thread;
    use ktime_types::{MonotonicInstant, TimeSpan};
    use linux_raw_sys::general::{FUTEX_PRIVATE_FLAG, FUTEX_WAIT, FUTEX_WAIT_BITSET, timespec};
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
        let stored =
            take_futex_restart().expect("interrupted resume must re-arm the same deadline");
        assert_eq!(stored.deadline(), deadline);
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
}
