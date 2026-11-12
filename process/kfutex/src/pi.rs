// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel PI ownership indexed separately from ordinary futex waiters.

use alloc::{collections::BTreeMap, sync::Arc};

use kerrno::{KError, KResult};
use ktask::{
    KtaskRef,
    future::{self, block_on, interruptible},
};
use ktime_types::TimeSpan;

use super::{FUTEX_TABLE, FutexTable, GlobalFutexTable};
use crate::FutexKey;

type PiLock = krtmutex::RawRtMutex<ktask::TaskPiHost>;
type PiIndex = (u8, u64, u64, usize);
pub(super) type PiStates = BTreeMap<PiIndex, PiState>;

struct UserPiOwner {
    word_tid: u32,
    owner_died: bool,
}

pub(super) struct PiState {
    lock: Arc<PiLock>,
    user: Option<UserPiOwner>,
}

#[path = "pi_user.rs"]
mod user;
pub use user::PiFutexContext;

impl FutexTable {
    fn pi_lock(&self, key: FutexKey, owner: Option<KtaskRef>) -> KResult<Arc<PiLock>> {
        let mut bucket = self.bucket(self.bucket_id(key)).state.lock();
        if bucket
            .waiters
            .iter()
            .any(|waiter| waiter.is_queued() && waiter.route_unlocked().0 == key)
        {
            return Err(KError::InvalidInput);
        }
        let state = bucket.pi.entry(key.pi_index()).or_insert_with(|| PiState {
            lock: Arc::new(PiLock::new(owner.clone())),
            user: None,
        });
        if state.user.is_some() {
            return Err(KError::InvalidInput);
        }
        Ok(state.lock.clone())
    }

    fn retire_pi(&self, key: FutexKey, pin: Arc<PiLock>) {
        let retired = {
            let mut bucket = self.bucket(self.bucket_id(key)).state.lock();
            // All lookups pin under this lock. Drop ours before checking so
            // concurrent final calls cannot both retain an unused entry.
            drop(pin);
            let can_retire = bucket.pi.get(&key.pi_index()).is_some_and(|state| {
                let is_unpinned = Arc::strong_count(&state.lock) == 1;
                if state.user.is_some() {
                    // Without an in-flight waiter, user ownership needs only
                    // its word. Later contenders can adopt that owner again.
                    is_unpinned || !state.lock.is_locked()
                } else {
                    is_unpinned && !state.lock.is_locked()
                }
            });
            can_retire.then(|| bucket.pi.remove(&key.pi_index()))
        };
        // A user entry may still own its raw lock; dropping it can submit a
        // deboost and must happen after releasing the bucket lock.
        drop(retired);
    }

    fn lock_pi(
        &self,
        key: FutexKey,
        owner: Option<KtaskRef>,
        timeout: Option<TimeSpan>,
    ) -> KResult<()> {
        let lock = self.pi_lock(key, owner)?;
        let result = match lock.begin_lock() {
            Err(error) => Err(error),
            Ok(mut wait) => block_on(async {
                let error = match interruptible(future::timeout(timeout, &mut wait)).await {
                    Ok(Ok(result)) => return result,
                    Ok(Err(elapsed)) => KError::from(elapsed),
                    Err(interrupted) => KError::from(interrupted),
                };
                // Cancellation may report an error only if it beat handoff.
                if wait.cancel() { Err(error) } else { Ok(()) }
            }),
        };
        self.retire_pi(key, lock);
        result
    }

    fn trylock_pi(&self, key: FutexKey, owner: Option<KtaskRef>) -> KResult<()> {
        let lock = self.pi_lock(key, owner)?;
        let result = lock.try_lock().and_then(|acquired| {
            if acquired {
                Ok(())
            } else {
                Err(KError::WouldBlock)
            }
        });
        self.retire_pi(key, lock);
        result
    }

    fn unlock_pi(&self, key: FutexKey, owner: &KtaskRef) -> KResult<()> {
        let lock = self
            .bucket(self.bucket_id(key))
            .state
            .lock()
            .pi
            .get(&key.pi_index())
            .filter(|state| state.user.is_none())
            .map(|state| state.lock.clone())
            .ok_or(KError::OperationNotPermitted)?;
        // Boost submission and wake delivery run after releasing the bucket.
        let result = lock.unlock(owner);
        self.retire_pi(key, lock);
        result
    }
}

impl GlobalFutexTable {
    /// Acquires kernel PI ownership for the current task in sleepable context.
    ///
    /// `owner` adopts an externally established, validated live owner only when
    /// this key has no PI state yet. Existing kernel ownership is authoritative.
    /// Success retains ownership until `unlock_pi`; no guard spans the call.
    /// This API performs no TID lookup, user-word access or owner-death recovery.
    /// The caller must release every owned key before its task exits.
    ///
    /// # Errors
    /// Returns `InvalidInput` for an ordinary waiter on the key, `Deadlock` for
    /// recursion/cycles, or `BadState` for a second concurrent PI wait.
    /// Timeout and signal return errors only when cancellation beats handoff.
    pub fn lock_pi(
        self,
        key: FutexKey,
        owner: Option<KtaskRef>,
        timeout: Option<TimeSpan>,
    ) -> KResult<()> {
        FUTEX_TABLE.lock_pi(key, owner, timeout)
    }

    /// Acquires kernel PI ownership without waiting, with the owner contract of `lock_pi`.
    ///
    /// Requires task context with allocation permitted and no scheduler locks.
    ///
    /// # Errors
    /// Returns `WouldBlock` for contention, `Deadlock` for recursion, or
    /// `InvalidInput` for an ordinary waiter on the key.
    pub fn trylock_pi(self, key: FutexKey, owner: Option<KtaskRef>) -> KResult<()> {
        FUTEX_TABLE.trylock_pi(key, owner)
    }

    /// Releases a validated owner and hands the key to its highest-priority waiter.
    ///
    /// Requires task context without scheduler locks. The explicit owner also
    /// permits host cleanup on its behalf; caller-side identity checks belong
    /// to the caller. Empty, unpinned PI entries are reclaimed.
    ///
    /// # Errors
    /// Returns `OperationNotPermitted` for an unknown key or mismatched owner.
    pub fn unlock_pi(self, key: FutexKey, owner: &KtaskRef) -> KResult<()> {
        FUTEX_TABLE.unlock_pi(key, owner)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/pi.rs");
