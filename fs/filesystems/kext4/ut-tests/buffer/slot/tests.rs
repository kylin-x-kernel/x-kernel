// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Atomic buffer edits without the mounted core's outer lock.

use unittest::def_test;

use super::*;

#[def_test]
fn edit_holds_slot_protection_until_publication() {
    let slot = Arc::new(MetadataBufferSlot::new());
    slot.publish(Arc::from([0u8; 16]));
    let access = slot.begin_write_access(TransactionId::new(1)).unwrap();
    let result = access.update_bytes(|old, new| {
        assert!(slot.contents.try_lock().is_none());
        assert_eq!(old[0], 0);
        new[0] = 7;
        Ok(23)
    });
    assert_eq!(result, Ok(23));
    assert_eq!(slot.result().unwrap().as_ref()[0], 7);
}

#[def_test]
fn failed_edit_preserves_bytes_and_transaction_state() {
    let slot = Arc::new(MetadataBufferSlot::new());
    slot.publish(Arc::from([0u8; 16]));
    let transaction = TransactionId::new(2);
    let access = slot.begin_write_access(transaction).unwrap();
    let result: Ext4Result<()> = access.update_bytes(|_, new| {
        new[0] = 9;
        Err(Ext4Error::OutOfBounds)
    });
    assert_eq!(result, Err(Ext4Error::OutOfBounds));
    assert_eq!(slot.result().unwrap().as_ref()[0], 0);
    assert_eq!(
        slot.state().unwrap(),
        MetadataBufferState::Journaled(transaction)
    );
}

#[def_test]
fn concurrent_edits_preserve_both_writers_changes() {
    let slot = Arc::new(MetadataBufferSlot::new());
    slot.publish(Arc::from([0u8; 16]));
    let ready = Arc::new(core::sync::atomic::AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for writer in 0..2 {
        let slot = slot.clone();
        let ready = ready.clone();
        tasks.push(ktask::spawn(move || {
            let access = slot.begin_write_access(TransactionId::new(3)).unwrap();
            ready.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
            while ready.load(core::sync::atomic::Ordering::Acquire) != 2 {
                ktask::yield_now();
            }
            for _ in 0..64 {
                access
                    .update_bytes(|old, new| {
                        new[writer] = old[writer] + 1;
                        Ok(())
                    })
                    .unwrap();
            }
        }));
    }
    for task in tasks {
        task.join();
    }
    assert_eq!(&slot.result().unwrap().as_ref()[..2], &[64, 64]);
}
