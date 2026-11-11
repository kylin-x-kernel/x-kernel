// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A data-carrying priority-inheritance mutex for the current task.

use alloc::sync::Arc;
use core::{
    cell::UnsafeCell,
    future::Future,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    pin::{Pin, pin},
    task::{Context, Poll},
};

use kerrno::{KError, KErrorKind, KResult};
use kiface_task_api::TaskExecutionIf;
use klazy::Once;

use crate::{
    PiHost,
    graph::{self, LockState, Waiter, with_graph},
};

/// A sleeping mutex whose waiters lend their priority to the owner.
pub struct RtMutex<H: PiHost, T: ?Sized> {
    state: Once<Arc<LockState<H>>>,
    data: UnsafeCell<T>,
}

// SAFETY: the lock hands `&mut T` to one owner at a time, possibly on another
// task, so `T: Send` is all the protected value needs.
unsafe impl<H: PiHost, T: ?Sized + Send> Send for RtMutex<H, T> {}
// SAFETY: see the `Send` justification above.
unsafe impl<H: PiHost, T: ?Sized + Send> Sync for RtMutex<H, T> {}

impl<H: PiHost, T> RtMutex<H, T> {
    /// An unlocked mutex holding `value`.
    pub const fn new(value: T) -> Self {
        Self {
            state: Once::new(),
            data: UnsafeCell::new(value),
        }
    }
}

impl<H: PiHost, T: ?Sized> RtMutex<H, T> {
    /// Locks the mutex, sleeping until the owner hands it over.
    ///
    /// Requires sleepable task context. The wait is not interruptible.
    ///
    /// # Errors
    ///
    /// [`KErrorKind::Deadlock`] when the current task already owns the mutex
    /// or the owner chain leads back to it. [`KErrorKind::BadState`] when
    /// this task is already waiting on a priority-inheritance lock.
    pub fn lock(&self) -> KResult<RtMutexGuard<'_, H, T>> {
        let task = H::current();
        if self.try_acquire(&task)? {
            return Ok(RtMutexGuard::new(self));
        }
        let mut wait = self.queue(&task)?;
        let mut result = Ok(());
        {
            let mut body = pin!(async {
                result = (&mut wait).await;
            });
            TaskExecutionIf::block_on(body.as_mut());
        }
        result?;
        Ok(RtMutexGuard::new(self))
    }

    /// Locks the mutex if it is free. Never blocks.
    ///
    /// # Errors
    ///
    /// [`KErrorKind::Deadlock`] when the current task already owns it.
    pub fn try_lock(&self) -> KResult<Option<RtMutexGuard<'_, H, T>>> {
        Ok(self
            .try_acquire(&H::current())?
            .then(|| RtMutexGuard::new(self)))
    }

    /// Borrows the value through exclusive access, without locking.
    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }

    fn state(&self) -> &Arc<LockState<H>> {
        self.state.call_once(|| Arc::new(LockState::new()))
    }

    /// `Ok(true)` when `task` now owns the lock.
    fn try_acquire(&self, task: &H::Task) -> KResult<bool> {
        let state = self.state();
        with_graph(|token| {
            let inner = state.inner(token);
            match inner.owner.as_ref() {
                Some(owner) if H::same_task(owner, task) => Err(KError::from(KErrorKind::Deadlock)),
                Some(_) => Ok(false),
                None => {
                    inner.owner = Some(task.clone());
                    Ok(true)
                }
            }
        })
    }

    /// Queues `task`, or records that it owns a lock which became free.
    fn queue(&self, task: &H::Task) -> KResult<Wait<H>> {
        let state = self.state();
        let queued = with_graph(|token| {
            if H::node(task).blocked(token).is_some() {
                return Err(KError::from(KErrorKind::BadState));
            }
            let inner = state.inner(token);
            let Some(owner) = inner.owner.clone() else {
                inner.owner = Some(task.clone());
                return Ok(None);
            };
            graph::ensure_no_deadlock::<H>(&owner, task, token)?;
            let waiter = Arc::new(Waiter::new(task.clone(), state.clone()));
            graph::enqueue(&waiter, token);
            Ok(Some((waiter, owner)))
        })?;
        let Some((waiter, owner)) = queued else {
            return Ok(Wait {
                pending: Some(Pending::Acquired(state.clone())),
            });
        };
        let wait = Wait {
            pending: Some(Pending::Queued(waiter)),
        };
        graph::propagate::<H>(owner);
        Ok(wait)
    }
}

