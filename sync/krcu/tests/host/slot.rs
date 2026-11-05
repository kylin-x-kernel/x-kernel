// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Startup publication needs no kernel RCU provider or fabricated read scope.

extern crate std;

use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use kalloc_types::KBox;

use super::RcuSlot;

struct Payload(Arc<AtomicUsize>);

impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn concurrent_first_publish_retains_one_owner_without_a_gp_service() {
    const WRITERS: usize = 4;
    let slot = Arc::new(RcuSlot::new());
    let barrier = Arc::new(Barrier::new(WRITERS));
    let drops = Arc::new(AtomicUsize::new(0));
    let tasks: std::vec::Vec<_> = (0..WRITERS)
        .map(|_| {
            let slot = slot.clone();
            let barrier = barrier.clone();
            let drops = drops.clone();
            thread::spawn(move || {
                let value = KBox::new_nowait(Payload(drops)).unwrap();
                let address = &*value as *const _;
                barrier.wait();
                match slot.try_publish(value) {
                    Ok(()) => 1,
                    Err(returned) => {
                        assert_eq!(&*returned as *const _, address);
                        drop(returned);
                        0
                    }
                }
            })
        })
        .collect();
    let winners: usize = tasks.into_iter().map(|task| task.join().unwrap()).sum();
    assert_eq!(winners, 1);
    assert_eq!(drops.load(Ordering::Relaxed), WRITERS - 1);
    drop(slot);
    assert_eq!(drops.load(Ordering::Relaxed), WRITERS);
}
