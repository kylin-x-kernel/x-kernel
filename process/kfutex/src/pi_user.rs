// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User-word PI transactions. Bucket -> graph is the only nested lock order.

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use ktask::{
    KtaskRef, current,
    future::{self, block_on, interruptible},
};
use ktime_types::TimeSpan;
use kuaccess::{
    atomic_cmpxchg_u32_nofault, atomic_load_u32, atomic_load_u32_nofault, fault_in_user_writeable,
};
use linux_raw_sys::general::{FUTEX_OWNER_DIED, FUTEX_TID_MASK, FUTEX_WAITERS};

use super::{
    super::{BUCKET_COUNT, FUTEX_TABLE, FutexTable, GlobalFutexTable},
    PiLock, PiState, UserPiOwner,
};
use crate::{FutexKey, waiter::BucketId};

/// Process-side identity and mapping services for user PI futex operations.
///
/// Called in sleepable user-task context. `owner_is_exiting` additionally runs
/// under a bucket lock: it must only read nonblocking task/exit metadata.
pub trait PiFutexContext {
    /// User virtual address of the naturally aligned futex word.
    fn address(&self) -> usize;
    /// Re-resolves private/shared identity after fault-in or remapping.
    fn resolve_key(&self) -> KResult<FutexKey>;
    /// Finds a live user task in the caller's TID domain; zero is never passed.
    fn lookup_owner(&self, tid: u32) -> KResult<KtaskRef>;
    /// Pairs with the exit mark published before the PI-state sweep.
    fn owner_is_exiting(&self, owner: &KtaskRef) -> bool;
}

fn tid(task: &KtaskRef) -> KResult<u32> {
    let id = u32::try_from(task.trace_id()).map_err(|_| KError::InvalidInput)?;
    if id == 0 || id > FUTEX_TID_MASK {
        return Err(KError::InvalidInput);
    }
    Ok(id)
}

fn exchange(address: usize, old: u32, new: u32) -> KResult<()> {
    if atomic_cmpxchg_u32_nofault(address, old, new)
        .map_err(KError::from)?
        .0
    {
        Ok(())
    } else {
        Err(KError::WouldBlock)
    }
}

fn word_matches(word: u32, owner: &UserPiOwner) -> bool {
    let value = word & FUTEX_TID_MASK;
    value == owner.word_tid || (value == 0 && word & FUTEX_OWNER_DIED != 0)
}

impl PiState {
    fn exit_owner(&mut self, task: &KtaskRef) -> KResult<krtmutex::PiEffects<ktask::TaskPiHost>> {
        self.lock.unlock_deferred(task, |_| {
            if let Some(owner) = self.user.as_mut() {
                owner.owner_died = true;
            }
            Ok(())
        })
    }
}

