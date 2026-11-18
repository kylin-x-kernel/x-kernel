// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kirq::{IrqDesc, tests_irq::SyntheticLine};
use kvfs::{SimpleFs, SuperBlockFlags};
use unittest::{assert, assert_eq, def_test};

use super::{IrqNamespace, LineRecord};

fn namespace() -> Arc<IrqNamespace> {
    let fs = crate::new_procfs(SuperBlockFlags::empty()).unwrap();
    let simple = fs.private::<Arc<SimpleFs>>().unwrap().clone();
    Arc::new(IrqNamespace::new(simple))
}

#[def_test(serial)]
fn retirement_callback_drops_the_strong_record() {
    let namespace = namespace();
    let line = SyntheticLine::new(IrqDesc::from_virq(0x7120));
    let record = namespace.ensure_record(line.virq()).unwrap();
    assert_eq!(namespace.record_count(), 1);
    line.retire_for_tests();
    assert_eq!(namespace.record_count(), 0);
    drop(record);
}

#[def_test(serial)]
#[expect(
    clippy::used_underscore_binding,
    reason = "the test deliberately holds the underscore-named retirement observer to simulate a \
              delayed callback after Line retirement"
)]
fn delayed_old_notification_cannot_remove_successor() {
    let namespace = namespace();
    let virq = 0x7121;
    let first = SyntheticLine::new(IrqDesc::from_virq(virq));
    let old_record = namespace.ensure_record(virq).unwrap();
    let delayed = old_record._observer.clone();
    first.retire_for_tests();
    assert_eq!(namespace.record_count(), 0);

    let second = SyntheticLine::new(IrqDesc::from_virq(virq));
    let successor = namespace.ensure_record(virq).unwrap();
    assert_eq!(namespace.record_count(), 1);
    delayed.on_line_retired(virq);
    assert_eq!(namespace.record_count(), 1);
    assert!(
        successor
            .handle
            .is_same_line(&namespace.records.lock().get(&virq).unwrap().handle)
    );
    drop(second);
}

fn candidate(namespace: &Arc<IrqNamespace>, line: &SyntheticLine) -> Arc<LineRecord> {
    let handle = kirq::lookup_irq_line(line.virq()).unwrap().unwrap();
    namespace
        .build_record(line.virq(), Arc::new(handle))
        .unwrap()
}

// These tests deterministically order pre-built candidates; they do not run
// two threads. Real concurrent pressure remains an integration-test concern.
#[def_test]
fn prebuilt_same_line_candidates_reuse_one_inode_set() {
    let namespace = namespace();
    let line = SyntheticLine::new(IrqDesc::from_virq(0x7122));
    let first = candidate(&namespace, &line);
    let second = candidate(&namespace, &line);

    let published = namespace.commit_candidate(line.virq(), first).unwrap();
    let reused = namespace.commit_candidate(line.virq(), second).unwrap();
    assert!(Arc::ptr_eq(&published, &reused));
    assert_eq!(namespace.record_count(), 1);
    assert_eq!(published.dir.inode(), reused.dir.inode());
}

#[def_test]
fn stale_candidate_without_published_successor_keeps_map_empty() {
    let namespace = namespace();
    let virq = 0x7124;
    let first = SyntheticLine::new(IrqDesc::from_virq(virq));
    let stale = candidate(&namespace, &first);
    first.retire_for_tests();

    let second = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(
        namespace.commit_candidate(virq, stale).err(),
        Some(kvfs::VfsError::NotFound)
    );
    assert_eq!(namespace.record_count(), 0);
    let successor = namespace.ensure_record(virq).unwrap();
    assert_eq!(namespace.record_count(), 1);
    assert!(
        successor
            .handle
            .is_same_line(&kirq::lookup_irq_line(virq).unwrap().unwrap())
    );
    drop(second);
}

#[def_test]
fn stale_a_candidate_cannot_replace_published_b() {
    let namespace = namespace();
    let virq = 0x7123;
    let first = SyntheticLine::new(IrqDesc::from_virq(virq));
    let stale = candidate(&namespace, &first);
    first.retire_for_tests();

    let second = SyntheticLine::new(IrqDesc::from_virq(virq));
    let successor = namespace.ensure_record(virq).unwrap();
    let reused = namespace.commit_candidate(virq, stale).unwrap();
    assert!(Arc::ptr_eq(&successor, &reused));
    assert_eq!(namespace.record_count(), 1);
    assert!(
        namespace
            .records
            .lock()
            .get(&virq)
            .unwrap()
            .handle
            .is_same_line(&kirq::lookup_irq_line(virq).unwrap().unwrap())
    );
    drop(second);
}