impl<H: PiHost, T: ?Sized> Drop for RtMutex<H, T> {
    fn drop(&mut self) {
        let Some(state) = self.state.get() else {
            return;
        };
        // A forgotten owner would keep donating after the lock is gone.
        // Waiters still blocked in `lock` require the mutex to stay alive;
        // dropping it then is a caller bug.
        loop {
            let Some(owner) = with_graph(|token| state.inner(token).owner.clone()) else {
                return;
            };
            if release(state, &owner).is_err() {
                return;
            }
        }
    }
}

fn release<H: PiHost>(state: &LockState<H>, owner: &H::Task) -> KResult<()> {
    let transfer = with_graph(|token| {
        if !state.is_owned_by(owner, token) {
            return Err(KError::from(KErrorKind::OperationNotPermitted));
        }
        Ok(graph::transfer(state, token))
    })?;
    if let Some(old) = transfer.old_owner {
        graph::propagate::<H>(old);
    }
    if let Some(new) = transfer.new_owner {
        graph::propagate::<H>(new);
    }
    if let Some(waker) = transfer.waker {
        waker.wake();
    }
    Ok(())
}

enum Pending<H: PiHost> {
    Acquired(Arc<LockState<H>>),
    Queued(Arc<Waiter<H>>),
}

/// Current-task wait. Drop withdraws it; a grant that arrives first is passed on.
struct Wait<H: PiHost> {
    pending: Option<Pending<H>>,
}

impl<H: PiHost> Future for Wait<H> {
    type Output = KResult<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<KResult<()>> {
        let this = self.get_mut();
        let Some(pending) = this.pending.take() else {
            return Poll::Ready(Err(KError::from(KErrorKind::BadState)));
        };
        match pending {
            Pending::Acquired(_) => Poll::Ready(Ok(())),
            Pending::Queued(waiter) => {
                let poll = with_graph(|token| {
                    if waiter.is_granted(token) {
                        return Poll::Ready(Ok(()));
                    }
                    if !waiter.is_queued() {
                        return Poll::Ready(Err(KError::from(KErrorKind::BadState)));
                    }
                    waiter.register_waker(cx.waker(), token);
                    Poll::Pending
                });
                if poll.is_pending() {
                    this.pending = Some(Pending::Queued(waiter));
                }
                poll
            }
        }
    }
}

impl<H: PiHost> Drop for Wait<H> {
    fn drop(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        match pending {
            Pending::Acquired(state) => {
                let _ = release(&state, &H::current());
            }
            Pending::Queued(waiter) => {
                let (is_granted, owner) = with_graph(|token| {
                    if waiter.is_granted(token) {
                        return (true, None);
                    }
                    if !waiter.is_queued() {
                        return (false, None);
                    }
                    (false, graph::dequeue(&waiter, token))
                });
                if let Some(owner) = owner {
                    graph::propagate::<H>(owner);
                }
                if is_granted {
                    let _ = release(&waiter.lock, &waiter.task);
                }
            }
        }
    }
}

/// Exclusive access to an [`RtMutex`] value. Unlocks on drop.
///
/// Not `Send`: the owning task must release the lock itself.
pub struct RtMutexGuard<'a, H: PiHost, T: ?Sized> {
    mutex: &'a RtMutex<H, T>,
    _not_send: PhantomData<*const ()>,
}

// SAFETY: sharing the guard only shares `&T`.
unsafe impl<H: PiHost, T: ?Sized + Sync> Sync for RtMutexGuard<'_, H, T> {}

impl<'a, H: PiHost, T: ?Sized> RtMutexGuard<'a, H, T> {
    fn new(mutex: &'a RtMutex<H, T>) -> Self {
        Self {
            mutex,
            _not_send: PhantomData,
        }
    }
}

impl<H: PiHost, T: ?Sized> Deref for RtMutexGuard<'_, H, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the guard proves the current task owns the lock.
        unsafe { &*self.mutex.data.get() }
    }
}

impl<H: PiHost, T: ?Sized> DerefMut for RtMutexGuard<'_, H, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the guard proves the current task owns the lock, and
        // `&mut self` excludes other borrows through this guard.
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<H: PiHost, T: ?Sized> Drop for RtMutexGuard<'_, H, T> {
    fn drop(&mut self) {
        // The guard cannot leave the owning task, so the owner check passes.
        let released = self.mutex.unlock();
        debug_assert!(released.is_ok(), "an RtMutex guard is held by its owner");
    }
}

impl<H: PiHost, T: ?Sized> RtMutex<H, T> {
    fn unlock(&self) -> KResult<()> {
        let Some(state) = self.state.get() else {
            return Err(KError::from(KErrorKind::OperationNotPermitted));
        };
        release(state, &H::current())
    }
}
