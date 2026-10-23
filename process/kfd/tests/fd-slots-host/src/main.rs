// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host / Miri fixture for the real descriptor slot storage.
//!
//! `FileSlots` publishes slot occupancy through a bitmap while the values live
//! in `MaybeUninit` storage, so its four `assume_init_*` calls depend on one
//! ordering rule: a set bit always means the slot already holds a value.
//! `place` therefore writes before setting the bit, `clone_from` writes every
//! clone before publishing the copied bitmap, and `clear` unpublishes the
//! bitmap before dropping values.
//!
//! The kernel unit tests cannot pin that rule: they only instantiate
//! `FileSlots<FileDescriptor>`, whose `Clone` and `Drop` (an `Arc` clone and
//! drop) never panic, so an inverted publish order stays invisible. This binary
//! compiles the real `process/kfd/src/fd_slots.rs` and exercises the same paths
//! with a `T` whose `Clone` and `Drop` unwind. Run it as described in
//! `README.md`.

// The storage refers to the kernel limit crate by name. Aliasing this crate
// under that name keeps the fixture dependency-free, the same way
// `arch/kirq/tests/irq-work-host` covers its kernel dependencies.
extern crate self as krlimit;

/// Mirrors `krlimit::FILE_LIMIT`. The checks below read capacity through this
/// value, so it must stay in sync with `process/krlimit/src/lib.rs`; the exact
/// value only scales the fixture, it does not affect what is asserted.
pub const FILE_LIMIT: usize = 1024;

#[path = "../../../src/fd_slots.rs"]
mod fd_slots;

use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;

use fd_slots::FileSlots;

/// Descriptor capacity of the storage under test.
const CAP: usize = FILE_LIMIT;

/// Random seeds for the differential checks, trimmed under Miri.
const SEEDS: u64 = if cfg!(miri) { 4 } else { 40 };
/// Operation steps per seed for the differential checks, trimmed under Miri.
const STEPS: usize = if cfg!(miri) { 80 } else { 400 };

/// Deterministic xorshift64* generator, so every run replays exactly.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

/// Occupancy reported by the storage, in ascending id order.
fn ids<T>(slots: &FileSlots<T>) -> Vec<usize> {
    slots.ids().collect()
}

/// Reads the free-slot bound when built with `--cfg unittest`.
///
/// The bound is asserted against allocation results in every other build, so
/// the fixture runs without extra flags.
#[cfg(unittest)]
fn bound<T>(slots: &FileSlots<T>) -> Option<usize> {
    Some(slots.next_free())
}

#[cfg(not(unittest))]
fn bound<T>(_slots: &FileSlots<T>) -> Option<usize> {
    None
}

/// Checks the free-slot bound still covers every free id below it.
fn check_bound<T>(slots: &FileSlots<T>, occupied: &[bool], context: &str) {
    let Some(bound) = bound(slots) else {
        return;
    };
    assert!(bound <= CAP, "{context}: bound {bound} exceeds capacity");
    for (id, &is_occupied) in occupied.iter().enumerate().take(bound) {
        assert!(is_occupied, "{context}: bound {bound} skips free id {id}");
    }
}

/// Allocation, insertion and removal against brute force on a dense table.
///
/// Brute force knows nothing about the bitmap or the bound, so a bound that
/// skipped a lower free id would change an allocation result.
fn check_allocation() -> usize {
    let mut cases = 0;
    for seed in 1..=SEEDS {
        let mut rng = Rng(seed);
        let mut slots = FileSlots::<usize>::new();
        let mut occupied = vec![false; CAP];
        // Start dense so fully occupied bitmap words are the common case.
        for (id, is_occupied) in occupied.iter_mut().enumerate() {
            if rng.next() & 3 != 0 {
                slots.add_at(id, id).unwrap();
                *is_occupied = true;
            }
        }

        for step in 0..STEPS {
            let context = format!("seed={seed} step={step}");
            match rng.below(6) {
                0..=2 => {
                    let minimum = rng.below(CAP + 2);
                    let end = rng.below(CAP + 2);
                    let want = (minimum..end.min(CAP)).find(|&fd| !occupied[fd]);
                    assert_eq!(
                        slots.alloc(minimum, end, 0).ok(),
                        want,
                        "alloc {context} min={minimum} end={end}"
                    );
                    if let Some(fd) = want {
                        occupied[fd] = true;
                    }
                }
                3 => {
                    // `add` assigns the lowest free id overall.
                    let want = (0..CAP).find(|&fd| !occupied[fd]);
                    assert_eq!(slots.add(0).ok(), want, "add {context}");
                    if let Some(fd) = want {
                        occupied[fd] = true;
                    }
                }
                4 => {
                    let fd = rng.below(CAP);
                    assert_eq!(slots.remove(fd).is_some(), occupied[fd], "remove {context}");
                    occupied[fd] = false;
                }
                _ => {
                    let fd = rng.below(CAP);
                    assert_eq!(slots.add_at(fd, 0).is_ok(), !occupied[fd], "add_at {context}");
                    occupied[fd] = true;
                }
            }

            let expected: Vec<usize> = (0..CAP).filter(|&id| occupied[id]).collect();
            assert_eq!(ids(&slots), expected, "occupancy {context}");
            assert_eq!(slots.count(), expected.len(), "count {context}");
            check_bound(&slots, &occupied, &context);
            cases += 1;
        }
    }
    cases
}

