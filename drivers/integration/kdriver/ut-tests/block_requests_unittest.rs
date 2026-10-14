// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real host wait/softirq integration with a queue-aware fake virtio device.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use block::{BlockDeviceOperations, completion::BlockCompletionOperations};
use device_res::IrqHandler;
use kirq::context::test_support::ScopedHardIrqContext;
use unittest::{assert_eq, def_test};

use crate::{block_completion::prepare_block_wait, block_completion_dispatch::BlockIoReclaimer};

#[def_test(serial)]
fn virtio_requests_sleep_across_cpus_and_reclaim_through_block_softirq() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let (disk, hardware) = virtio::mock_virtio::block_test_disk(0, false, prepare_block_wait);
    let operations: Arc<dyn BlockCompletionOperations> = disk.clone();
    let reclaimer = BlockIoReclaimer::new(Arc::downgrade(&operations));
    let returned = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for index in 0..24 {
        let disk = disk.clone();
        let returned = returned.clone();
        let task = ktask::TaskInner::new_kthread(
            move || {
                let mut buffer = [0u8; 512];
                disk.read_block(index, &mut buffer).unwrap();
                core::assert!(buffer.iter().all(|byte| *byte == 0xa5));
                returned.fetch_add(1, Ordering::Release);
            },
            "blk-request-test".into(),
            0x8000,
        )
        .unwrap();
        let mut mask = ktask::KCpuMask::new();
        mask.set(index as usize % 2, true);
        task.set_cpumask(mask);
        tasks.push(ktask::spawn_task(task));
    }
    // With no used entries, five callers await terminal completion and the
    // other nineteen await FIFO admission. Neither group can busy-poll here.
    while !tasks
        .iter()
        .all(|task| task.state() == ktask::TaskState::Blocked)
    {
        ktask::yield_now();
    }
    assert_eq!(hardware.lock().submitted_count(), 5);
    assert_eq!(returned.load(Ordering::Acquire), 0);
    let mut completed = 0;
    while completed < 24 {
        let batch = hardware.lock().complete_pending();
        completed += batch;
        if batch != 0 {
            {
                let _irq = ScopedHardIrqContext::enter();
                core::assert!(disk.handle(0).handled());
                reclaimer.mark_pending();
            }
            kirq::softirq::run_pending_softirqs();
        }
        ktask::yield_now();
    }
    for task in tasks {
        assert_eq!(task.join(), 0);
    }
    assert_eq!(returned.load(Ordering::Acquire), 24);
    disk.disable_interrupts();
    let mut waiter = prepare_block_wait().unwrap();
    reclaimer.stop_and_wait(waiter.as_mut());
}