impl FutexTable {
    fn user_pi_pin(
        &self,
        context: &impl PiFutexContext,
        key: FutexKey,
        observed: u32,
        is_trylock: bool,
    ) -> KResult<Option<Arc<PiLock>>> {
        let caller = current().clone();
        let caller_tid = tid(&caller)?;
        let owner_tid = observed & FUTEX_TID_MASK;
        if owner_tid == caller_tid {
            return Err(KError::from(kerrno::KErrorKind::Deadlock));
        }
        // Registry lookup may sleep. Its result is used only after the nofault
        // value and absence of an existing PI state are rechecked below.
        let candidate = if owner_tid != 0 {
            context.lookup_owner(owner_tid)
        } else {
            Err(KError::NoSuchProcess)
        };
        let mut bucket = self.bucket(self.bucket_id(key)).state.lock();
        if bucket
            .waiters
            .iter()
            .any(|waiter| waiter.is_queued() && waiter.route_unlocked().0 == key)
        {
            return Err(KError::InvalidInput);
        }
        let word = atomic_load_u32_nofault(context.address()).map_err(KError::from)?;
        if word != observed {
            return Err(KError::WouldBlock);
        }
        if let Some(state) = bucket.pi.get(&key.pi_index()) {
            let user = state.user.as_ref().ok_or(KError::InvalidInput)?;
            if !word_matches(word, user) {
                return Err(KError::InvalidInput);
            }
            if state.lock.is_locked() {
                return Ok(Some(state.lock.clone()));
            }
            // User callers revalidate the map identity before enqueueing, so
            // a free user entry may be detached even with lookups in flight.
            bucket.pi.remove(&key.pi_index());
        }
        if owner_tid == 0 {
            exchange(
                context.address(),
                word,
                caller_tid | (word & FUTEX_OWNER_DIED),
            )?;
            return Ok(None);
        }
        let owner = candidate?;
        if tid(&owner)? != owner_tid {
            return Err(KError::NoSuchProcess);
        }
        if context.owner_is_exiting(&owner) {
            return Err(KError::NoSuchProcess);
        }
        if is_trylock {
            return Err(KError::WouldBlock);
        }
        // WAITERS prevents an owner from taking the userspace unlock fast path
        // while a contender holds the published state but has not enqueued yet.
        exchange(context.address(), word, word | FUTEX_WAITERS)?;
        let lock = Arc::new(PiLock::new(Some(owner.clone())));
        bucket.pi.insert(
            key.pi_index(),
            PiState {
                lock: lock.clone(),
                user: Some(UserPiOwner {
                    word_tid: owner_tid,
                    owner_died: word & FUTEX_OWNER_DIED != 0,
                }),
            },
        );
        Ok(Some(lock))
    }

    fn user_pi_lock(
        &self,
        context: &impl PiFutexContext,
        timeout: Option<TimeSpan>,
        is_trylock: bool,
    ) -> KResult<()> {
        context.resolve_key()?;
        let start = khal::time::monotonic_time();
        loop {
            let observed = atomic_load_u32(context.address()).map_err(KError::from)?;
            let key = context.resolve_key()?;
            let attempt = self.user_pi_attempt(
                context,
                key,
                observed,
                timeout.map(|span| {
                    span.saturating_sub(
                        khal::time::monotonic_time().saturating_duration_since(start),
                    )
                }),
                is_trylock,
            );
            match attempt {
                Err(error) if error == KError::BadAddress => {
                    fault_in_user_writeable(context.address()).map_err(KError::from)?
                }
                Err(error) if error == KError::WouldBlock && !is_trylock => ktask::yield_now(),
                result => return result,
            }
            if timeout.is_some_and(|span| {
                khal::time::monotonic_time().saturating_duration_since(start) >= span
            }) {
                return Err(KError::TimedOut);
            }
        }
    }

    fn user_pi_attempt(
        &self,
        context: &impl PiFutexContext,
        key: FutexKey,
        observed: u32,
        timeout: Option<TimeSpan>,
        is_trylock: bool,
    ) -> KResult<()> {
        let attempt_start = khal::time::monotonic_time();
        let Some(lock) = self.user_pi_pin(context, key, observed, is_trylock)? else {
            return Ok(());
        };
        let result = (|| {
            let (mut wait, effects) = {
                let mut bucket = self.bucket(self.bucket_id(key)).state.lock();
                let state = bucket
                    .pi
                    .get_mut(&key.pi_index())
                    .filter(|state| Arc::ptr_eq(&state.lock, &lock))
                    .ok_or(KError::WouldBlock)?;
                let owner = state.user.as_ref().ok_or(KError::InvalidInput)?;
                let word = atomic_load_u32_nofault(context.address()).map_err(KError::from)?;
                if !word_matches(word, owner) {
                    return Err(KError::InvalidInput);
                }
                if !lock.is_locked() {
                    return Err(KError::WouldBlock);
                }
                if is_trylock {
                    return Err(KError::WouldBlock);
                }
                exchange(context.address(), word, word | FUTEX_WAITERS)?;
                lock.begin_lock_deferred()?
            };
            drop(effects);
            let timeout = timeout.map(|span| {
                span.saturating_sub(
                    khal::time::monotonic_time().saturating_duration_since(attempt_start),
                )
            });
            let acquired = block_on(async {
                let error = match interruptible(future::timeout(timeout, &mut wait)).await {
                    Ok(Ok(result)) => return result,
                    Ok(Err(elapsed)) => KError::from(elapsed),
                    Err(interrupted) => KError::from(interrupted),
                };
                if wait.cancel() { Err(error) } else { Ok(()) }
            });
            acquired?;
            if let Err(error) = self.fixup_user_owner(context, key, &lock) {
                self.abandon_user_owner(key, &lock);
                return Err(error);
            }
            Ok(())
        })();
        self.retire_pi(key, lock);
        result
    }

