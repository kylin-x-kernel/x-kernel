// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Time management module.

use alloc::{borrow::ToOwned, collections::binary_heap::BinaryHeap, sync::Arc};
use core::{mem, time::Duration};

use event_listener::{Event, listener};
use khal::time::{NANOS_PER_SEC, TimeValue, monotonic_time_nanos, wall_time};
use ksignal::Signo;
use ksync::Mutex;
use ktask::{
    WeakKtaskRef, current,
    future::{block_on, timeout_at},
};
use lazy_static::lazy_static;
use strum::FromRepr;

use crate::task::poll_timer;

fn time_value_from_nanos(nanos: usize) -> TimeValue {
    let secs = nanos as u64 / NANOS_PER_SEC;
    let nsecs = nanos as u64 - secs * NANOS_PER_SEC;
    TimeValue::new(secs, nsecs as u32)
}

struct Entry {
    deadline: Duration,
    task: WeakKtaskRef,
}
impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Entry {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        other.deadline.cmp(&self.deadline)
    }
}

lazy_static! {
    static ref ALARM_LIST: Mutex<BinaryHeap<Entry>> = Mutex::new(BinaryHeap::new());
    static ref EVENT_NEW_TIMER: Event = Event::new();
}

/// The type of interval timer.
#[repr(i32)]
#[allow(non_camel_case_types)]
#[derive(Eq, PartialEq, Debug, Clone, Copy, FromRepr)]
pub enum ITimerType {
    /// 统计系统实际运行时间
    Real    = 0,
    /// 统计用户态运行时间
    Virtual = 1,
    /// 统计进程的所有用户态/内核态运行时间
    Prof    = 2,
}

impl ITimerType {
    /// Returns the signal number associated with this timer type.
    pub fn signo(&self) -> Signo {
        match self {
            ITimerType::Real => Signo::SIGALRM,
            ITimerType::Virtual => Signo::SIGVTALRM,
            ITimerType::Prof => Signo::SIGPROF,
        }
    }
}

#[derive(Default)]
struct ITimer {
    interval_ns: usize,
    remained_ns: usize,
}

impl ITimer {
    pub fn new(interval_ns: usize, remained_ns: usize) -> Self {
        let result = Self {
            interval_ns,
            remained_ns,
        };
        result.renew_timer();
        result
    }

    pub fn update(&mut self, delta: usize) -> bool {
        if self.remained_ns == 0 {
            return false;
        }
        if self.remained_ns > delta {
            self.remained_ns -= delta;
            false
        } else {
            self.remained_ns = self.interval_ns;
            self.renew_timer();
            true
        }
    }

    pub fn renew_timer(&self) {
        if self.remained_ns > 0 {
            let deadline = wall_time() + Duration::from_nanos(self.remained_ns as u64);
            let mut guard = ALARM_LIST.lock();
            let should_wake = guard.peek().is_none_or(|it| it.deadline > deadline);
            guard.push(Entry {
                deadline,
                task: Arc::downgrade(&current()),
            });
            drop(guard);
            if should_wake {
                EVENT_NEW_TIMER.notify(1);
            }
        }
    }
}

/// Represents the state of the timer.
#[derive(Debug)]
pub enum TimerState {
    /// Fallback state.
    None,
    /// The timer is running in user space.
    User,
    /// The timer is running in kernel space.
    Kernel,
}

// TODO(mivik): preempting does not change the timer state currently
/// A manager for time-related operations.
pub struct TimeManager {
    utime_ns: usize,
    stime_ns: usize,
    last_wall_ns: usize,
    state: TimerState,
    itimers: [ITimer; 3],
}

impl Default for TimeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TimeManager {
    pub(crate) fn new() -> Self {
        Self {
            utime_ns: 0,
            stime_ns: 0,
            last_wall_ns: 0,
            state: TimerState::None,
            itimers: Default::default(),
        }
    }

    /// Returns the current user time and system time as a tuple of `TimeValue`.
    pub fn output(&self) -> (TimeValue, TimeValue) {
        let utime = time_value_from_nanos(self.utime_ns);
        let stime = time_value_from_nanos(self.stime_ns);
        (utime, stime)
    }

