// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert_eq, def_test};

use super::*;
use crate::{BioVec, DriverError};

struct Signals {
    count: AtomicUsize,
    state: alloc::sync::Weak<SpinNoIrq<BioWaitState>>,
}

impl BlockSignals for Signals {
    fn notify_completion(&self) {
        core::assert!(self.state.upgrade().unwrap().lock().result.is_some());
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}

struct Waiter {
    signals: Arc<Signals>,
    on_wait: Option<Box<dyn FnOnce()>>,
}

impl BlockWaiter for Waiter {
    fn signals(&self) -> Arc<dyn BlockSignals> {
        self.signals.clone()
    }

    fn wait_completion(&mut self) {
        self.on_wait.take().expect("completed bio must not wait")();
        core::assert_eq!(self.signals.count.load(Ordering::Relaxed), 1);
    }
}

fn pending() -> (Arc<SpinNoIrq<BioWaitState>>, BioWait) {
    let state = Arc::new(SpinNoIrq::new(BioWaitState {
        result: None,
        notification: None,
    }));
    let wait = BioWait {
        state: state.clone(),
        _task_bound: PhantomData,
    };
    (state, wait)
}

fn result(status: DriverResult) -> BioResult {
    (
        status,
        alloc::vec![BioVec::new(alloc::vec![0x5a; 512].into_boxed_slice())],
    )
}

#[def_test]
fn inline_completion_never_prepares_host_wait_resources() {
    for status in [Ok(()), Err(DriverError::Io)] {
        let (state, wait) = pending();
        let result = result(status);
        let address = result.1[0].bytes().as_ptr();
        BioWaitState::complete(&state, result);
        let (returned_status, vectors) =
            wait.wait_with(|| panic!("inline I/O must not prepare a waiter"));
        assert_eq!(returned_status, status);
        assert_eq!(vectors[0].bytes().as_ptr(), address);
        assert_eq!(vectors[0].bytes(), &[0x5a; 512]);
    }
}

#[def_test]
fn completion_during_wait_preparation_needs_no_notification() {
    let (state, wait) = pending();
    let signals = Arc::new(Signals {
        count: AtomicUsize::new(0),
        state: Arc::downgrade(&state),
    });
    let (status, vectors) = wait.wait_with(|| {
        BioWaitState::complete(&state, result(Err(DriverError::Io)));
        Box::new(Waiter {
            signals: signals.clone(),
            on_wait: None,
        })
    });
    assert_eq!(status, Err(DriverError::Io));
    assert_eq!(vectors[0].bytes(), &[0x5a; 512]);
    assert_eq!(signals.count.load(Ordering::Relaxed), 0);
}

#[def_test]
fn completion_after_enrollment_publishes_result_before_notification() {
    let (state, wait) = pending();
    let signals = Arc::new(Signals {
        count: AtomicUsize::new(0),
        state: Arc::downgrade(&state),
    });
    let (status, vectors) = wait.wait_with(|| {
        Box::new(Waiter {
            signals: signals.clone(),
            on_wait: Some(Box::new(move || {
                BioWaitState::complete(&state, result(Ok(())))
            })),
        })
    });
    assert_eq!(status, Ok(()));
    assert_eq!(vectors[0].bytes(), &[0x5a; 512]);
    assert_eq!(signals.count.load(Ordering::Relaxed), 1);
}
