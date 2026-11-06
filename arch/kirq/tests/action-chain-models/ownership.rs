// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Finite ordering models mapped to production in README.md, not a second IRQ core.

use loom::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{
            AtomicUsize,
            Ordering::{self, Acquire, Relaxed, Release},
        },
        mpsc,
    },
    thread,
};

// A and B predate capture. Only B.next and the appended C's metadata need
// modeling: A's cached successor lets traversal reach B after the append.
fn append(publish: Ordering, observe: Ordering) {
    loom::model(move || {
        let next = Arc::new(AtomicUsize::new(0));
        let sequence = Arc::new(AtomicUsize::new(0));
        let payload = Arc::new(AtomicUsize::new(0));
        let writer_next = next.clone();
        let writer_sequence = sequence.clone();
        let writer_payload = payload.clone();
        let writer = thread::spawn(move || {
            writer_sequence.store(3, Relaxed);
            writer_payload.store(23, Relaxed);
            writer_next.store(3, publish);
        });
        let ceiling = 2;
        let mut visited = vec![1, 2];
        let mut initialized = true;
        if next.load(observe) == 3 {
            let observed = sequence.load(Relaxed);
            initialized = observed == 3 && payload.load(Relaxed) == 23;
            if observed <= ceiling {
                visited.push(3);
            }
        }
        writer.join().unwrap();
        assert!(initialized, "append initialization");
        assert_eq!(visited, [1, 2], "publication ceiling");
    });
}

#[test]
fn append_publishes_metadata_before_ceiling_read() {
    append(Release, Acquire);
}

#[test]
#[should_panic(expected = "append initialization")]
fn relaxed_append_store_is_detected() {
    append(Relaxed, Acquire);
}

#[test]
#[should_panic(expected = "append initialization")]
fn relaxed_successor_load_is_detected() {
    append(Release, Relaxed);
}

fn cached_successor(clear_detached_next: bool) {
    loom::model(move || {
        let a_next = Arc::new(AtomicUsize::new(2));
        let b_next = Arc::new(AtomicUsize::new(3));
        let (captured_tx, captured_rx) = mpsc::channel();
        let (unlinked_tx, unlinked_rx) = mpsc::channel();
        let reader_a = a_next.clone();
        let reader_b = b_next.clone();
        let reader = thread::spawn(move || {
            let cached = reader_a.load(Acquire);
            captured_tx.send(()).unwrap();
            unlinked_rx.recv().unwrap();
            (cached, reader_b.load(Acquire))
        });
        captured_rx.recv().unwrap();
        // This writer stands for the sole management transaction. Storage is
        // retained until the reader joins; lifetime is checked separately below.
        a_next.store(3, Release);
        if clear_detached_next {
            b_next.store(0, Release);
        }
        assert_eq!(a_next.load(Acquire), 3, "new traversal skips removed node");
        unlinked_tx.send(()).unwrap();
        let (cached, successor) = reader.join().unwrap();
        assert_eq!(cached, 2);
        assert_eq!(successor, 3, "detached successor preserved");
    });
}

#[test]
fn cached_middle_node_keeps_successor_after_unlink() {
    cached_successor(false);
}

#[test]
#[should_panic(expected = "detached successor preserved")]
fn clearing_detached_link_is_detected() {
    cached_successor(true);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    LateAdmission,
    SkipDrain,
    EarlyFinish,
}

struct Line {
    has_node: bool,
    running: bool,
}

// An observation oracle, not a production lock. It serializes logical access
// and destruction events, but never spans a callback or supplies drain authority.
// Allocation identity is fixed; backing Arc storage outlives logical reclamation.
#[derive(Default)]
struct Audit {
    reclaimed: bool,
    completed: bool,
    accesses: usize,
    accessed_after_reclaim: bool,
}

fn access(audit: &Mutex<Audit>) {
    let mut audit = audit.lock().unwrap();
    audit.accessed_after_reclaim |= audit.reclaimed;
    audit.accesses += 1;
}

