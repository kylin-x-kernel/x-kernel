// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

/// Test-only observation points around an owned sleep's real IRQ callback.
/// Wake probes run without the sleep-state lock in timer IRQ context. They
/// must not sleep, unwind, or cancel their timer. Any inter-CPU handshake must
/// have an independently runnable releaser. `on_cancel_wait` runs on the waiter
/// after the core actually observed in-flight delivery and released its lock.
pub trait SleepProbeForTests: Send + Sync {
    fn before_wake(&self);
    fn after_wake(&self);
    fn on_cancel_wait(&self);
}

struct Registration {
    task: crate::KtaskRef,
    probe: Arc<dyn SleepProbeForTests>,
}

kspin::static_lock! {
    static PROBE: kspin::SpinNoIrq<Option<Registration>> = kspin::SpinNoIrq::new(None);
}

/// Installs a probe for sleeps prepared by this task only. Serial fixtures
/// must drain all scoped waits before returning; unrelated task sleeps are
/// unaffected. The registration is removed even if the body unwinds.
pub fn with_sleep_probe_for_tests<R>(
    probe: Arc<dyn SleepProbeForTests>,
    body: impl FnOnce() -> R,
) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            let previous = PROBE.lock().take();
            drop(previous);
        }
    }
    {
        let mut slot = PROBE.lock();
        assert!(slot.is_none(), "sleep probe requires a serial fixture");
        *slot = Some(Registration {
            task: crate::current().clone(),
            probe,
        });
    }
    let _reset = Reset;
    body()
}

pub(super) fn current_probe() -> Option<Arc<dyn SleepProbeForTests>> {
    PROBE.lock().as_ref().and_then(|entry| {
        crate::KtaskRef::ptr_eq(&entry.task, &crate::current()).then(|| entry.probe.clone())
    })
}
