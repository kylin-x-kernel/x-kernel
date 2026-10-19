// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::KError;
use khal::paging::{MappingFlags, PageSize};
use memaddr::{PAGE_SIZE_4K, VirtAddr};
use unittest::def_test;
use vmobj::{AnonObjectId, FileObjectId, VmObjectId};

use super::{
    FileMappingInfo, VmArea, VmAreaSet, VmBackingInfo, VmBackingKind, VmInheritance, VmRuntimeRef,
};

fn vaddr(addr: usize) -> VirtAddr {
    VirtAddr::from_usize(addr)
}

fn test_area(start: usize, size: usize, page_offset: u64) -> VmArea {
    area_with_flags(
        start,
        size,
        page_offset,
        MappingFlags::READ | MappingFlags::WRITE,
        MappingFlags::READ | MappingFlags::WRITE,
    )
}

fn area_with_flags(
    start: usize,
    size: usize,
    page_offset: u64,
    flags: MappingFlags,
    max_flags: MappingFlags,
) -> VmArea {
    VmArea::new(
        vaddr(start),
        size,
        flags,
        max_flags,
        VmBackingInfo::new(VmBackingKind::Linear, PageSize::Size4K),
        page_offset,
        None,
    )
}

fn file_area(start: usize, size: usize, page_offset: u64, file_offset: u64) -> VmArea {
    VmArea::new(
        vaddr(start),
        size,
        MappingFlags::READ | MappingFlags::WRITE,
        MappingFlags::READ | MappingFlags::WRITE,
        VmBackingInfo::new(
            VmBackingKind::FileShared {
                object: VmObjectId::File(FileObjectId::from_raw(7)),
            },
            PageSize::Size4K,
        ),
        page_offset,
        Some(FileMappingInfo {
            offset: file_offset,
            inode: 11,
            path: None,
        }),
    )
}

fn ranges(set: &VmAreaSet) -> alloc::vec::Vec<(usize, usize, u64)> {
    set.iter()
        .map(|area| {
            (
                area.start().as_usize(),
                area.end().as_usize(),
                area.page_offset(),
            )
        })
        .collect()
}

fn file_offsets(set: &VmAreaSet) -> alloc::vec::Vec<u64> {
    set.iter()
        .map(|area| area.file_mapping().expect("file metadata").offset)
        .collect()
}

fn merge_result(left: VmArea, right: VmArea) -> alloc::vec::Vec<(usize, usize, u64)> {
    let mut set = VmAreaSet::new();
    let start = left.start();
    set.insert(left);
    set.insert(right);
    set.merge_adjacent(start).expect("target VMA must exist");
    ranges(&set)
}

#[def_test]
fn vma_set_rejects_overlapping_insert() {
    let mut set = VmAreaSet::new();
    set.try_insert(test_area(0x1000, PAGE_SIZE_4K * 2, 4))
        .expect("first insert must succeed");

    assert_eq!(
        set.try_insert(test_area(0x2000, PAGE_SIZE_4K, 5)),
        Err(KError::AlreadyExists)
    );
    set.try_insert(test_area(0x3000, PAGE_SIZE_4K, 6))
        .expect("adjacent insert must succeed");
}

#[def_test]
fn vma_set_middle_unmap_splits_and_preserves_offsets() {
    let mut set = VmAreaSet::new();
    set.insert(test_area(0x1000, PAGE_SIZE_4K * 4, 10));

    set.unmap(vaddr(0x2000), PAGE_SIZE_4K);

    assert_eq!(
        ranges(&set),
        alloc::vec![(0x1000, 0x2000, 10), (0x3000, 0x5000, 12)]
    );
}

#[def_test]
fn vma_set_front_and_back_unmap_preserve_remaining_metadata() {
    let mut set = VmAreaSet::new();
    set.insert(test_area(0x1000, PAGE_SIZE_4K * 4, 20));

    set.unmap(vaddr(0x1000), PAGE_SIZE_4K);
    assert_eq!(ranges(&set), alloc::vec![(0x2000, 0x5000, 21)]);

    set.unmap(vaddr(0x4000), PAGE_SIZE_4K);
    assert_eq!(ranges(&set), alloc::vec![(0x2000, 0x4000, 21)]);
}

