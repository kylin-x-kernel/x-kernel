// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;
use vmobj::{MappingViewKind, MappingViewSpec};

use super::{AnonPrivateObject, AnonPrivatePageCommitError};

#[def_test]
fn anon_private_fork_child_keeps_lineage_and_gets_new_identity() {
    let parent = AnonPrivateObject::new_root();
    let child = parent.fork_child();

    assert_eq!(parent.lineage(), child.lineage());
    assert_ne!(parent.id(), child.id());
    assert_ne!(parent.anon_id(), child.anon_id());
}

#[def_test]
fn anon_private_registers_views_and_emits_object_hits() {
    let object = AnonPrivateObject::new_root();
    let _view = object.register_view(MappingViewSpec {
        mm_id: 9,
        vma_start: 0x9000,
        vma_len: 0x3000,
        object_start: 0x0,
        object_len: 0x3000,
        kind: MappingViewKind::Private,
        notifier: None,
    });

    let work = object
        .invalidate_range(0x1000, 0x1000)
        .expect("private anon object hit must be reported");
    assert_eq!(work.hits().len(), 1);
    let hit = &work.hits()[0];
    assert_eq!(hit.view().mm_id(), 9);
    assert_eq!(hit.vma_start(), 0xa000);
    assert_eq!(hit.object_start(), 0x1000);
    assert_eq!(hit.object_len(), 0x1000);
}

#[def_test]
fn anon_private_page_state_fork_and_discard_work() {
    use khal::{mem::PhysAddr, paging::PageSize};

    let parent = AnonPrivateObject::new_root();
    let child = parent.fork_child();
    let handle = parent
        .install_page(0x1000, PhysAddr::from_usize(0x2000), PageSize::Size4K)
        .expect("install private page");
    assert!(handle.is_exclusive());

    let prepared = parent
        .prepare_fork_child_pages(0x1000, 0x1000, &child)
        .expect("prepare fork child pages");
    assert_eq!(prepared.pages().len(), 1);
    assert_eq!(prepared.pages()[0].object_start(), 0x1000);
    assert!(!handle.is_exclusive());
    prepared.commit().expect("commit fork child pages");
    assert_eq!(
        child.page_at(0x1000).expect("child page").phys_addr(),
        PhysAddr::from_usize(0x2000)
    );

    let released = parent.detach_range(0x1000, 0x1000).finalize_release();
    assert!(released.is_empty());
    let released = child.detach_range(0x1000, 0x1000).finalize_release();
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].phys_addr(), PhysAddr::from_usize(0x2000));
}

#[def_test]
fn anon_private_first_touch_prepare_commits_page_slot() {
    use khal::{mem::PhysAddr, paging::PageSize};

    let object = AnonPrivateObject::new_root();
    let prepared = object
        .prepare_first_touch_page(0x1000)
        .expect("prepare first-touch page");

    assert!(
        object.page_at(0x1000).is_none(),
        "prepared first-touch page must not publish object state"
    );

    let handle = prepared
        .commit(PhysAddr::from_usize(0x5000), PageSize::Size4K)
        .expect("commit first-touch page");
    assert_eq!(handle.phys_addr(), PhysAddr::from_usize(0x5000));
    assert_eq!(
        object
            .page_at(0x1000)
            .expect("committed page slot")
            .phys_addr(),
        PhysAddr::from_usize(0x5000)
    );
}

#[def_test]
fn anon_private_first_touch_commit_revalidates_empty_slot() {
    use kerrno::KError;
    use khal::{mem::PhysAddr, paging::PageSize};

    let object = AnonPrivateObject::new_root();
    let prepared = object
        .prepare_first_touch_page(0x1000)
        .expect("prepare first-touch page");
    object
        .install_page(0x1000, PhysAddr::from_usize(0x6000), PageSize::Size4K)
        .expect("install competing page");

    let err = match prepared.commit(PhysAddr::from_usize(0x7000), PageSize::Size4K) {
        Ok(_) => panic!("commit must reject a slot that became populated"),
        Err(err) => err,
    };
    assert_eq!(err, KError::AlreadyExists);
    assert_eq!(
        object
            .page_at(0x1000)
            .expect("original page must remain installed")
            .phys_addr(),
        PhysAddr::from_usize(0x6000)
    );
}

