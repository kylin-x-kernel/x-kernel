// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[test]
fn concurrent_start_cannot_lose_an_outgoing_reader() {
    extern crate std;
    let node = ReaderNode::new();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let reader = TaskReader::new();
            for _ in 0..32 {
                reader.with_read(|_| {
                    barrier.wait();
                    let _guard = kspin::NoPreemptIrqSave::new();
                    // SAFETY: this thread alone enrolls/exits its stable reader;
                    // the other thread only drives period state under node locks.
                    assert_eq!(unsafe { node.note_context_switch(&reader) }, None);
                    barrier.wait();
                    barrier.wait();
                });
                barrier.wait();
            }
        });
        for period in 0..32 {
            barrier.wait();
            assert!(node.begin_period(period));
            barrier.wait();
            assert_eq!(node.reader_count(), 1);
            assert_eq!(node.ready_period(), None);
            assert!(!node.finish_period(period));
            barrier.wait();
            barrier.wait();
            // When enrollment won the race, CPU observation is still owed.
            // This fixture supplies a fresh boundary only after scope exit.
            let inactive = TaskReader::new();
            let _guard = kspin::NoPreemptIrqSave::new();
            // SAFETY: worker scope has ended; no other scheduler touches this
            // inactive reader, and the next period starts only on this thread.
            unsafe { node.note_context_switch(&inactive) };
            assert_eq!(node.ready_period(), Some(period));
            assert!(node.finish_period(period));
        }
    });
}
