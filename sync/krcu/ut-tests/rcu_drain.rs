// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The boot-owned service persists throughout these tests. Local counter
//! fixtures do not replace its driver, callbacks, or permanent marker storage.

use alloc::{sync::Weak, task::Wake};
use core::sync::atomic::AtomicBool;

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{
    drain_callbacks,
    host::{TaskWait, with_prepared_wait},
};

#[def_test]
fn early_markers_cannot_finish_installation() -> unittest::TestResult {
    with_prepared_wait(check_early_markers_cannot_finish_installation).unwrap()
}

fn check_early_markers_cannot_finish_installation(wait: &mut dyn TaskWait) -> unittest::TestResult {
    let completion = DrainCompletion::new(wait.waker());
    completion.remaining.fetch_add(1, Ordering::Relaxed);
    completion.finish_one();
    assert!(!completion.is_complete());
    // Install another marker only after the first has already finished.
    completion.remaining.fetch_add(1, Ordering::Relaxed);
    completion.finish_one();
    assert!(!completion.is_complete());
    completion.finish_one();
    assert!(completion.is_complete());
    unittest::TestResult::Ok
}

#[def_test]
fn final_marker_after_installation_releases_wait() -> unittest::TestResult {
    with_prepared_wait(check_final_marker_after_installation_releases_wait).unwrap()
}

fn check_final_marker_after_installation_releases_wait(
    wait: &mut dyn TaskWait,
) -> unittest::TestResult {
    let completion = DrainCompletion::new(wait.waker());
    completion.remaining.fetch_add(1, Ordering::Relaxed);
    completion.finish_one();
    wait.waker().wake();
    wait.park();
    assert!(!completion.is_complete());
    completion.finish_one();
    wait.park();
    assert!(completion.is_complete());
    unittest::TestResult::Ok
}

struct ReuseMarkerWake {
    marker: &'static DrainMarker,
    previous: Weak<DrainCompletion>,
    next: Arc<DrainCompletion>,
    was_previous_retained: Arc<AtomicBool>,
}

impl Wake for ReuseMarkerWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.was_previous_retained
            .store(self.previous.upgrade().is_some(), Ordering::Release);
        // Model the next installer running before the old wakeup unwinds.
        // This also detects a marker lock accidentally held across wakeup.
        *self.marker.completion.lock() = Some(self.next.clone());
    }
}

#[def_test(serial)]
fn marker_reuse_during_old_wakeup_keeps_completion_owners_separate() -> unittest::TestResult {
    with_prepared_wait(check_marker_reuse).unwrap()
}

fn check_marker_reuse(wait: &mut dyn TaskWait) -> unittest::TestResult {
    let service = SERVICE.get().unwrap();
    // SAFETY: stop prior fixtures' callbacks before manipulating an unqueued
    // permanent marker; no resource required by those callbacks is held.
    let drained = unsafe { drain_callbacks() };
    assert_eq!(drained, Ok(()));
    let mut observations = None;
    RcuTaskIf::with_serialized_drain(&mut || {
        let marker = &service.drain.markers[0];
        let next = Arc::new(DrainCompletion::new(wait.waker()));
        let was_retained = Arc::new(AtomicBool::new(false));
        let previous = Arc::new_cyclic(|previous| {
            DrainCompletion::new(Waker::from(Arc::new(ReuseMarkerWake {
                marker,
                previous: previous.clone(),
                next: next.clone(),
                was_previous_retained: was_retained.clone(),
            })))
        });
        let observer = Arc::downgrade(&previous);
        *marker.completion.lock() = Some(previous);
        let record = core::ptr::from_ref(marker);
        // SAFETY: project from the whole permanently retained marker allocation.
        let node = unsafe { &raw const (*record).node }.cast_mut();
        // SAFETY: exclusive installation under the drain mutex; the prior drain
        // removed all marker membership. This unqueued completion-only fixture
        // directly invokes once without asking the service executor to run it.
        unsafe { complete_marker(node) };
        let installed = marker.completion.lock().take();
        observations = Some((
            was_retained.load(Ordering::Acquire),
            observer.upgrade().is_none(),
            installed
                .as_ref()
                .is_some_and(|installed| Arc::ptr_eq(installed, &next)),
            next.is_complete(),
            next.remaining.load(Ordering::Relaxed),
        ));
    });
    let (was_retained, is_previous_released, is_next_installed, is_next_complete, remaining) =
        observations.unwrap();
    assert!(was_retained);
    assert!(is_previous_released);
    assert!(is_next_installed);
    assert!(!is_next_complete);
    assert_eq!(remaining, 1);
    unittest::TestResult::Ok
}
