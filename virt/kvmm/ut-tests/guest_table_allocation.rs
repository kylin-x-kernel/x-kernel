// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Private allocation callbacks exercise the production table builders.

use kalloc::{PageRef, UsageKind};
use unittest::def_test;

use crate::mm::GuestMem;

#[cfg(target_arch = "aarch64")]
type GuestTables = super::Stage2;
#[cfg(target_arch = "x86_64")]
type GuestTables = super::Ept;
#[cfg(target_arch = "riscv64")]
type GuestTables = super::GStage;

const PAGE_BYTES: usize = 4096;
const GUEST_BASE: u64 = 0x4000_0000;
const HOST_BASE: u64 = 0x8000_0000;
const RAM_BYTES: u64 = 4 * 1024 * 1024;
const CONSTRUCTOR_CALLS: usize = if cfg!(target_arch = "x86_64") { 6 } else { 5 };
const TABLE_PAGES: usize = if cfg!(target_arch = "riscv64") {
    8
} else if cfg!(target_arch = "x86_64") {
    6
} else {
    5
};

fn usage(kind: UsageKind) -> usize {
    kalloc::global_allocator().usages().get(kind)
}

fn check_request(call: usize, pages: usize, alignment_bytes: usize) {
    let expected_pages = if cfg!(target_arch = "riscv64") && call == 1 {
        4
    } else {
        1
    };
    assert_eq!(
        (pages, alignment_bytes),
        (expected_pages, expected_pages * PAGE_BYTES)
    );
}

fn allocate_request(pages: usize, alignment_bytes: usize) -> PageRef {
    let page = kalloc::alloc_zeroed_pages_exact_ref(pages, alignment_bytes, UsageKind::Global)
        .expect("table fixture allocation");
    assert_eq!(page.page_count(), pages);
    assert_eq!(page.phys_addr().as_usize() % alignment_bytes, 0);
    page
}

#[def_test(serial)]
fn every_constructor_allocation_failure_releases_its_linked_prefix() {
    let before = usage(UsageKind::Global);
    for _ in 0..2 {
        for fail_call in 1..=CONSTRUCTOR_CALLS {
            let mut calls = 0;
            let tables = GuestTables::new_with_allocator(
                GUEST_BASE,
                RAM_BYTES,
                HOST_BASE,
                1,
                &mut |pages, alignment_bytes| {
                    calls += 1;
                    check_request(calls, pages, alignment_bytes);
                    if calls == fail_call {
                        None
                    } else {
                        Some(allocate_request(pages, alignment_bytes))
                    }
                },
            );
            assert!(tables.is_none(), "failure ordinal {fail_call}");
            assert_eq!(calls, fail_call);
            assert_eq!(
                usage(UsageKind::Global),
                before,
                "failure ordinal {fail_call}"
            );
        }
    }
}

#[def_test(serial)]
fn completed_constructor_retains_exact_table_geometry_until_drop() {
    let before = usage(UsageKind::Global);
    for _ in 0..2 {
        let mut calls = 0;
        let fail_call = CONSTRUCTOR_CALLS + 1;
        let tables = GuestTables::new_with_allocator(
            GUEST_BASE,
            RAM_BYTES,
            HOST_BASE,
            1,
            &mut |pages, alignment_bytes| {
                calls += 1;
                check_request(calls, pages, alignment_bytes);
                if calls == fail_call {
                    None
                } else {
                    Some(allocate_request(pages, alignment_bytes))
                }
            },
        )
        .expect("constructor success control");
        assert_eq!(calls, CONSTRUCTOR_CALLS);
        assert_eq!(usage(UsageKind::Global), before + TABLE_PAGES * PAGE_BYTES);
        assert_eq!(tables.gpa_to_hpa(GUEST_BASE), Some(HOST_BASE));
        assert_eq!(
            tables.gpa_to_hpa(GUEST_BASE + RAM_BYTES - 1),
            Some(HOST_BASE + RAM_BYTES - 1)
        );
        assert_eq!(tables.gpa_to_hpa(GUEST_BASE + RAM_BYTES), None);
        drop(tables);
        assert_eq!(usage(UsageKind::Global), before);
    }
}

struct FailingTables(GuestTables);

impl GuestMem for FailingTables {
    fn new(mem_base: u64, mem_size: u64, hpa_base: u64, vmid: u32) -> Option<Self> {
        let retained_ram = usage(UsageKind::VirtMem);
        let mut calls = 0;
        let tables = GuestTables::new_with_allocator(
            mem_base,
            mem_size,
            hpa_base,
            vmid,
            &mut |pages, alignment_bytes| {
                calls += 1;
                check_request(calls, pages, alignment_bytes);
                if calls == CONSTRUCTOR_CALLS {
                    None
                } else {
                    Some(allocate_request(pages, alignment_bytes))
                }
            },
        );
        assert_eq!(calls, CONSTRUCTOR_CALLS);
        // Vm::build must still retain its reservation while the mapper's
        // failed constructor destroys every child and the root.
        assert_eq!(usage(UsageKind::VirtMem), retained_ram);
        tables.map(Self)
    }

    fn map_region(&self, _: u64, _: u64, _: u64, _: crate::mm::GuestPerm) -> bool {
        false
    }

    fn gpa_to_hpa(&self, gpa: u64) -> Option<u64> {
        self.0.gpa_to_hpa(gpa)
    }

    fn activate(&self) {
        panic!("construction failure fixture must never activate");
    }
}

struct FailingConstructionArch;

impl crate::arch::VmmArch for FailingConstructionArch {
    type ArchVcpu = ();
    type GuestMem = FailingTables;

    fn init_vcpu(_: &mut crate::Vcpu<Self>, _: u64, _: u64) -> bool {
        false
    }

    fn restore_guest_ctx(_: &mut crate::Vcpu<Self>) {}

    fn enter_guest(_: &mut crate::Vcpu<Self>) -> bool {
        false
    }

    fn exit_handler(_: &mut crate::Vcpu<Self>) -> crate::ExitAction {
        crate::ExitAction::Exit
    }

    fn save_guest_ctx(_: &mut crate::Vcpu<Self>) {}

    fn guest_test_code() -> (*const u8, usize) {
        (core::ptr::null(), 0)
    }
}

#[def_test(serial)]
fn reserved_vm_build_releases_ram_after_late_table_allocation_failure() {
    let tables_before = usage(UsageKind::Global);
    let ram_before = usage(UsageKind::VirtMem);
    let cfg = crate::VmConfig::new(GUEST_BASE, 3 * PAGE_BYTES as u64, 1);
    assert!(crate::Vm::<FailingConstructionArch>::new(cfg).is_none());
    assert_eq!(usage(UsageKind::Global), tables_before);
    assert_eq!(usage(UsageKind::VirtMem), ram_before);
}

unittest::path_mod_test!(
    #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
    mod split,
    "guest_table_split.rs"
);