/// Both iterator directions, including interleaved consumption.
fn check_ids_iteration() -> usize {
    let mut cases = 0;
    for seed in 1..=SEEDS {
        let mut rng = Rng(seed);
        let mut slots = FileSlots::<usize>::new();
        let occupied: Vec<bool> = (0..CAP).map(|_| rng.next() & 1 == 1).collect();
        for (id, &set) in occupied.iter().enumerate() {
            if set {
                slots.add_at(id, id).unwrap();
            }
        }
        let expected: Vec<usize> = (0..CAP).filter(|&id| occupied[id]).collect();

        assert_eq!(ids(&slots), expected, "forward seed={seed}");
        assert_eq!(
            slots.ids().rev().collect::<Vec<_>>(),
            expected.iter().rev().copied().collect::<Vec<_>>(),
            "backward seed={seed}"
        );

        // Interleaved consumption must yield every id exactly once.
        let mut iter = slots.ids();
        let mut mixed = Vec::new();
        while let Some(front) = iter.next() {
            mixed.push(front);
            let Some(back) = iter.next_back() else {
                break;
            };
            mixed.push(back);
        }
        let mut sorted = mixed.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, expected, "interleaved seed={seed}");
        cases += 1;
    }
    cases
}

/// `T` with observable clones and drops and configurable unwinding.
#[derive(Debug)]
struct Counted {
    id: usize,
    drops: Rc<Cell<usize>>,
    clones: Rc<Cell<usize>>,
    panic_on_clone: usize,
    panic_on_drop: usize,
}

impl Counted {
    fn new(
        id: usize,
        drops: &Rc<Cell<usize>>,
        clones: &Rc<Cell<usize>>,
        panic_on_clone: usize,
        panic_on_drop: usize,
    ) -> Self {
        Self {
            id,
            drops: Rc::clone(drops),
            clones: Rc::clone(clones),
            panic_on_clone,
            panic_on_drop,
        }
    }
}

impl Clone for Counted {
    fn clone(&self) -> Self {
        let n = self.clones.get() + 1;
        self.clones.set(n);
        assert!(n != self.panic_on_clone, "clone {n} unwinds");
        Self::new(
            self.id,
            &self.drops,
            &self.clones,
            self.panic_on_clone,
            self.panic_on_drop,
        )
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
        assert!(self.id + 1 != self.panic_on_drop, "drop of {} unwinds", self.id);
    }
}

/// Reading assigned slots through `get`/`get_mut`, and refusing free ones.
///
/// `get` and `get_mut` are the readers justified by "a set bit means the slot
/// holds a value", so an inverted publish order must fail here as well.
fn check_reads() -> usize {
    let drops = Rc::new(Cell::new(0));
    let clones = Rc::new(Cell::new(0));
    let assigned = [0usize, 3, CAP - 1];
    let mut slots = FileSlots::new();
    for id in assigned {
        slots
            .add_at(id, Counted::new(id, &drops, &clones, usize::MAX, usize::MAX))
            .unwrap();
    }

    for id in 0..CAP {
        match slots.get(id) {
            Some(value) => assert_eq!(value.id, id, "get({id}) reads the stored value"),
            None => assert!(
                !assigned.contains(&id),
                "get({id}) must not read a free slot"
            ),
        }
    }

    // `get_mut` mutates in place without changing occupancy.
    let value = slots.get_mut(3).expect("assigned slot is mutable");
    value.id = 3;
    assert_eq!(slots.count(), 3);
    assert_eq!(ids(&slots), vec![0, 3, CAP - 1]);

    drop(slots);
    assert_eq!(drops.get(), 3, "each value is dropped once");
    1
}

