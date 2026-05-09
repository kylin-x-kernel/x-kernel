// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-side timer runtime.

#![no_std]

extern crate alloc;

use alloc::{borrow::ToOwned, collections::binary_heap::BinaryHeap, sync::Arc};
use core::{mem, time::Duration};

use event_listener::{Event, listener};
use khal::time::{NANOS_PER_SEC, TimeValue, monotonic_time, monotonic_time_nanos};
use ksignal::Signo;
use ksync::Mutex;
use ktask::{
    TaskInner, WeakKtaskRef, current,
    future::{block_on, timeout_at},
};
use ktypes::Once;
use lazy_static::lazy_static;
use posix_types::ITimerType;

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
}

static EVENT_NEW_TIMER: Event = Event::new();
static EXPIRED_TASK_HANDLER: Once<fn(&TaskInner)> = Once::new();
const ITIMER_SIGNAL_CAPACITY: usize = 3;

fn timer_signal(ty: ITimerType) -> Signo {
    match ty {
        ITimerType::Real => Signo::SIGALRM,
        ITimerType::Virtual => Signo::SIGVTALRM,
        ITimerType::Prof => Signo::SIGPROF,
    }
}

#[derive(Default)]
struct ITimer {
    interval_ns: usize,
    remaining_ns: usize,
}

impl ITimer {
    fn new(interval_ns: usize, remaining_ns: usize) -> Self {
        let result = Self {
            interval_ns,
            remaining_ns,
        };
        result.renew_timer();
        result
    }

    fn update(&mut self, delta: usize) -> bool {
        if self.remaining_ns == 0 {
            return false;
        }
        if self.remaining_ns > delta {
            self.remaining_ns -= delta;
            false
        } else {
            self.remaining_ns = self.interval_ns;
            self.renew_timer();
            true
        }
    }

