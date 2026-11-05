// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

use kalloc_types::KBox;
use unittest::{assert, assert_eq, def_test};

use crate::RcuSlot;

#[def_test]
fn optional_publication_returns_rejected_owner_and_drops_current_once() {
    struct Payload<'a>(&'a AtomicUsize);
    impl Drop for Payload<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let drops = AtomicUsize::new(0);
    let slot = RcuSlot::new();
    assert!(
        slot.try_publish(KBox::new_nowait(Payload(&drops)).unwrap())
            .is_ok()
    );
    let offered = KBox::new_nowait(Payload(&drops)).unwrap();
    let address = &*offered as *const _;
    let rejected = slot.try_publish(offered).err().unwrap();
    assert_eq!(&*rejected as *const _, address);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(rejected);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    drop(slot);
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

#[def_test]
fn optional_container_transfer_and_sharing_match_payload_requirements() {
    fn require_send<T: Send>() {}
    fn require_sync<T: Sync>() {}
    require_send::<RcuSlot<core::cell::Cell<u64>>>();
    require_send::<RcuSlot<u64>>();
    require_sync::<RcuSlot<u64>>();
}
