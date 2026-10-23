// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Publication litmus models, not a second XArray implementation.
//! Identity integers stand for pointers; retained Arc storage stands for GP
//! protection. Relaxed atomic data permits defined negative-control executions.

use loom::{
    sync::{
        Arc, Mutex,
        atomic::{
            AtomicUsize,
            Ordering::{self, *},
        },
    },
    thread,
};

#[derive(Default)]
struct Storage {
    root: AtomicUsize,
    height: AtomicUsize,
    child: AtomicUsize,
    leaf: AtomicUsize,
    value: AtomicUsize,
}

fn root_publication(publish: Ordering, observe: Ordering) {
    loom::model(move || {
        let data = Arc::new(Storage::default());
        let writer_data = data.clone();
        let writer = thread::spawn(move || {
            writer_data.height.store(1, Relaxed);
            writer_data.value.store(23, Relaxed);
            writer_data.leaf.store(1, Release);
            writer_data.root.store(1, publish);
        });
        if data.root.load(observe) == 1 {
            assert_eq!(data.height.load(Relaxed), 1, "published initialization");
            assert_eq!(data.leaf.load(Acquire), 1, "published initialization");
            assert_eq!(data.value.load(Relaxed), 23, "published initialization");
        }
        writer.join().unwrap();
    });
}

#[test]
fn root_release_acquire_exposes_initialized_node() {
    root_publication(Release, Acquire);
}

#[test]
#[should_panic(expected = "published initialization")]
fn relaxed_root_publication_is_detected() {
    root_publication(Relaxed, Acquire);
}

#[test]
#[should_panic(expected = "published initialization")]
fn relaxed_root_observation_is_detected() {
    root_publication(Release, Relaxed);
}

fn child_then_payload(child_publish: Ordering) {
    loom::model(move || {
        let data = Arc::new(Storage::default());
        let writer_data = data.clone();
        let writer = thread::spawn(move || {
            // The offered owner is initialized before publish starts linking.
            writer_data.value.store(23, Relaxed);
            writer_data.height.store(1, Relaxed);
            writer_data.child.store(1, child_publish);
            writer_data.leaf.store(1, Release);
        });
        if data.child.load(Acquire) == 1 {
            assert_eq!(data.height.load(Relaxed), 1, "child initialization");
            // Missing payload is allowed while the writer is still inserting.
            if data.leaf.load(Acquire) == 1 {
                assert_eq!(data.value.load(Relaxed), 23, "payload initialization");
            }
        }
        writer.join().unwrap();
    });
}

#[test]
fn reachable_child_can_precede_payload_installation() {
    child_then_payload(Release);
}

#[test]
#[should_panic(expected = "child initialization")]
fn relaxed_child_publication_is_detected() {
    child_then_payload(Relaxed);
}

fn payload_in_existing_branch(publish: Ordering) {
    loom::model(move || {
        let data = Arc::new(Storage::default());
        // A branch retained by a sibling already exists. Publishing it cannot
        // convey initialization of an owner created by this later insertion.
        data.height.store(1, Relaxed);
        data.child.store(1, Release);
        let writer_data = data.clone();
        let writer = thread::spawn(move || {
            writer_data.value.store(23, Relaxed);
            writer_data.leaf.store(1, publish);
        });
        if data.child.load(Acquire) == 1 && data.leaf.load(Acquire) == 1 {
            assert_eq!(data.value.load(Relaxed), 23, "payload initialization");
        }
        writer.join().unwrap();
    });
}

#[test]
fn existing_branch_publishes_new_payload_initialization() {
    payload_in_existing_branch(Release);
}

#[test]
#[should_panic(expected = "payload initialization")]
fn relaxed_payload_publication_is_detected() {
    payload_in_existing_branch(Relaxed);
}

fn grow_after_writer_handoff(publish: Ordering) {
    loom::model(move || {
        let data = Arc::new(Storage::default());
        let writers = Arc::new(Mutex::new(()));
        let first_data = data.clone();
        let first_lock = writers.clone();
        let first = thread::spawn(move || {
            let _guard = first_lock.lock().unwrap();
            first_data.value.store(17, Relaxed);
            first_data.leaf.store(1, Release);
            first_data.root.store(1, Release);
        });
        let second_data = data.clone();
        let second = thread::spawn(move || {
            let _guard = writers.lock().unwrap();
            if second_data.root.load(Acquire) == 1 {
                second_data.height.store(2, Relaxed);
                second_data.child.store(1, Release);
                second_data.root.store(2, publish);
            }
        });
        if data.root.load(Acquire) == 2 {
            assert_eq!(data.height.load(Relaxed), 2, "grown root initialization");
            assert_eq!(data.child.load(Acquire), 1, "grown root initialization");
            assert_eq!(
                data.leaf.load(Acquire),
                1,
                "retained subtree initialization"
            );
            assert_eq!(
                data.value.load(Relaxed),
                17,
                "retained subtree initialization"
            );
        }
        first.join().unwrap();
        second.join().unwrap();
    });
}

#[test]
fn writer_handoff_and_root_growth_retain_initialized_subtree() {
    grow_after_writer_handoff(Release);
}

#[test]
#[should_panic(expected = "grown root initialization")]
fn relaxed_grown_root_publication_is_detected() {
    grow_after_writer_handoff(Relaxed);
}
