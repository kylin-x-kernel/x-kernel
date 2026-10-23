// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bounded multi-CPU submission, self-release and drain accounting.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use kcpu_id_map::KCpuMaskExt;
use krcu::{CallbackNode, PreparedCallback};

const PER_PRODUCER: usize = 32;

struct Ledger {
    calls: Vec<AtomicUsize>,
    drops: Vec<AtomicUsize>,
    accepted: AtomicUsize,
    cancelled: AtomicUsize,
    live: AtomicUsize,
    peak_live: AtomicUsize,
    errors: AtomicUsize,
}

#[repr(C)]
struct Record {
    node: CallbackNode,
    id: usize,
    ledger: Arc<Ledger>,
}

const _: () = assert!(core::mem::offset_of!(Record, node) == 0);

impl Drop for Record {
    fn drop(&mut self) {
        self.ledger.drops[self.id].fetch_add(1, Ordering::Relaxed);
        self.ledger.live.fetch_sub(1, Ordering::Relaxed);
    }
}

/// # Safety
/// Consume once the zero-offset node projection of the Box transferred by
/// prepare. The service has detached it and no other owner may access it.
unsafe fn invoke(node: *mut CallbackNode) {
    // SAFETY: repr(C) and the checked offset recover the submitted allocation;
    // acceptance retained its unique Box ownership until this invocation.
    let record = unsafe { Box::from_raw(node.cast::<Record>()) };
    if kirq::context::interrupt_context_level() != kirq::context::InterruptContextLevel::Softirq
        || ktask::is_sleepable_context()
    {
        record.ledger.errors.fetch_add(1, Ordering::Relaxed);
    }
    record.ledger.calls[record.id].fetch_add(1, Ordering::Relaxed);
    // This record was never published to readers. Drop releases only its own
    // storage and ledger reference, both valid in non-sleepable callback context.
    drop(record);
}

/// # Safety
/// Consume once the same Box projection as invoke, but only while unsubmitted.
/// Neither the queue nor a callback may access this node.
unsafe fn cancel(node: *mut CallbackNode) {
    // SAFETY: the unique unsubmitted handle retains the original Box owner.
    let record = unsafe { Box::from_raw(node.cast::<Record>()) };
    record.ledger.cancelled.fetch_add(1, Ordering::Relaxed);
    drop(record);
}

fn prepare(id: usize, ledger: &Arc<Ledger>) -> PreparedCallback {
    let record = Box::into_raw(Box::new(Record {
        node: CallbackNode::new(invoke),
        id,
        ledger: ledger.clone(),
    }));
    let live = ledger.live.fetch_add(1, Ordering::Relaxed) + 1;
    ledger.peak_live.fetch_max(live, Ordering::Relaxed);
    // SAFETY: raw projection preserves the enclosing live Box's provenance.
    let node = unsafe { &raw mut (*record).node };
    // SAFETY: an initialized field of the live Box allocation is non-null.
    let node = unsafe { NonNull::new_unchecked(node) };
    // SAFETY: the initialized Box field is non-null, uniquely owned and stable;
    // cancel/invoke consume that owner once and never block. No protected data
    // was published. Ledger storage survives both paths and permits transfer.
    unsafe { PreparedCallback::new(node, cancel) }
}

pub(super) fn run() {
    let mut cpus = Vec::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
    assert!(!cpus.is_empty());
    let total = cpus.len() * PER_PRODUCER;
    let ledger = Arc::new(Ledger {
        calls: (0..total).map(|_| AtomicUsize::new(0)).collect(),
        drops: (0..total).map(|_| AtomicUsize::new(0)).collect(),
        accepted: AtomicUsize::new(0),
        cancelled: AtomicUsize::new(0),
        live: AtomicUsize::new(0),
        peak_live: AtomicUsize::new(0),
        errors: AtomicUsize::new(0),
    });
    let mut producers = Vec::new();
    for (producer, cpu) in cpus.into_iter().enumerate() {
        let ledger = ledger.clone();
        let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || {
                if khal::percpu::this_cpu_id() != cpu {
                    ledger.errors.fetch_add(1, Ordering::Relaxed);
                }
                for index in 0..PER_PRODUCER {
                    let prepared = prepare(producer * PER_PRODUCER + index, &ledger);
                    // SAFETY: unpublished unique records; permanent callback
                    // code and boot service, nonblocking invocation, no held
                    // RCU/scheduler locks. No node access follows acceptance.
                    match unsafe { prepared.submit() } {
                        Ok(()) => {
                            ledger.accepted.fetch_add(1, Ordering::Relaxed);
                        }
                        Err((_error, returned)) => {
                            drop(returned);
                        }
                    }
                }
            },
            "rcu-callback-producer".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        task.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu));
        ktask::activate_task(&task);
        producers.push(task);
    }
    // Invocation can precede submit's return, so acceptance and call counters
    // are compared only after producer joins and the following drain.
    let statuses: Vec<_> = producers.iter().map(|task| task.join()).collect();
    // SAFETY: all producers joined, no reposts or downstream work, sleepable
    // watched task context and no locks. Ledger and callback code remain live.
    let drained = unsafe { krcu::drain_callbacks() };
    let accepted = ledger.accepted.load(Ordering::Relaxed);
    let cancelled = ledger.cancelled.load(Ordering::Relaxed);
    let calls: usize = ledger
        .calls
        .iter()
        .map(|count| count.load(Ordering::Relaxed))
        .sum();
    let drops: usize = ledger
        .drops
        .iter()
        .map(|count| count.load(Ordering::Relaxed))
        .sum();
    let live = ledger.live.load(Ordering::Relaxed);
    let peak_live = ledger.peak_live.load(Ordering::Relaxed);
    let errors = ledger.errors.load(Ordering::Relaxed);
    klogger::kprintln!(
        "RCU_CALLBACK_BATCH accepted={accepted}/{total} calls={calls} drops={drops} \
         cancelled={cancelled} live={live} peak_live={peak_live} errors={errors}"
    );
    assert!(drained.is_ok() && statuses.iter().all(|status| *status == 0));
    assert_eq!(accepted, total);
    assert_eq!(cancelled, 0);
    assert_eq!(errors, 0);
    assert_eq!(live, 0);
    assert!((1..=total).contains(&peak_live));
    for id in 0..total {
        assert_eq!(ledger.calls[id].load(Ordering::Relaxed), 1);
        assert_eq!(ledger.drops[id].load(Ordering::Relaxed), 1);
    }
}