#[def_test]
fn vma_set_unmap_preserves_file_metadata_offsets() {
    let mut set = VmAreaSet::new();
    set.insert(file_area(0x1000, PAGE_SIZE_4K * 4, 50, 0x8000));

    set.unmap(vaddr(0x2000), PAGE_SIZE_4K);

    assert_eq!(
        ranges(&set),
        alloc::vec![(0x1000, 0x2000, 50), (0x3000, 0x5000, 52)]
    );
    assert_eq!(file_offsets(&set), alloc::vec![0x8000, 0xa000]);
}

#[def_test]
fn vma_set_exact_unmap_removes_only_target_area() {
    let mut set = VmAreaSet::new();
    set.insert(test_area(0x1000, PAGE_SIZE_4K, 1));
    set.insert(test_area(0x3000, PAGE_SIZE_4K, 3));

    set.unmap(vaddr(0x1000), PAGE_SIZE_4K);

    assert!(set.find(vaddr(0x1000)).is_none());
    assert!(set.find(vaddr(0x3000)).is_some());
    assert_eq!(ranges(&set), alloc::vec![(0x3000, 0x4000, 3)]);
}

#[def_test]
fn vma_set_protect_middle_preserves_non_permission_metadata() {
    let mut set = VmAreaSet::new();
    set.insert(test_area(0x1000, PAGE_SIZE_4K * 3, 30).with_inheritance(VmInheritance::DontCopy));

    set.protect(vaddr(0x2000), PAGE_SIZE_4K, MappingFlags::READ);

    let areas = set.iter().collect::<alloc::vec::Vec<_>>();
    assert_eq!(areas.len(), 3);
    assert_eq!(areas[0].inheritance(), VmInheritance::DontCopy);
    assert_eq!(areas[1].inheritance(), VmInheritance::DontCopy);
    assert_eq!(areas[2].inheritance(), VmInheritance::DontCopy);
    assert_eq!(areas[1].flags(), MappingFlags::READ);
    assert_eq!(
        areas[1].max_flags(),
        MappingFlags::READ | MappingFlags::WRITE
    );
    assert_eq!(areas[1].page_offset(), 31);
}

#[def_test]
fn vma_set_protect_preserves_file_metadata_offsets() {
    let mut set = VmAreaSet::new();
    set.insert(file_area(0x1000, PAGE_SIZE_4K * 3, 70, 0x10000));

    set.protect(vaddr(0x2000), PAGE_SIZE_4K, MappingFlags::READ);

    let areas = set.iter().collect::<alloc::vec::Vec<_>>();
    assert_eq!(areas.len(), 3);
    assert_eq!(file_offsets(&set), alloc::vec![0x10000, 0x11000, 0x12000]);
    assert_eq!(areas[1].file_offset_for(vaddr(0x2000)), Some(0x11000));
    assert_eq!(areas[1].page_offset(), 71);
}

#[def_test]
fn vma_set_merge_adjacent_uses_caller_predicate() {
    let mut set = VmAreaSet::new();
    set.insert(test_area(0x1000, PAGE_SIZE_4K, 40));
    set.insert(test_area(0x2000, PAGE_SIZE_4K, 41));
    set.insert(test_area(0x3000, PAGE_SIZE_4K, 99));

    let merged_start = set
        .merge_adjacent_where(vaddr(0x2000), |left, right| {
            left.flags() == right.flags()
                && left.max_flags() == right.max_flags()
                && left.backing() == right.backing()
                && left.inheritance() == right.inheritance()
                && left.page_offset() + (left.size() / PAGE_SIZE_4K) as u64 == right.page_offset()
        })
        .expect("target VMA must exist");

    assert_eq!(merged_start, vaddr(0x1000));
    assert_eq!(
        ranges(&set),
        alloc::vec![(0x1000, 0x3000, 40), (0x3000, 0x4000, 99)]
    );
}

#[def_test]
fn vma_set_semantic_merge_accepts_matching_adjacent_vmas() {
    let left = test_area(0x1000, PAGE_SIZE_4K, 40).with_runtime(Some(VmRuntimeRef::new_linear(0)));
    let right = test_area(0x2000, PAGE_SIZE_4K, 41).with_runtime(Some(VmRuntimeRef::new_linear(0)));

    assert_eq!(merge_result(left, right), alloc::vec![(0x1000, 0x3000, 40)]);
}

