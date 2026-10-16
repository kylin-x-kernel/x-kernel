// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use memaddr::PAGE_SIZE_4K;
use unittest::def_test;

use super::{
    FileObjectId, MappingView, MappingViewId, MappingViewKind, MappingViewRange,
    ObjectInvalidateRequest, ObjectInvalidateWork, ObjectViewHit, VmObjectId,
};

fn sample_view() -> MappingView {
    MappingView::new(
        MappingViewId::from_raw(1),
        7,
        MappingViewRange {
            vma_start: 0x4000,
            vma_len: 0x3000,
            object_start: 0x8000,
            object_len: 0x3000,
        },
        MappingViewKind::Shared,
    )
}

#[def_test]
fn mapping_view_rejects_empty_or_overflowing_ranges() {
    assert!(
        !MappingViewRange {
            vma_start: 0x4000,
            vma_len: 0,
            object_start: 0x8000,
            object_len: 0x1000,
        }
        .is_valid()
    );
    assert!(
        !MappingViewRange {
            vma_start: 0x4000,
            vma_len: 0x1000,
            object_start: 0x8000,
            object_len: 0,
        }
        .is_valid()
    );
    assert!(
        !MappingViewRange {
            vma_start: 0x4000,
            vma_len: 0x1000,
            object_start: u64::MAX,
            object_len: 2,
        }
        .is_valid()
    );
    assert!(
        MappingView::try_new(
            MappingViewId::from_raw(1),
            7,
            MappingViewRange {
                vma_start: 0x4000,
                vma_len: 0,
                object_start: 0x8000,
                object_len: 0x1000,
            },
            MappingViewKind::Shared,
        )
        .is_none()
    );
    assert!(
        !MappingViewRange {
            vma_start: u64::MAX,
            vma_len: 2,
            object_start: 0x8000,
            object_len: 0x1000,
        }
        .is_valid()
    );
}

#[def_test]
fn object_hit_must_stay_inside_registered_view() {
    let view = sample_view();
    let hit = ObjectViewHit::try_new(view.clone(), 0x9000, 0x1000)
        .expect("hit inside view should be accepted");

    assert_eq!(hit.vma_start(), 0x5000);
    assert_eq!(hit.vma_len(), 0x1000);
    assert!(ObjectViewHit::try_new(view.clone(), 0x7000, 0x1000).is_none());
    assert!(ObjectViewHit::try_new(view, 0xa000, 0x2000).is_none());
}

#[def_test]
fn page_hit_clips_object_event_to_view_overlap() {
    let view = sample_view();
    let hit = view
        .page_hit(0x7000, 0x3000)
        .expect("partially overlapping event should hit view");

    assert_eq!(hit.object_start(), 0x8000);
    assert_eq!(hit.object_len(), 0x2000);
    assert_eq!(hit.vma_start(), 0x4000);
    assert!(view.page_hit(0xb000, 0x1000).is_none());
}

#[def_test]
fn invalidate_work_range_must_cover_hits() {
    let view = sample_view();
    let hit = ObjectViewHit::try_new(view, 0x9000, 0x1000).expect("valid hit");
    let work = ObjectInvalidateWork::new(
        VmObjectId::File(FileObjectId::from_raw(11)),
        0x8000,
        0x3000,
        vec![hit.clone()],
    );

    assert_eq!(work.object(), VmObjectId::File(FileObjectId::from_raw(11)));
    assert_eq!(work.object_start(), 0x8000);
    assert_eq!(work.object_end(), 0xb000);
    assert_eq!(work.hits(), core::slice::from_ref(&hit));
    assert!(
        ObjectInvalidateWork::try_new(
            VmObjectId::File(FileObjectId::from_raw(11)),
            0x8000,
            0x1000,
            vec![hit],
        )
        .is_none()
    );
}

#[def_test]
fn invalidate_work_only_derives_requests_for_carried_hits() {
    let view = sample_view();
    let carried = ObjectViewHit::try_new(view.clone(), 0x9000, 0x1000).expect("valid hit");
    let not_carried =
        ObjectViewHit::try_new(view.clone(), 0x8000, 0x1000).expect("valid uncarried hit");
    let work = ObjectInvalidateWork::new(
        VmObjectId::File(FileObjectId::from_raw(11)),
        0x8000,
        0x3000,
        vec![carried.clone()],
    );

    assert!(work.request_for_hit(&carried).is_some());
    assert!(work.request_for_hit(&not_carried).is_none());

    let suffix = carried
        .aligned_object_suffix(PAGE_SIZE_4K as u64)
        .expect("aligned carried hit should produce itself");
    assert!(work.request_for_subhit(&carried, &suffix).is_some());
    assert!(work.request_for_subhit(&not_carried, &suffix).is_none());
}

#[def_test]
fn object_hit_can_produce_aligned_suffix() {
    let view = sample_view();
    let hit = ObjectViewHit::try_new(view, 0x8001, 0x1fff).expect("valid unaligned hit");
    let suffix = hit
        .aligned_object_suffix(PAGE_SIZE_4K as u64)
        .expect("hit should have a 4K-aligned suffix");

    assert_eq!(suffix.object_start(), 0x9000);
    assert_eq!(suffix.object_len(), 0x1000);
    assert_eq!(suffix.vma_start(), 0x5000);
}

#[def_test]
fn invalidate_request_uses_shared_view_hit_language() {
    let view = sample_view();
    let request = ObjectInvalidateRequest::from_page_hit(
        VmObjectId::File(FileObjectId::from_raw(11)),
        &view,
        0x9000,
        0x1000,
    )
    .expect("request should be produced for a covered hit");

    assert_eq!(
        request.object(),
        VmObjectId::File(FileObjectId::from_raw(11))
    );
    assert_eq!(request.hit().vma_start(), 0x5000);
}