fn release(line: &Mutex<Line>, idle: &Condvar) {
    let mut line = line.lock().unwrap();
    line.running = false;
    idle.notify_all();
}

fn lifetime(fault: Fault) {
    loom::model(move || {
        let line = Arc::new(Mutex::new(Line {
            has_node: true,
            running: false,
        }));
        let idle = Arc::new(Condvar::new());
        let audit = Arc::new(Mutex::new(Audit::default()));
        let management = Mutex::new(());
        // Capture and admission normally share this exact Line critical section.
        {
            let mut state = line.lock().unwrap();
            assert!(state.has_node);
            if fault != Fault::LateAdmission {
                state.running = true;
            }
        }
        let reader_line = line.clone();
        let reader_idle = idle.clone();
        let reader_audit = audit.clone();
        let reader = thread::spawn(move || {
            if fault == Fault::LateAdmission {
                let mut state = reader_line.lock().unwrap();
                state.running = true;
            }
            // Delayed metadata/link read and callback access are separate events.
            access(&reader_audit);
            access(&reader_audit);
            if fault == Fault::EarlyFinish {
                release(&reader_line, &reader_idle);
            }
            {
                let mut audit = reader_audit.lock().unwrap();
                audit.accessed_after_reclaim |= audit.reclaimed;
                audit.completed = true;
            }
            if fault != Fault::EarlyFinish {
                release(&reader_line, &reader_idle);
            }
        });
        let completed_at_reclaim = {
            let _management = management.lock().unwrap();
            let mut state = line.lock().unwrap();
            state.has_node = false;
            if fault != Fault::SkipDrain {
                while state.running {
                    state = idle.wait(state).unwrap();
                }
            }
            drop(state);
            // No speculative reclamation based on enabled bits or queue state.
            let mut observed = audit.lock().unwrap();
            let completed = observed.completed;
            observed.reclaimed = true;
            completed
        };
        reader.join().unwrap();
        assert!(
            completed_at_reclaim,
            "reclamation before original completion"
        );
        let observed = audit.lock().unwrap();
        assert!(!observed.accessed_after_reclaim, "access after reclamation");
        assert_eq!(observed.accesses, 2);
        assert!(observed.reclaimed);
    });
}

#[test]
fn running_capture_drain_covers_delayed_traversal_and_completion() {
    lifetime(Fault::None);
}

#[test]
#[should_panic(expected = "reclamation before original completion")]
fn admission_after_capture_is_detected() {
    lifetime(Fault::LateAdmission);
}

#[test]
#[should_panic(expected = "reclamation before original completion")]
fn omitted_drain_is_detected() {
    lifetime(Fault::SkipDrain);
}

#[test]
#[should_panic(expected = "reclamation before original completion")]
fn release_before_completion_is_detected() {
    lifetime(Fault::EarlyFinish);
}

#[test]
fn survivor_capture_after_unlink_does_not_reach_retired_node() {
    loom::model(|| {
        // Membership [B,C] becomes [C]. One old reader already owns B.
        let line = Arc::new(Mutex::new((2usize, true)));
        let idle = Arc::new(Condvar::new());
        let management = Mutex::new(());
        let _management = management.lock().unwrap();
        line.lock().unwrap().0 = 3;
        let survivor_line = line.clone();
        let survivor_idle = idle.clone();
        let survivor = thread::spawn(move || {
            let head = {
                let mut state = survivor_line.lock().unwrap();
                // Schedule the next accepted arrival after the old running
                // owner ends. Production dispatch rejects busy arrivals; this
                // wait selects an interleaving, not a blocking IRQ handler.
                while state.1 {
                    state = survivor_idle.wait(state).unwrap();
                }
                state.1 = true;
                state.0
            };
            assert_eq!(head, 3, "survivor cannot capture withdrawn node");
            survivor_line.lock().unwrap().1 = false;
        });
        // Management remains held but does not gate the surviving action.
        {
            let mut state = line.lock().unwrap();
            state.1 = false;
            idle.notify_all();
        }
        survivor.join().unwrap();
        assert!(!line.lock().unwrap().1);
    });
}