    fn renew_timer(&self) {
        if self.remaining_ns > 0 {
            let deadline = monotonic_time() + Duration::from_nanos(self.remaining_ns as u64);
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

/// A manager for per-thread timer and CPU-time accounting.
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
    /// Creates a new [`TimeManager`].
    pub fn new() -> Self {
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

    /// Polls the time manager and returns expired timer signals.
    pub fn poll(&mut self) -> [Option<Signo>; ITIMER_SIGNAL_CAPACITY] {
        let now_ns = monotonic_time_nanos() as usize;
        let delta = now_ns - self.last_wall_ns;
        let mut signals = [None; ITIMER_SIGNAL_CAPACITY];
        let mut signal_count = 0;
        match self.state {
            TimerState::User => {
                self.utime_ns += delta;
                Self::push_signal(
                    &mut signals,
                    &mut signal_count,
                    self.update_itimer(ITimerType::Virtual, delta),
                );
                Self::push_signal(
                    &mut signals,
                    &mut signal_count,
                    self.update_itimer(ITimerType::Prof, delta),
                );
            }
            TimerState::Kernel => {
                self.stime_ns += delta;
                Self::push_signal(
                    &mut signals,
                    &mut signal_count,
                    self.update_itimer(ITimerType::Prof, delta),
                );
            }
            TimerState::None => {}
        }
        Self::push_signal(
            &mut signals,
            &mut signal_count,
            self.update_itimer(ITimerType::Real, delta),
        );
        self.last_wall_ns = now_ns;
        signals
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
        remaining_ns: usize,
    ) -> (TimeValue, TimeValue) {
        let old = mem::replace(
            &mut self.itimers[ty as usize],
            ITimer::new(interval_ns, remaining_ns),
        );
        (
            time_value_from_nanos(old.interval_ns),
            time_value_from_nanos(old.remaining_ns),
        )
    }

    /// Gets the current interval and remaining time.
    pub fn get_itimer(&self, ty: ITimerType) -> (TimeValue, TimeValue) {
        let itimer = &self.itimers[ty as usize];
        (
            time_value_from_nanos(itimer.interval_ns),
            time_value_from_nanos(itimer.remaining_ns),
        )
    }

    fn update_itimer(&mut self, ty: ITimerType, delta: usize) -> Option<Signo> {
        self.itimers[ty as usize]
            .update(delta)
            .then(|| timer_signal(ty))
    }

    fn push_signal(
        signals: &mut [Option<Signo>; ITIMER_SIGNAL_CAPACITY],
        signal_count: &mut usize,
        signo: Option<Signo>,
    ) {
        if let Some(signo) = signo {
            signals[*signal_count] = Some(signo);
            *signal_count += 1;
        }
    }
}

/// Registers the callback used to handle expired timer tasks.
pub fn register_expired_task_handler(handler: fn(&TaskInner)) {
    EXPIRED_TASK_HANDLER.call_once(|| handler);
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

        let now = monotonic_time();
        if deadline <= now {
            if let Some(task) = task_weak.upgrade()
                && let Some(handler) = EXPIRED_TASK_HANDLER.get()
            {
                handler(&task);
            }

            let mut guard = ALARM_LIST.lock();
            assert!(guard.pop().is_some_and(|it| it.deadline == deadline));
        } else {
            listener!(EVENT_NEW_TIMER => listener);
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
        kbuild_config::TASK_STACK_SIZE,
    );
}

#[cfg(unittest)]
mod tests {
    use ksignal::Signo;
    use posix_types::ITimerType;
    use unittest::def_test;

    #[def_test]
    fn test_timer_signal_mapping() {
        assert_eq!(super::timer_signal(ITimerType::Real), Signo::SIGALRM);
        assert_eq!(super::timer_signal(ITimerType::Virtual), Signo::SIGVTALRM);
        assert_eq!(super::timer_signal(ITimerType::Prof), Signo::SIGPROF);
    }

    #[def_test]
    fn test_timemanager_default_output() {
        let tm = super::TimeManager::new();
        let (u, s) = tm.output();
        assert_eq!(u.as_secs(), 0);
        assert_eq!(u.subsec_nanos(), 0);
        assert_eq!(s.as_secs(), 0);
        assert_eq!(s.subsec_nanos(), 0);
    }

    #[def_test]
    fn test_time_value_from_nanos_basic() {
        let tv = super::time_value_from_nanos(0);
        assert_eq!(tv.as_secs(), 0);
        assert_eq!(tv.subsec_nanos(), 0);
    }

    #[def_test]
    fn test_time_value_from_nanos_subsec() {
        let tv = super::time_value_from_nanos(500_000_000);
        assert_eq!(tv.as_secs(), 0);
        assert_eq!(tv.subsec_nanos(), 500_000_000);
    }

    #[def_test]
    fn test_time_value_from_nanos_multi_sec() {
        let tv = super::time_value_from_nanos(2_500_000_000);
        assert_eq!(tv.as_secs(), 2);
        assert_eq!(tv.subsec_nanos(), 500_000_000);
    }

    #[def_test]
    fn test_timemanager_get_itimer_default() {
        let tm = super::TimeManager::new();
        for ty in [ITimerType::Real, ITimerType::Virtual, ITimerType::Prof] {
            let (interval, remained) = tm.get_itimer(ty);
            assert_eq!(interval.as_secs(), 0);
            assert_eq!(remained.as_secs(), 0);
        }
    }

    #[def_test]
    fn test_timemanager_set_state() {
        let mut tm = super::TimeManager::new();
        tm.set_state(super::TimerState::User);
        tm.set_state(super::TimerState::Kernel);
        tm.set_state(super::TimerState::None);
        let (u, s) = tm.output();
        assert_eq!(u.as_secs(), 0);
        assert_eq!(s.as_secs(), 0);
    }

    #[def_test]
    fn test_itimer_update_zero_remained() {
        let mut timer = super::ITimer::default();
        assert!(!timer.update(100));
    }

    #[def_test]
    fn test_itimer_update_counts_down_without_firing() {
        let mut timer = super::ITimer {
            interval_ns: 0,
            remaining_ns: 10,
        };
        assert!(!timer.update(3));
        assert_eq!(timer.remaining_ns, 7);
    }

    #[def_test]
    fn test_itimer_update_fires_and_resets_to_interval() {
        let mut timer = super::ITimer {
            interval_ns: 0,
            remaining_ns: 5,
        };
        assert!(timer.update(5));
        assert_eq!(timer.remaining_ns, 0);
    }

    #[def_test]
    fn test_timemanager_set_itimer_returns_previous_values() {
        let mut tm = super::TimeManager::new();

        let (old_interval, old_remained) = tm.set_itimer(ITimerType::Real, 11, 22);
        assert_eq!(old_interval.as_secs(), 0);
        assert_eq!(old_remained.as_secs(), 0);

        let (old_interval, old_remained) = tm.set_itimer(ITimerType::Real, 33, 44);
        assert_eq!(old_interval.subsec_nanos(), 11);
        assert_eq!(old_remained.subsec_nanos(), 22);

        let (interval, remained) = tm.get_itimer(ITimerType::Real);
        assert_eq!(interval.subsec_nanos(), 33);
        assert_eq!(remained.subsec_nanos(), 44);
    }

    #[def_test]
    fn test_timemanager_update_itimer_emits_signal_on_expiration() {
        let mut tm = super::TimeManager::new();
        tm.itimers[ITimerType::Real as usize] = super::ITimer {
            interval_ns: 0,
            remaining_ns: 1,
        };

        let emitted = tm.update_itimer(ITimerType::Real, 1);

        assert_eq!(emitted, Some(Signo::SIGALRM));
        let (_, remained) = tm.get_itimer(ITimerType::Real);
        assert_eq!(remained.subsec_nanos(), 0);
    }

    #[def_test]
    fn test_timemanager_update_itimer_no_emit_before_expiration() {
        let mut tm = super::TimeManager::new();
        tm.itimers[ITimerType::Prof as usize] = super::ITimer {
            interval_ns: 0,
            remaining_ns: 10,
        };

        let emitted = tm.update_itimer(ITimerType::Prof, 3);

        assert_eq!(emitted, None);
        let (_, remained) = tm.get_itimer(ITimerType::Prof);
        assert_eq!(remained.subsec_nanos(), 7);
    }
}
