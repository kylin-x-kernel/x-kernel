// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Exercise actual withdrawal and callback recovery without fabricating RCU.
//! Fixtures explicitly end every old borrow before invoking eligible callbacks.

use super::*;
use crate::XArray;

#[test]
fn detached_path_survives_reinsertion_until_its_own_callbacks() {
    let array = XArray::<Box<usize>>::new();
    for key in [0, 64, usize::MAX, 0, usize::MAX] {
        // SAFETY: host fixture, primitive nonblocking payload and allocator.
        assert!(unsafe { array.insert(key, Box::new(7)) }.is_ok());
        let root = array.root.load(Ordering::Acquire);
        // SAFETY: private fixture retains all withdrawn storage until below.
        let old_root = unsafe { &*root };
        // SAFETY: no reclamation occurs while this test holds raw borrows.
        let payload = unsafe { array.lookup(key) }.cast::<usize>();
        // SAFETY: live Box payload, not freed until after the final borrow use.
        let borrowed = unsafe { &*payload };
        let (old, detached) = {
            let _guard = array.writers.lock();
            // SAFETY: this private fixture inserted the key and has no other
            // writers. The guard serializes withdrawal; old borrows end below
            // before the fixture releases the payload and detached nodes.
            unsafe { array.withdraw(key) }
        };
        assert_eq!(
            detached.iter().filter(|raw| !raw.is_null()).count(),
            height_for(key) as usize
        );
        assert!(array.root.load(Ordering::Acquire).is_null());
        // SAFETY: new publication is independent of the detached allocations.
        assert!(unsafe { array.insert(key, Box::new(9)) }.is_ok());
        assert_ne!(array.root.load(Ordering::Acquire), root);
        assert!(
            old_root
                .slots
                .iter()
                .all(|slot| slot.load(Ordering::Acquire).is_null())
        );
        assert_eq!(*borrowed, 7);
        assert_eq!(old, payload.cast());
        // SAFETY: all old payload borrows ended; this fixture owns withdrawal.
        drop(unsafe { Box::from_raw(old.cast::<usize>()) });
        for raw in detached.into_iter().filter(|raw| !raw.is_null()) {
            // SAFETY: project from the original allocation; no old node borrows
            // remain. The detached node has never been submitted to an executor.
            let callback = unsafe { &raw mut (*raw).callback };
            // SAFETY: fixture establishes GP eligibility by ending all borrows.
            unsafe { release(callback) };
        }
        // Withdraw the newly published version so the next iteration starts empty.
        let (old, detached) = {
            let _guard = array.writers.lock();
            // SAFETY: reinsertion succeeded in this private fixture, the writer
            // guard is held, and no readers of the new version exist.
            unsafe { array.withdraw(key) }
        };
        // SAFETY: no borrows of this new payload exist, and withdrawal owns it.
        drop(unsafe { Box::from_raw(old.cast::<usize>()) });
        for raw in detached.into_iter().filter(|raw| !raw.is_null()) {
            // SAFETY: raw field projection from this uniquely withdrawn node.
            let callback = unsafe { &raw mut (*raw).callback };
            // SAFETY: no readers exist, and this is the sole callback invocation.
            unsafe { release(callback) };
        }
    }
}