/// `clone_from` copies values, replaces previous ones, and drops each once.
fn check_clone_from_counts() -> usize {
    let drops = Rc::new(Cell::new(0));
    let clones = Rc::new(Cell::new(0));
    let mut source = FileSlots::new();
    for id in [0usize, 5, 17, CAP - 1] {
        source
            .add_at(id, Counted::new(id, &drops, &clones, usize::MAX, usize::MAX))
            .unwrap();
    }

    let mut target = FileSlots::new();
    target
        .add_at(3, Counted::new(3, &drops, &clones, usize::MAX, usize::MAX))
        .unwrap();

    target.clone_from(&source);
    assert_eq!(drops.get(), 1, "clear drops the previous occupant once");
    assert_eq!(clones.get(), 4, "values are cloned, not moved");
    assert_eq!(ids(&target), vec![0, 5, 17, CAP - 1]);
    assert_eq!(target.count(), 4);

    drop(target);
    assert_eq!(drops.get(), 5, "each copied value is dropped once");
    drop(source);
    assert_eq!(drops.get(), 9);
    1
}

/// A panicking `Clone` must leave nothing published.
///
/// With the write-before-publish order the target stays empty, so dropping it
/// touches no slot; an inverted order leaves set bits for slots that were never
/// written, which reads uninitialized memory and drops it.
fn check_clone_panic_leaves_nothing_published() -> usize {
    let mut cases = 0;
    for panic_on_clone in 1..=5usize {
        let drops = Rc::new(Cell::new(0));
        let clones = Rc::new(Cell::new(0));
        let mut source = FileSlots::new();
        for id in 0..5usize {
            source
                .add_at(
                    id,
                    Counted::new(id, &drops, &clones, panic_on_clone, usize::MAX),
                )
                .unwrap();
        }

        let mut target = FileSlots::new();
        let result = catch_unwind(AssertUnwindSafe(|| target.clone_from(&source)));
        assert!(result.is_err(), "clone must unwind at {panic_on_clone}");

        assert_eq!(target.count(), 0, "nothing may be published");
        assert!(ids(&target).is_empty(), "no slot may be published");
        let before_drop = drops.get();
        drop(target);
        assert_eq!(
            drops.get(),
            before_drop,
            "unpublished clones must not be dropped"
        );

        drop(source);
        assert_eq!(drops.get(), before_drop + 5, "source values drop once");
        cases += 1;
    }
    cases
}

/// A panicking `Drop` while `clone_from` clears the target must leave it empty.
///
/// `clear` unpublishes the bitmap before dropping, so a later drop of the same
/// target cannot drop an already dropped slot a second time.
fn check_drop_panic_during_clear() -> usize {
    let drops = Rc::new(Cell::new(0));
    let clones = Rc::new(Cell::new(0));

    // The value in slot 0 unwinds when it is dropped.
    let mut target = FileSlots::new();
    target
        .add_at(0, Counted::new(0, &drops, &clones, usize::MAX, 1))
        .unwrap();
    target
        .add_at(1, Counted::new(1, &drops, &clones, usize::MAX, usize::MAX))
        .unwrap();

    let mut source = FileSlots::new();
    source
        .add_at(7, Counted::new(7, &drops, &clones, usize::MAX, usize::MAX))
        .unwrap();

    let result = catch_unwind(AssertUnwindSafe(|| target.clone_from(&source)));
    assert!(result.is_err(), "clearing the target must unwind");
    assert_eq!(target.count(), 0, "the target must not claim dropped slots");
    assert!(ids(&target).is_empty(), "no slot may stay published");

    let before_drop = drops.get();
    drop(target);
    assert_eq!(drops.get(), before_drop, "no slot may be dropped twice");
    drop(source);
    assert_eq!(drops.get(), before_drop + 1);
    1
}

fn main() {
    // Panics from `Counted::clone` / `Counted::drop` are the subject of two
    // checks and are caught below; keep their messages out of the output.
    std::panic::set_hook(Box::new(|_| {}));

    let mut cases = 0;
    cases += check_allocation();
    cases += check_ids_iteration();
    cases += check_reads();
    cases += check_clone_from_counts();
    cases += check_clone_panic_leaves_nothing_published();
    cases += check_drop_panic_during_clear();

    println!(
        "PASS: {cases} slot-storage cases, capacity {CAP}; publish order holds under panicking \
         Clone and Drop"
    );
}