#[def_test]
fn anon_private_replace_if_same_rejects_changed_slot() {
    use khal::{mem::PhysAddr, paging::PageSize};

    let object = AnonPrivateObject::new_root();
    let expected = object
        .install_page(0x1000, PhysAddr::from_usize(0x6000), PageSize::Size4K)
        .expect("install expected page");
    object
        .replace_page(0x1000, PhysAddr::from_usize(0x7000), PageSize::Size4K)
        .expect("replace competing page");

    let result = object.replace_page_if_same_after(
        0x1000,
        &expected,
        PhysAddr::from_usize(0x8000),
        PageSize::Size4K,
        || Ok::<_, ()>(()),
    );
    assert!(matches!(result, Err(AnonPrivatePageCommitError::Changed)));
    assert_eq!(
        object
            .page_at(0x1000)
            .expect("competing page must remain installed")
            .phys_addr(),
        PhysAddr::from_usize(0x7000)
    );
}

#[def_test]
fn anon_private_detach_rolls_back_when_not_finalized() {
    use khal::{mem::PhysAddr, paging::PageSize};

    let object = AnonPrivateObject::new_root();
    object
        .install_page(0x1000, PhysAddr::from_usize(0x3000), PageSize::Size4K)
        .expect("install private page");

    let detached = object.detach_range(0x1000, 0x1000);
    assert!(object.page_at(0x1000).is_none());
    drop(detached);

    let page = object
        .page_at(0x1000)
        .expect("detached page must roll back");
    assert_eq!(page.phys_addr(), PhysAddr::from_usize(0x3000));
}

#[def_test]
fn anon_private_fork_prepare_rolls_back_without_commit() {
    use khal::{mem::PhysAddr, paging::PageSize};

    let parent = AnonPrivateObject::new_root();
    let child = parent.fork_child();
    let handle = parent
        .install_page(0x1000, PhysAddr::from_usize(0x4000), PageSize::Size4K)
        .expect("install private page");

    {
        let prepared = parent
            .prepare_fork_child_pages(0x1000, 0x1000, &child)
            .expect("prepare fork pages");
        assert_eq!(prepared.pages().len(), 1);
        assert!(!handle.is_exclusive());
    }

    assert!(handle.is_exclusive());
    assert!(child.page_at(0x1000).is_none());
}

#[def_test]
fn anon_private_first_cow_commit_failure_keeps_slot_absent() {
    use khal::{mem::PhysAddr, paging::PageSize};
    let object = AnonPrivateObject::new_root();
    let prepared = object
        .prepare_first_touch_page(0)
        .expect("prepare absent slot");
    let result = prepared.commit_after(PhysAddr::from_usize(0x7000), PageSize::Size4K, || {
        Err::<(), _>(17)
    });
    assert!(matches!(
        result,
        Err(AnonPrivatePageCommitError::Commit(17))
    ));
    assert!(object.page_at(0).is_none());
    let prepared = object.prepare_first_touch_page(0).expect("prepare again");
    object
        .install_page(0, PhysAddr::from_usize(0x8000), PageSize::Size4K)
        .expect("competing page");
    let mut did_run = false;
    let result = prepared.commit_after(PhysAddr::from_usize(0x9000), PageSize::Size4K, || {
        did_run = true;
        Ok::<(), ()>(())
    });
    assert!(matches!(result, Err(AnonPrivatePageCommitError::Changed)));
    assert!(!did_run);
    assert_eq!(
        object
            .page_at(0)
            .expect("competing slot retained")
            .phys_addr(),
        PhysAddr::from_usize(0x8000)
    );
    object.detach_range(0, 4096).finalize_release();
}