#[def_test]
fn vma_set_semantic_merge_rejects_permission_mismatch() {
    let left = test_area(0x1000, PAGE_SIZE_4K, 40);
    let right = area_with_flags(
        0x2000,
        PAGE_SIZE_4K,
        41,
        MappingFlags::READ,
        MappingFlags::READ | MappingFlags::WRITE,
    );

    assert_eq!(
        merge_result(left, right),
        alloc::vec![(0x1000, 0x2000, 40), (0x2000, 0x3000, 41)]
    );
}

#[def_test]
fn vma_set_semantic_merge_rejects_max_permission_mismatch() {
    let left = test_area(0x1000, PAGE_SIZE_4K, 40);
    let right = area_with_flags(
        0x2000,
        PAGE_SIZE_4K,
        41,
        MappingFlags::READ | MappingFlags::WRITE,
        MappingFlags::READ,
    );

    assert_eq!(
        merge_result(left, right),
        alloc::vec![(0x1000, 0x2000, 40), (0x2000, 0x3000, 41)]
    );
}

#[def_test]
fn vma_set_semantic_merge_rejects_backing_mismatch() {
    let left = test_area(0x1000, PAGE_SIZE_4K, 40);
    let right = test_area(0x2000, PAGE_SIZE_4K, 41).with_backing(VmBackingInfo::new(
        VmBackingKind::AnonymousPrivate {
            object: VmObjectId::Anon(AnonObjectId::from_raw(9)),
        },
        PageSize::Size4K,
    ));

    assert_eq!(
        merge_result(left, right),
        alloc::vec![(0x1000, 0x2000, 40), (0x2000, 0x3000, 41)]
    );
}

#[def_test]
fn vma_set_semantic_merge_rejects_object_offset_gap() {
    let left = test_area(0x1000, PAGE_SIZE_4K, 40);
    let right = test_area(0x2000, PAGE_SIZE_4K, 99);

    assert_eq!(
        merge_result(left, right),
        alloc::vec![(0x1000, 0x2000, 40), (0x2000, 0x3000, 99)]
    );
}

#[def_test]
fn vma_set_semantic_merge_rejects_file_metadata_gap() {
    let left = file_area(0x1000, PAGE_SIZE_4K, 40, 0x8000);
    let right = file_area(0x2000, PAGE_SIZE_4K, 41, 0xc000);

    assert_eq!(
        merge_result(left, right),
        alloc::vec![(0x1000, 0x2000, 40), (0x2000, 0x3000, 41)]
    );
}

#[def_test]
fn vma_set_semantic_merge_rejects_inheritance_mismatch() {
    let left = test_area(0x1000, PAGE_SIZE_4K, 40);
    let right = test_area(0x2000, PAGE_SIZE_4K, 41).with_inheritance(VmInheritance::DontCopy);

    assert_eq!(
        merge_result(left, right),
        alloc::vec![(0x1000, 0x2000, 40), (0x2000, 0x3000, 41)]
    );
}

#[def_test]
fn vma_set_semantic_merge_rejects_runtime_kind_mismatch() {
    let left = test_area(0x1000, PAGE_SIZE_4K, 40).with_runtime(Some(VmRuntimeRef::new_linear(0)));
    let right = test_area(0x2000, PAGE_SIZE_4K, 41).with_runtime(Some(
        VmRuntimeRef::new_anon_private(vaddr(0x2000), PageSize::Size4K),
    ));

    assert_eq!(
        merge_result(left, right),
        alloc::vec![(0x1000, 0x2000, 40), (0x2000, 0x3000, 41)]
    );
}

#[def_test]
fn vma_set_semantic_merge_rejects_unknown_runtime_kind() {
    let left = test_area(0x1000, PAGE_SIZE_4K, 40);
    let right = test_area(0x2000, PAGE_SIZE_4K, 41);

    assert_eq!(
        merge_result(left, right),
        alloc::vec![(0x1000, 0x2000, 40), (0x2000, 0x3000, 41)]
    );
}
