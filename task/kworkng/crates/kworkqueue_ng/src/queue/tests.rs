// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::{
    assert, assert_eq,
    pin::pin,
    sync::atomic::{AtomicUsize, Ordering},
};

use kcpu_id_map::LogicalCpuId;
use unittest::def_test;

use super::{
    Queue,
    config::{CpuPolicy, QueueConfig, QueueConfigError},
    control::{CLOSED, COLOR_COUNT},
};
use crate::work::{Work, observers::CompletionObserver};

mod routing;

fn queue() -> Arc<Queue> {
    queue_for(&[0, 1], 1)
}

/// Prepares an unbound test queue whose execution is driven by explicit claims.
///
/// # Panics
/// Panics if the supplied CPU set or release limit is invalid.
pub(crate) fn queue_for(cpus: &[usize], max_active: usize) -> Arc<Queue> {
    let cpus: alloc::vec::Vec<_> = cpus.iter().copied().map(LogicalCpuId::new).collect();
    Queue::new(QueueConfig::validate(&cpus, CpuPolicy::CurrentCpu, max_active, |_| true).unwrap())
}

/// Checks sparse input normalization and explicit configuration failures.
///
/// # Panics
/// Panics on silent defaults, invalid acceptance or unstable CPU ordering.
#[def_test]
fn configuration_rejects_invalid_inputs_and_sorts_cpus() {
    let cpu = LogicalCpuId::new;
    let config = QueueConfig::validate(&[cpu(4), cpu(0), cpu(2)], CpuPolicy::RoundRobin, 3, |_| {
        true
    })
    .unwrap();
    assert_eq!(config.cpus(), &[cpu(0), cpu(2), cpu(4)]);
    assert_eq!(config.policy(), CpuPolicy::RoundRobin);
    assert_eq!(config.max_active(), 3);
    assert_eq!(
        QueueConfig::validate(&[], CpuPolicy::CurrentCpu, 1, |_| true).unwrap_err(),
        QueueConfigError::EmptyCpuSet
    );
    assert_eq!(
        QueueConfig::validate(&[cpu(0)], CpuPolicy::CurrentCpu, 0, |_| true).unwrap_err(),
        QueueConfigError::ZeroMaxActive
    );
    assert_eq!(
        QueueConfig::validate(&[cpu(2), cpu(0), cpu(2)], CpuPolicy::CurrentCpu, 1, |_| {
            true
        })
        .unwrap_err(),
        QueueConfigError::DuplicateCpu(cpu(2))
    );
    assert_eq!(
        QueueConfig::validate(&[cpu(0), cpu(4)], CpuPolicy::CurrentCpu, 1, |id| id
            != cpu(4))
        .unwrap_err(),
        QueueConfigError::CpuUnavailable(cpu(4))
    );
}

/// Checks production configuration consults the actual initialized boot CPU map.
///
/// # Panics
/// Panics if a present CPU is rejected or an unmapped CPU becomes usable.
#[def_test]
fn configuration_checks_boot_cpu_presence() {
    let mut cpus = alloc::vec::Vec::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
    let config = QueueConfig::new(&cpus, CpuPolicy::CurrentCpu, 1).unwrap();
    assert_eq!(config.cpus(), cpus.as_slice());
    let absent = LogicalCpuId::new(usize::MAX);
    assert_eq!(
        QueueConfig::new(&[absent], CpuPolicy::CurrentCpu, 1).unwrap_err(),
        QueueConfigError::CpuUnavailable(absent)
    );
}