    /// Polls the time manager to update the timers and emit signals if
    /// necessary.
    pub fn poll(&mut self, emitter: impl Fn(Signo)) {
        let now_ns = monotonic_time_nanos() as usize;
        let delta = now_ns - self.last_wall_ns;
        match self.state {
            TimerState::User => {
                self.utime_ns += delta;
                self.update_itimer(ITimerType::Virtual, delta, &emitter);
                self.update_itimer(ITimerType::Prof, delta, &emitter);
            }
            TimerState::Kernel => {
                self.stime_ns += delta;
                self.update_itimer(ITimerType::Prof, delta, &emitter);
            }
            TimerState::None => {}
        }
        self.update_itimer(ITimerType::Real, delta, &emitter);
        self.last_wall_ns = now_ns;
    }

    /// Updates the timer state.
    pub fn set_state(&mut self, state: TimerState) {
        self.state = state;
    }

    /// Sets the interval timer of the specified type with the given interval
    /// and remaining time.
    pub fn set_itimer(
        &mut self,
        ty: ITimerType,
        interval_ns: usize,
        remained_ns: usize,
    ) -> (TimeValue, TimeValue) {
        let old = mem::replace(
            &mut self.itimers[ty as usize],
            ITimer::new(interval_ns, remained_ns),
        );
        (
            time_value_from_nanos(old.interval_ns),
            time_value_from_nanos(old.remained_ns),
        )
    }

    /// Gets the current interval and remaining time.
    pub fn get_itimer(&self, ty: ITimerType) -> (TimeValue, TimeValue) {
        let itimer = &self.itimers[ty as usize];
        (
            time_value_from_nanos(itimer.interval_ns),
            time_value_from_nanos(itimer.remained_ns),
        )
    }

    fn update_itimer(&mut self, ty: ITimerType, delta: usize, emitter: impl Fn(Signo)) {
        if self.itimers[ty as usize].update(delta) {
            emitter(ty.signo());
        }
    }
}

async fn alarm_task() {
    loop {
        let entry = {
            let guard = ALARM_LIST.lock();
            guard.peek().map(|e| (e.deadline, e.task.clone()))
        };

        let Some((deadline, task_weak)) = entry else {
            listener!(EVENT_NEW_TIMER => listener);
            if !ALARM_LIST.lock().is_empty() {
                continue;
            }
            listener.await;
            continue;
        };

        let now = wall_time();
        if deadline <= now {
            // 任务已到期，执行它
            if let Some(task) = task_weak.upgrade() {
                poll_timer(&task);
            }

            // 从队列中移除
            let mut guard = ALARM_LIST.lock();
            assert!(guard.pop().is_some_and(|it| it.deadline == deadline));
        } else {
            // 任务未到期，等待到 deadline 或新任务插入
            listener!(EVENT_NEW_TIMER => listener);

            // 检查队列头是否还是同一个任务
            if ALARM_LIST
                .lock()
                .peek()
                .is_none_or(|it| it.deadline != deadline)
            {
                continue;
            }

            let _ = timeout_at(Some(deadline), listener).await;
        }
    }
}

/// Spawns the alarm task.
pub fn spawn_alarm_task() {
    ktask::spawn_raw(
        || block_on(alarm_task()),
        "alarm_task".to_owned(),
        platconfig::TASK_STACK_SIZE,
    );
}

/// Unit tests.
#[cfg(unittest)]
pub mod tests_time {
    use ksignal::Signo;
    use unittest::def_test;

    use super::{ITimerType, TimeManager};

    #[def_test]
    fn test_itimer_signo() {
        assert_eq!(ITimerType::Real.signo(), Signo::SIGALRM);
        assert_eq!(ITimerType::Virtual.signo(), Signo::SIGVTALRM);
        assert_eq!(ITimerType::Prof.signo(), Signo::SIGPROF);
    }

    #[def_test]
    fn test_itimer_from_repr() {
        assert_eq!(ITimerType::from_repr(0), Some(ITimerType::Real));
        assert_eq!(ITimerType::from_repr(1), Some(ITimerType::Virtual));
        assert_eq!(ITimerType::from_repr(2), Some(ITimerType::Prof));
        assert_eq!(ITimerType::from_repr(3), None);
    }

    #[def_test]
    fn test_timemanager_default_output() {
        let tm = TimeManager::new();
        let (u, s) = tm.output();
        assert_eq!(u.as_secs(), 0);
        assert_eq!(u.subsec_nanos(), 0);
        assert_eq!(s.as_secs(), 0);
        assert_eq!(s.subsec_nanos(), 0);
    }
}