    fn fixup_user_owner(
        &self,
        context: &impl PiFutexContext,
        key: FutexKey,
        lock: &Arc<PiLock>,
    ) -> KResult<()> {
        let caller = current().clone();
        let caller_tid = tid(&caller)?;
        loop {
            if context.resolve_key()? != key {
                return Err(KError::BadAddress);
            }
            let result = (|| {
                let mut bucket = self.bucket(self.bucket_id(key)).state.lock();
                let state = bucket
                    .pi
                    .get_mut(&key.pi_index())
                    .filter(|state| Arc::ptr_eq(&state.lock, lock))
                    .ok_or(KError::InvalidInput)?;
                let owner = state.user.as_mut().ok_or(KError::InvalidInput)?;
                if !lock.is_owned_by(&caller) {
                    return Err(KError::OperationNotPermitted);
                }
                let word = atomic_load_u32_nofault(context.address()).map_err(KError::from)?;
                if !word_matches(word, owner) {
                    return Err(KError::InvalidInput);
                }
                let died = owner.owner_died || word & FUTEX_OWNER_DIED != 0;
                exchange(
                    context.address(),
                    word,
                    caller_tid | FUTEX_WAITERS | if died { FUTEX_OWNER_DIED } else { 0 },
                )?;
                owner.word_tid = caller_tid;
                owner.owner_died = died;
                Ok(())
            })();
            match result {
                Err(error) if error == KError::BadAddress => {
                    fault_in_user_writeable(context.address()).map_err(KError::from)?;
                    if context.resolve_key()? != key {
                        return Err(KError::BadAddress);
                    }
                }
                Err(error) if error == KError::WouldBlock => ktask::yield_now(),
                result => return result,
            }
        }
    }

    fn abandon_user_owner(&self, key: FutexKey, lock: &Arc<PiLock>) {
        let effects = {
            let mut bucket = self.bucket(self.bucket_id(key)).state.lock();
            let Some(state) = bucket
                .pi
                .get_mut(&key.pi_index())
                .filter(|state| Arc::ptr_eq(&state.lock, lock))
            else {
                return;
            };
            if state.user.is_none() {
                return;
            }
            lock.unlock_deferred(&current().clone(), |_| Ok(()))
        };
        drop(effects);
    }