/// Checks sparse CPU IDs route to their own compact partitions without fallback.
///
/// # Panics
/// Panics on sparse index confusion, shared allowance or acceptance on a disallowed CPU.
#[def_test]
fn sparse_cpu_partitions_preserve_identity_and_independent_allowance() {
    let queue = queue_for(&[4, 0, 2], 1);
    assert_eq!(queue.partitions.len(), 3);
    let works = pin!(core::array::from_fn::<_, 3, _>(|_| Work::with_callback(
        || {}
    )));
    for (work, id) in works.iter().zip([0, 2, 4]) {
        // SAFETY: the pinned array remains live through all executions below.
        assert!(unsafe {
            queue
                .submit(work, Some(LogicalCpuId::new(id)), || {
                    panic!("explicit target")
                })
                .is_ok()
        });
    }
    let rejected = pin!(Work::with_callback(|| {}));
    for id in [1, 3, 5] {
        // SAFETY: rejected stays pinned and no occurrence can be published on these CPUs.
        assert!(!unsafe {
            queue
                .submit(&rejected, Some(LogicalCpuId::new(id)), || {
                    panic!("explicit target")
                })
                .is_ok()
        });
        assert!(queue.claim_with_demand(LogicalCpuId::new(id)).0.is_none());
    }
    let a = queue.claim_with_demand(LogicalCpuId::new(0)).0.unwrap();
    let b = queue.claim_with_demand(LogicalCpuId::new(2)).0.unwrap();
    let c = queue.claim_with_demand(LogicalCpuId::new(4)).0.unwrap();
    for work in works.iter() {
        assert!(!work.cancel_pending());
    }
    a.run();
    b.run();
    c.run();
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Checks every color is decoded independently from closure and CPU placement.
///
/// # Panics
/// Panics if upper color bits alias closure, a CPU route, or another batch.
#[def_test]
fn all_colors_preserve_cpu_and_batch_identity() {
    let queue = queue();
    let works = pin!(core::array::from_fn::<_, { COLOR_COUNT }, _>(|_| {
        Work::with_callback(|| {})
    }));
    for (color, work) in works.iter().enumerate() {
        // Test-only use of fresh colors, never reuse of an outstanding batch.
        queue.control.store(color, Ordering::SeqCst);
        assert_eq!(queue.admission_color(), Some(color));
        // SAFETY: the pinned array remains live until every Work is canceled below.
        assert!(unsafe {
            queue
                .submit(work, Some(LogicalCpuId::new(color % 2)), || {
                    panic!("explicit target")
                })
                .is_ok()
        });
    }
    queue.close_admission();
    assert_eq!(
        queue.control.load(Ordering::SeqCst),
        CLOSED | (COLOR_COUNT - 1)
    );
    assert_eq!(queue.admission_color(), None);
    let receipts: alloc::vec::Vec<_> = (0..COLOR_COUNT)
        .map(|color| {
            let (receipt, token) = CompletionObserver::prepare();
            assert!(
                queue.partitions[color % 2]
                    .observe_batch(color, token)
                    .is_ok()
            );
            let (_, absent) = CompletionObserver::prepare();
            assert!(
                queue.partitions[1 - color % 2]
                    .observe_batch(color, absent)
                    .is_err()
            );
            receipt
        })
        .collect();
    for color in (0..COLOR_COUNT).rev() {
        assert!(works[color].cancel_pending());
        for (index, receipt) in receipts.iter().enumerate() {
            assert_eq!(receipt.is_completed(), index >= color);
        }
    }
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Checks Queue supplies references and rejects invalid/duplicate placement intact.
///
/// # Panics
/// Panics if rejection changes ownership, references leak, or execution is lost.
#[def_test]
fn submission_owns_queue_resources() {
    let queue = queue();
    let calls = AtomicUsize::new(0);
    let work = pin!(Work::with_callback(|| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    // SAFETY: pinned Work and its borrowed captures outlive all operations below.
    assert!(!unsafe {
        queue
            .submit(&work, Some(LogicalCpuId::new(2)), || {
                panic!("explicit target")
            })
            .is_ok()
    });
    assert_eq!(Arc::strong_count(&queue), 1);
    // SAFETY: same stable storage, drained before return.
    assert!(unsafe {
        queue
            .submit(&work, Some(LogicalCpuId::new(1)), || {
                panic!("explicit target")
            })
            .is_ok()
    });
    assert_eq!(Arc::strong_count(&queue), 2);
    // SAFETY: duplicate cannot extend storage lifetime beyond the drain below.
    assert!(!unsafe {
        queue
            .submit(&work, Some(LogicalCpuId::new(0)), || {
                panic!("explicit target")
            })
            .is_ok()
    });
    assert_eq!(Arc::strong_count(&queue), 2);
    assert!(queue.partitions[0].claim_next().is_none());
    queue.partitions[1].claim_next().unwrap().run();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Checks closing rejects new admissions but leaves accepted execution available.
///
/// # Panics
/// Panics if close cancels prior work, admits new work or leaks a Queue reference.
#[def_test]
fn closing_is_not_draining() {
    let queue = queue();
    let before = pin!(Work::with_callback(|| {}));
    let after = pin!(Work::with_callback(|| {}));
    // SAFETY: both pinned Work allocations remain live through final execution.
    assert!(unsafe {
        queue
            .submit(&before, Some(LogicalCpuId::new(0)), || {
                panic!("explicit target")
            })
            .is_ok()
    });
    queue.close_admission();
    queue.close_admission();
    // SAFETY: even a rejected admission uses stable storage through this call.
    assert!(!unsafe {
        queue
            .submit(&after, Some(LogicalCpuId::new(1)), || {
                panic!("explicit target")
            })
            .is_ok()
    });
    let (receipt, token) = CompletionObserver::prepare();
    assert!(queue.partitions[0].observe_batch(0, token).is_ok());
    assert!(!receipt.is_completed());
    queue.partitions[0].claim_next().unwrap().run();
    assert!(receipt.is_completed());
    assert_eq!(Arc::strong_count(&queue), 1);
    assert_eq!(after.observer_count(), 0);
}

/// Checks counts use the Queue's current color on every partition.
///
/// # Panics
/// Panics if a submission uses stale color or closing discards the current color.
#[def_test]
fn current_color_is_sampled_per_admission() {
    let queue = queue();
    let old = pin!(Work::with_callback(|| {}));
    let next = pin!(Work::with_callback(|| {}));
    // SAFETY: pinned storage is canceled below before release.
    assert!(unsafe {
        queue
            .submit(&old, Some(LogicalCpuId::new(0)), || {
                panic!("explicit target")
            })
            .is_ok()
    });
    // Test-only publication of the unused color, with no reuse or other publisher.
    // Real flush must also establish barriers and independent completion receipts.
    queue.control.store(1, Ordering::SeqCst);
    // SAFETY: pinned storage is executed below before release.
    assert!(unsafe {
        queue
            .submit(&next, Some(LogicalCpuId::new(1)), || {
                panic!("explicit target")
            })
            .is_ok()
    });
    let (old_receipt, old_token) = CompletionObserver::prepare();
    let (new_receipt, new_token) = CompletionObserver::prepare();
    queue.close_admission();
    assert_eq!(queue.control.load(Ordering::SeqCst), CLOSED | 1);
    assert!(queue.partitions[0].observe_batch(0, old_token).is_ok());
    assert!(queue.partitions[1].observe_batch(1, new_token).is_ok());
    assert!(old.cancel_pending());
    assert!(old_receipt.is_completed());
    assert!(!new_receipt.is_completed());
    queue.partitions[1].claim_next().unwrap().run();
    assert!(new_receipt.is_completed());
}

/// Checks cancellation retains accounting after the last semantic Queue release.
///
/// # Panics
/// Panics if pending does not retain Queue or cleanup leaks Queue/partition storage.
#[def_test]
fn pending_retains_queue_without_a_cycle() {
    let queue = queue();
    let weak_queue = Arc::downgrade(&queue);
    let partition = queue.partitions[1].clone();
    let weak_partition = Arc::downgrade(&partition);
    let work = pin!(Work::with_callback(|| {}));
    // SAFETY: pinned Work is retained and canceled despite dropping the Queue handle.
    assert!(unsafe {
        queue
            .submit(&work, Some(LogicalCpuId::new(1)), || {
                panic!("explicit target")
            })
            .is_ok()
    });
    queue.close_admission();
    let (receipt, token) = CompletionObserver::prepare();
    assert!(partition.observe_batch(0, token).is_ok());
    drop(partition);
    drop(queue);
    assert!(weak_queue.upgrade().is_some());
    assert!(work.cancel_pending());
    assert!(receipt.is_completed());
    assert!(weak_queue.upgrade().is_none());
    assert!(weak_partition.upgrade().is_none());
}

/// Checks running retains Queue, fixes its partition and blocks successor execution.
///
/// # Panics
/// Panics if ownership migrates while running or Queue dies before its callback.
#[def_test]
fn running_retains_queue_and_original_route() {
    let queue = queue();
    let weak = Arc::downgrade(&queue);
    let partition = queue.partitions[0].clone();
    let work = pin!(Work::with_callback(|| {
        assert!(weak.upgrade().is_some());
    }));
    // SAFETY: pinned Work and captures outlive both executions below.
    assert!(unsafe {
        queue
            .submit(&work, Some(LogicalCpuId::new(0)), || {
                panic!("explicit target")
            })
            .is_ok()
    });
    let running = partition.claim_next().unwrap();
    // SAFETY: same retained storage, including the pending successor.
    assert!(!unsafe {
        queue
            .submit(&work, Some(LogicalCpuId::new(1)), || {
                panic!("explicit target")
            })
            .is_ok()
    });
    // SAFETY: original route accepts the successor drained below.
    assert!(unsafe {
        queue
            .submit(&work, Some(LogicalCpuId::new(0)), || {
                panic!("explicit target")
            })
            .is_ok()
    });
    assert!(partition.claim_next().is_none());
    queue.close_admission();
    drop(queue);
    running.run();
    assert!(weak.upgrade().is_some());
    partition.claim_next().unwrap().run();
    assert!(weak.upgrade().is_none());
}
