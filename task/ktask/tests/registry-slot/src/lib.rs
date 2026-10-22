// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(test)]
#[path = "../../../src/task_registry/slot.rs"]
mod slot;

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };

    use super::slot::Slot;

    struct Payload {
        value: usize,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for Payload {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn retired_generation_remains_readable_until_last_guard() {
        let drops = Arc::new(AtomicUsize::new(0));
        let slot = Slot::new();
        assert!(
            slot.try_insert(Payload {
                value: 19,
                drops: drops.clone()
            })
            .is_ok()
        );
        let first = slot.read().unwrap();
        let second = slot.read().unwrap();
        slot.retire();
        assert!(slot.read().is_none());
        assert_eq!(first.value, 19);
        assert_eq!(second.value, 19);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(first);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(second);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(
            slot.try_insert(Payload {
                value: 23,
                drops: drops.clone()
            })
            .is_ok()
        );
        assert_eq!(slot.read().unwrap().value, 23);
        slot.retire();
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn remote_retirement_does_not_wait_for_paused_reader() {
        let slot = Arc::new(Slot::<Payload>::new());
        let drops = Arc::new(AtomicUsize::new(0));
        assert!(
            slot.try_insert(Payload {
                value: 41,
                drops: drops.clone()
            })
            .is_ok()
        );
        let acquired = Arc::new(Barrier::new(2));
        let retired = Arc::new(Barrier::new(2));
        let reader = {
            let slot = slot.clone();
            let acquired = acquired.clone();
            let retired = retired.clone();
            std::thread::spawn(move || {
                let guard = slot.read().unwrap();
                acquired.wait();
                retired.wait();
                assert_eq!(guard.value, 41);
            })
        };
        acquired.wait();
        slot.retire();
        assert!(slot.read().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        retired.wait();
        reader.join().unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(
            slot.try_insert(Payload {
                value: 42,
                drops: drops.clone()
            })
            .is_ok()
        );
        slot.retire();
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn readers_race_repeated_publication_without_duplicate_destruction() {
        let slot = Arc::new(Slot::<Payload>::new());
        let drops = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(3));
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let slot = slot.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    for _ in 0..128 {
                        if let Some(guard) = slot.read() {
                            assert!(guard.value < 32);
                            std::thread::yield_now();
                            assert!(guard.value < 32);
                        }
                    }
                })
            })
            .collect();
        start.wait();
        for value in 0..32 {
            let mut payload = Payload {
                value,
                drops: drops.clone(),
            };
            loop {
                match slot.try_insert(payload) {
                    Ok(()) => break,
                    Err(returned) => payload = returned,
                }
                std::thread::yield_now();
            }
            slot.retire();
        }
        for reader in readers {
            reader.join().unwrap();
        }
        assert_eq!(drops.load(Ordering::SeqCst), 32);
    }
}