    fn user_pi_unlock(&self, context: &impl PiFutexContext) -> KResult<()> {
        let caller = current().clone();
        let caller_tid = tid(&caller)?;
        loop {
            let key = context.resolve_key()?;
            let result = (|| {
                let mut bucket = self.bucket(self.bucket_id(key)).state.lock();
                let word = atomic_load_u32_nofault(context.address()).map_err(KError::from)?;
                if word & FUTEX_TID_MASK != caller_tid {
                    return Err(KError::OperationNotPermitted);
                }
                if bucket
                    .waiters
                    .iter()
                    .any(|waiter| waiter.is_queued() && waiter.route_unlocked().0 == key)
                {
                    return Err(KError::InvalidInput);
                }
                let Some(state) = bucket.pi.get_mut(&key.pi_index()) else {
                    exchange(context.address(), word, 0)?;
                    return Ok(None);
                };
                let owner = state.user.as_mut().ok_or(KError::InvalidInput)?;
                if !state.lock.is_owned_by(&caller) || !word_matches(word, owner) {
                    return Err(KError::InvalidInput);
                }
                let lock = state.lock.clone();
                let effects = lock.unlock_deferred(&caller, |next| {
                    let next_tid = next.map(tid).transpose()?.unwrap_or(0);
                    exchange(
                        context.address(),
                        word,
                        next_tid | if next.is_some() { FUTEX_WAITERS } else { 0 },
                    )?;
                    owner.word_tid = next_tid;
                    owner.owner_died = false;
                    Ok(())
                })?;
                Ok(Some((lock, effects)))
            })();
            match result {
                Ok(Some((lock, effects))) => {
                    drop(effects);
                    self.retire_pi(key, lock);
                    return Ok(());
                }
                Ok(None) => return Ok(()),
                Err(error) if error == KError::BadAddress => {
                    fault_in_user_writeable(context.address()).map_err(KError::from)?
                }
                Err(error) if error == KError::WouldBlock => ktask::yield_now(),
                Err(error) => return Err(error),
            }
        }
    }

    fn exit_user_pi(&self, task: &KtaskRef) {
        for id in 0..BUCKET_COUNT {
            loop {
                let release =
                    {
                        let mut bucket = self.bucket(BucketId(id)).state.lock();
                        let Some(index) = bucket.pi.iter().find_map(|(index, state)| {
                            state.lock.is_owned_by(task).then_some(*index)
                        }) else {
                            break;
                        };
                        let Some(state) = bucket.pi.get_mut(&index) else {
                            break;
                        };
                        let lock = state.lock.clone();
                        let effects = state.exit_owner(task);
                        (index, lock, effects)
                    };
                let (index, pin, effects) = release;
                drop(effects);
                let mut bucket = self.bucket(BucketId(id)).state.lock();
                drop(pin);
                if bucket.pi.get(&index).is_some_and(|state| {
                    !state.lock.is_locked()
                        && (state.user.is_some() || Arc::strong_count(&state.lock) == 1)
                }) {
                    bucket.pi.remove(&index);
                }
            }
        }
    }
}

impl GlobalFutexTable {
    /// Locks a user PI word, with mapping revalidation, timeout and cancellation.
    ///
    /// # Errors
    /// Returns faults/lookup errors, deadlock or invalid-state errors, or a
    /// timeout/interruption only when cancellation beats ownership handoff.
    pub fn lock_pi_user(
        self,
        context: &impl PiFutexContext,
        timeout: Option<TimeSpan>,
    ) -> KResult<()> {
        FUTEX_TABLE.user_pi_lock(context, timeout, false)
    }

    /// Tries a user PI word without sleeping on its owner.
    ///
    /// # Errors
    /// Returns `WouldBlock` for contention and the validation errors of `lock_pi_user`.
    pub fn trylock_pi_user(self, context: &impl PiFutexContext) -> KResult<()> {
        FUTEX_TABLE.user_pi_lock(context, None, true)
    }

    /// Updates the user word before kernel ownership handoff and wake delivery.
    ///
    /// # Errors
    /// Returns `OperationNotPermitted` for nonowners, invalid-state or memory errors.
    pub fn unlock_pi_user(self, context: &impl PiFutexContext) -> KResult<()> {
        FUTEX_TABLE.user_pi_unlock(context)
    }

    /// Releases PI states after the host publishes its task-exiting flag.
    ///
    /// Run in the exiting task's sleepable context, after robust-list marking
    /// and before retiring its identity. Waiters repair words through their own mappings.
    pub fn exit_pi(self, task: &KtaskRef) {
        FUTEX_TABLE.exit_user_pi(task);
    }
}
