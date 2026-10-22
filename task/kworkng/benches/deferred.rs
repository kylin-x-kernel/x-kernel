// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kfinalize::DeferredArc;
use ktime_types::TimeSpan;

#[derive(Debug)]
struct MeasuredPayload {
    drops: Arc<AtomicUsize>,
}

impl Drop for MeasuredPayload {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Release);
    }
}

pub(super) fn run() {
    const COUNT: usize = 256;
    const CLONES: usize = 4096;
    for deferred in [false, true, true, false] {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut plain = alloc::vec::Vec::with_capacity(COUNT);
        let mut delayed = alloc::vec::Vec::with_capacity(COUNT);
        let started = khal::time::monotonic_time();
        for _ in 0..COUNT {
            let payload = MeasuredPayload {
                drops: drops.clone(),
            };
            if deferred {
                delayed.push(DeferredArc::new(payload).unwrap());
            } else {
                plain.push(Arc::new(payload));
            }
        }
        let prepared = khal::time::monotonic_time();
        if deferred {
            for _ in 0..CLONES {
                drop(core::hint::black_box(delayed[0].clone()));
            }
        } else {
            for _ in 0..CLONES {
                drop(core::hint::black_box(plain[0].clone()));
            }
        }
        let cloned = khal::time::monotonic_time();
        if deferred {
            let weak = DeferredArc::downgrade(&delayed[0]);
            for _ in 0..CLONES {
                drop(core::hint::black_box(weak.upgrade().unwrap()));
            }
        } else {
            let weak = Arc::downgrade(&plain[0]);
            for _ in 0..CLONES {
                drop(core::hint::black_box(weak.upgrade().unwrap()));
            }
        }
        let upgraded = khal::time::monotonic_time();
        plain.clear();
        delayed.clear();
        let submitted = khal::time::monotonic_time();
        let deadline = submitted + TimeSpan::from_secs(5);
        while drops.load(Ordering::Acquire) != COUNT || Arc::strong_count(&drops) != 1 {
            assert!(khal::time::monotonic_time() < deadline);
            ktask::sleep(TimeSpan::from_millis(1));
        }
        let completed = khal::time::monotonic_time();
        klogger::kprintln!(
            "deferred cost: delayed={deferred} count={COUNT} refs={CLONES} prepare_ns={} \
             clone_drop_ns={} upgrade_drop_ns={} submit_ns={} total_release_ns={}",
            (prepared - started).as_nanos(),
            (cloned - prepared).as_nanos(),
            (upgraded - cloned).as_nanos(),
            (submitted - upgraded).as_nanos(),
            (completed - upgraded).as_nanos()
        );
    }
    interference();
}

#[derive(Debug)]
struct SleepingHead {
    entered_cpu: Arc<AtomicUsize>,
    can_finish: Arc<AtomicBool>,
}

impl Drop for SleepingHead {
    fn drop(&mut self) {
        self.entered_cpu.store(
            khal::percpu::this_cpu_id().as_usize() + 1,
            Ordering::Release,
        );
        let deadline = khal::time::monotonic_time() + TimeSpan::from_secs(5);
        while !self.can_finish.load(Ordering::Acquire) {
            assert!(khal::time::monotonic_time() < deadline);
            ktask::sleep(TimeSpan::from_millis(1));
        }
    }
}

fn interference() {
    const BACKLOG: usize = 65;
    const PROBES: usize = 32;
    if kbuild_config::WORKQUEUE_WORKERS_PER_POOL < 2 {
        klogger::kprintln!("deferred interference: skipped (requires spare normal worker)");
        return;
    }
    for blocked in [false, true, true, false] {
        let entered_cpu = Arc::new(AtomicUsize::new(0));
        let can_finish = Arc::new(AtomicBool::new(false));
        let drops = Arc::new(AtomicUsize::new(0));
        let head = blocked.then(|| {
            DeferredArc::new(SleepingHead {
                entered_cpu: entered_cpu.clone(),
                can_finish: can_finish.clone(),
            })
            .unwrap()
        });
        let mut tail = alloc::vec::Vec::with_capacity(BACKLOG);
        for _ in 0..BACKLOG {
            tail.push(
                DeferredArc::new(MeasuredPayload {
                    drops: drops.clone(),
                })
                .unwrap(),
            );
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let captured = calls.clone();
        let ordinary = crate::WorkHandle::new(move || {
            captured.fetch_add(1, Ordering::Release);
        })
        .unwrap();
        let mut target = khal::percpu::this_cpu_id();
        drop(head);
        if blocked {
            let deadline = khal::time::monotonic_time() + TimeSpan::from_secs(5);
            while entered_cpu.load(Ordering::Acquire) == 0 {
                assert!(khal::time::monotonic_time() < deadline);
                ktask::sleep(TimeSpan::from_millis(1));
            }
            target = kcpu_id_map::LogicalCpuId::new(entered_cpu.load(Ordering::Acquire) - 1);
        }
        let released = khal::time::monotonic_time();
        tail.clear();
        let started = khal::time::monotonic_time();
        for _ in 0..PROBES {
            ordinary.queue_default(Some(target)).unwrap();
            ordinary.flush().unwrap();
        }
        let probed = khal::time::monotonic_time();
        let finished_while_blocked = drops.load(Ordering::Acquire);
        if blocked {
            assert_eq!(finished_while_blocked, 0);
        }
        assert_eq!(calls.load(Ordering::Acquire), PROBES);
        can_finish.store(true, Ordering::Release);
        let deadline = probed + TimeSpan::from_secs(5);
        while drops.load(Ordering::Acquire) != BACKLOG
            || Arc::strong_count(&drops) != 1
            || Arc::strong_count(&can_finish) != 1
        {
            assert!(khal::time::monotonic_time() < deadline);
            ktask::sleep(TimeSpan::from_millis(1));
        }
        let completed = khal::time::monotonic_time();
        ordinary.shutdown().unwrap();
        klogger::kprintln!(
            "deferred interference: blocked={blocked} cpu={} workers={} backlog={BACKLOG} \
             probes={PROBES} probe_ns={} tail_at_probe_end={finished_while_blocked} \
             after_gate_ns={} total_release_ns={}",
            target.as_usize(),
            kbuild_config::WORKQUEUE_WORKERS_PER_POOL,
            (probed - started).as_nanos(),
            (completed - probed).as_nanos(),
            (completed - released).as_nanos()
        );
    }
}
