// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

/// A recorded IOCSR access against the recording backend.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Access {
    /// Sentinel for an unused log slot.
    None,
    Read {
        addr: usize,
    },
    WriteD {
        addr: usize,
        val: u64,
    },
    ReadW {
        addr: usize,
    },
    WriteW {
        addr: usize,
        val: u32,
    },
}

/// In-memory recording backend with ENABLE/BOUNCE state. Other reads return
/// zero. It never touches real IOCSR.
struct RecorderEiointc<const N: usize> {
    accesses: [Access; N],
    count: usize,
    enable: [u64; VEC_REG_COUNT],
    bounce: [u64; VEC_REG_COUNT],
    route: [u32; VEC_COUNT / 4],
    has_unprotected_control_access: bool,
    has_masked_completion: bool,
    fail_enable_write: bool,
    fail_route_write: bool,
    fault_irq: usize,
    fail_restore_enable: bool,
    fail_restore_readback: bool,
    fail_cleanup_write: bool,
    restore_failed: bool,
    restore_pending: bool,
    cleanup_attempted: bool,
}

impl<const N: usize> RecorderEiointc<N> {
    const fn new() -> Self {
        Self {
            accesses: [Access::None; N],
            count: 0,
            enable: [0; VEC_REG_COUNT],
            bounce: [0; VEC_REG_COUNT],
            route: [0; VEC_COUNT / 4],
            has_unprotected_control_access: false,
            has_masked_completion: false,
            fail_enable_write: false,
            fail_route_write: false,
            fault_irq: 1,
            fail_restore_enable: false,
            fail_restore_readback: false,
            fail_cleanup_write: false,
            restore_failed: false,
            restore_pending: false,
            cleanup_attempted: false,
        }
    }

    fn record(&mut self, access: Access) {
        let is_completion = matches!(access, Access::WriteD { addr, .. }
            if (EIOINTC_REG_ISR..EIOINTC_REG_ISR + VEC_REG_COUNT * 8).contains(&addr));
        if is_completion {
            self.has_masked_completion |= !karch::local_irq_enabled();
        } else {
            self.has_unprotected_control_access |= karch::local_irq_enabled();
            #[cfg(feature = "smp")]
            {
                self.has_unprotected_control_access |= REGISTER_LOCK.try_lock().is_some();
            }
        }
        if self.count < self.accesses.len() {
            self.accesses[self.count] = access;
        }
        self.count += 1;
    }

    fn access_count(&self) -> usize {
        self.count
    }

    fn access(&self, i: usize) -> Access {
        self.accesses[i]
    }
}

impl<const N: usize> EiointcRegs for RecorderEiointc<N> {
    fn read_d(&mut self, addr: usize) -> u64 {
        self.record(Access::Read { addr });
        if (EIOINTC_REG_ENABLE..EIOINTC_REG_ENABLE + VEC_REG_COUNT * 8).contains(&addr) {
            let bank = (addr - EIOINTC_REG_ENABLE) / 8;
            let mut value = self.enable[bank];
            if self.fault_irq < VEC_COUNT && self.restore_pending && !self.cleanup_attempted {
                value &= !(1u64 << (self.fault_irq % VEC_COUNT_PER_REG));
            }
            return value;
        }
        if (EIOINTC_REG_BOUNCE..EIOINTC_REG_BOUNCE + VEC_REG_COUNT * 8).contains(&addr) {
            return self.bounce[(addr - EIOINTC_REG_BOUNCE) / 8];
        }
        0
    }

    fn write_d(&mut self, addr: usize, val: u64) {
        self.record(Access::WriteD { addr, val });
        if (EIOINTC_REG_ENABLE..EIOINTC_REG_ENABLE + VEC_REG_COUNT * 8).contains(&addr) {
            let bank = (addr - EIOINTC_REG_ENABLE) / 8;
            if self.fault_irq < VEC_COUNT {
                let bit = 1u64 << (self.fault_irq % VEC_COUNT_PER_REG);
                let was_set = self.enable[bank] & bit != 0;
                let next_set = val & bit != 0;
                if self.fail_restore_enable && next_set && !was_set {
                    self.restore_failed = true;
                    return;
                }
                if self.fail_restore_readback && next_set && !was_set {
                    self.enable[bank] = val;
                    self.restore_pending = true;
                    return;
                }
                if self.restore_pending && !next_set {
                    self.cleanup_attempted = true;
                    if self.fail_cleanup_write {
                        return;
                    }
                }
            }
            if self.fail_enable_write {
                return;
            }
            self.enable[bank] = val;
        } else if (EIOINTC_REG_BOUNCE..EIOINTC_REG_BOUNCE + VEC_REG_COUNT * 8).contains(&addr) {
            self.bounce[(addr - EIOINTC_REG_BOUNCE) / 8] = val;
        }
    }

    fn read_w(&mut self, addr: usize) -> u32 {
        self.record(Access::ReadW { addr });
        if !(EIOINTC_REG_ROUTE..EIOINTC_REG_ROUTE + VEC_COUNT).contains(&addr) {
            panic!("unexpected read_w address {addr:#x}");
        }
        self.route[(addr - EIOINTC_REG_ROUTE) / 4]
    }

    fn write_w(&mut self, addr: usize, val: u32) {
        if (EIOINTC_REG_BOUNCE..EIOINTC_REG_BOUNCE + VEC_REG_COUNT * 8).contains(&addr) {
            let offset = addr - EIOINTC_REG_BOUNCE;
            let shift = (offset % 8) * 8;
            let word = &mut self.bounce[offset / 8];
            *word = (*word & !(u64::from(u32::MAX) << shift)) | (u64::from(val) << shift);
        } else if (EIOINTC_REG_ROUTE..EIOINTC_REG_ROUTE + VEC_COUNT).contains(&addr) {
            if !self.fail_route_write {
                self.route[(addr - EIOINTC_REG_ROUTE) / 4] = val;
            }
        } else {
            if !((EIOINTC_REG_NODEMAP..EIOINTC_REG_NODEMAP + VEC_COUNT / 8).contains(&addr)
                || (EIOINTC_REG_IPMAP..EIOINTC_REG_IPMAP + VEC_COUNT / 8).contains(&addr))
            {
                panic!("unexpected write_w address {addr:#x}");
            }
        }
        self.record(Access::WriteW { addr, val });
    }
}

#[def_test]
fn preparation_masks_all_vectors_before_routing() {
    // Preparation records 88 accesses; other operations need at most four.
    // Only this fixture reserves the larger log, with overflow checked below.
    let mut rec = RecorderEiointc::<128>::new();
    init_impl(&mut rec);
    assert!(rec.access_count() <= rec.accesses.len());
    for i in 0..VEC_REG_COUNT {
        assert_eq!(rec.access(i), Access::WriteD {
            addr: EIOINTC_REG_ENABLE + i * 8, val: 0,
        });
    }
    // No later routing write may reopen an enable bank.
    for access in &rec.accesses[VEC_REG_COUNT..rec.access_count()] {
        let addr = match access {
            Access::WriteD { addr, .. } | Access::WriteW { addr, .. } => *addr,
            Access::ReadW { addr } => *addr,
            _ => continue,
        };
        assert!(!(EIOINTC_REG_ENABLE..EIOINTC_REG_ENABLE + VEC_REG_COUNT * 8).contains(&addr));
    }
}

#[def_test]
fn test_enable_legal_boundaries() {
    for irq in [0usize, 63, 64, 255] {
        let mut rec = RecorderEiointc::<8>::new();
        enable_irq_impl(&mut rec, irq);

        // ENABLE and BOUNCE, each as a read-modify-write.
        assert_eq!(rec.access_count(), 4);

        let offset = (irq / VEC_COUNT_PER_REG) * 8;
        let bit = 1u64 << (irq % VEC_COUNT_PER_REG);
        assert_eq!(
            rec.access(0),
            Access::Read {
                addr: EIOINTC_REG_ENABLE + offset
            }
        );
        assert_eq!(
            rec.access(1),
            Access::WriteD {
                addr: EIOINTC_REG_ENABLE + offset,
                val: bit
            }
        );
        assert_eq!(
            rec.access(2),
            Access::Read {
                addr: EIOINTC_REG_BOUNCE + offset
            }
        );
        assert_eq!(
            rec.access(3),
            Access::WriteD {
                addr: EIOINTC_REG_BOUNCE + offset,
                val: bit
            }
        );
    }
}

#[def_test]
fn test_enable_rejects_out_of_range() {
    for irq in [256usize, 3584, usize::MAX] {
        let mut rec = RecorderEiointc::<8>::new();
        enable_irq_impl(&mut rec, irq);
        // No IOCSR access before rejection: the log stays empty.
        assert_eq!(rec.access_count(), 0);
    }
}

#[def_test]
fn test_disable_legal() {
    for irq in [0usize, 255] {
        let mut rec = RecorderEiointc::<8>::new();
        disable_irq_impl(&mut rec, irq);

        // disable touches ENABLE only, as a read-modify-write.
        assert_eq!(rec.access_count(), 2);

        let offset = (irq / VEC_COUNT_PER_REG) * 8;
        assert_eq!(
            rec.access(0),
            Access::Read {
                addr: EIOINTC_REG_ENABLE + offset
            }
        );
        assert_eq!(
            rec.access(1),
            Access::WriteD {
                addr: EIOINTC_REG_ENABLE + offset,
                val: 0
            }
        );
    }
}

#[def_test]
fn test_disable_rejects_out_of_range() {
    for irq in [256usize, usize::MAX] {
        let mut rec = RecorderEiointc::<8>::new();
        disable_irq_impl(&mut rec, irq);
        assert_eq!(rec.access_count(), 0);
    }
}

#[def_test]
fn test_complete_legal() {
    for irq in [0usize, 255] {
        let mut rec = RecorderEiointc::<8>::new();
        complete_irq_impl(&mut rec, irq);

        // complete is a single ISR write.
        assert_eq!(rec.access_count(), 1);

        let offset = (irq / VEC_COUNT_PER_REG) * 8;
        let bit = 1u64 << (irq % VEC_COUNT_PER_REG);
        assert_eq!(
            rec.access(0),
            Access::WriteD {
                addr: EIOINTC_REG_ISR + offset,
                val: bit
            }
        );
    }
}

#[def_test]
fn test_complete_rejects_out_of_range() {
    for irq in [256usize, usize::MAX] {
        let mut rec = RecorderEiointc::<8>::new();
        complete_irq_impl(&mut rec, irq);
        assert_eq!(rec.access_count(), 0);
    }
}

#[def_test]
fn register_transactions_exclude_irq_and_preserve_other_vectors() {
    assert!(karch::local_irq_enabled());
    let mut preparation = RecorderEiointc::<128>::new();
    init_impl(&mut preparation);
    assert!(!preparation.has_unprotected_control_access);
    assert!(karch::local_irq_enabled());
    for bank in 0..VEC_REG_COUNT {
        let mut rec = RecorderEiointc::<16>::new();
        let first = bank * VEC_COUNT_PER_REG;
        let last = first + VEC_COUNT_PER_REG - 1;
        enable_irq_impl(&mut rec, first);
        enable_irq_impl(&mut rec, last);
        assert_eq!(rec.enable[bank], 1 | (1 << 63));
        assert_eq!(rec.bounce[bank], 1 | (1 << 63));
        disable_irq_impl(&mut rec, first);
        assert_eq!(rec.enable[bank], 1 << 63);
        assert_eq!(rec.bounce[bank], 1 | (1 << 63));
        disable_irq_impl(&mut rec, last);
        assert_eq!(rec.enable[bank], 0);
        assert!(!rec.has_unprotected_control_access);
        assert!(karch::local_irq_enabled());
    }
}

#[def_test]
fn completion_preserves_unmasked_context_and_writes_one_w1c() {
    // Only the recorder runs here, not live IOCSR. Starting unmasked makes
    // an accidental SpinNoIrq acquisition observable even in UP builds.
    assert!(karch::local_irq_enabled());
    let mut rec = RecorderEiointc::<8>::new();
    complete_irq_impl(&mut rec, 63);
    assert!(!rec.has_masked_completion);
    assert!(karch::local_irq_enabled());
    assert_eq!(rec.access_count(), 1);
    assert_eq!(rec.access(0), Access::WriteD { addr: EIOINTC_REG_ISR, val: 1 << 63 });
}

#[def_test]
fn retarget_preserves_neighbors_and_restores_enable() {
    let mut rec = RecorderEiointc::<32>::new();
    rec.route[0] = 0x4433_2211;
    rec.enable[0] = 1 << 1;
    rec.bounce[0] = 0xa5a5;
    assert_eq!(retarget_irq_impl(&mut rec, 1, 2, false), Ok(()));
    assert_eq!(rec.route[0], 0x4433_0411);
    assert_eq!(rec.enable[0], 1 << 1);
    assert_eq!(rec.bounce[0], 0xa5a5);
}

#[def_test]
fn retarget_keep_masked_leaves_target_masked() {
    let mut rec = RecorderEiointc::<32>::new();
    rec.enable[0] = 1 << 1;
    assert_eq!(retarget_irq_impl(&mut rec, 1, 3, true), Ok(()));
    assert_eq!(rec.enable[0], 0);
    assert_eq!(rec.route[0], 8 << 8);
}

#[def_test]
fn retarget_rejects_unencodable_cpu_without_access() {
    for raw in [4usize, 31, 32, usize::MAX] {
        let mut rec = RecorderEiointc::<8>::new();
        assert_eq!(
            retarget_irq_impl(&mut rec, 1, raw, false),
            Err(RetargetError::Rejected)
        );
        assert_eq!(rec.access_count(), 0);
    }
}

#[def_test]
fn retarget_mask_or_route_confirmation_failure_keeps_failure_closed() {
    let mut rec = RecorderEiointc::<32>::new();
    rec.enable[0] = 1 << 1;
    rec.fail_enable_write = true;
    assert_eq!(retarget_irq_impl(&mut rec, 1, 2, false), Err(RetargetError::HardwareFailure));
    assert!(rec.enable[0] != 0);

    let mut rec = RecorderEiointc::<32>::new();
    rec.enable[0] = 1 << 1;
    rec.fail_route_write = true;
    assert_eq!(retarget_irq_impl(&mut rec, 1, 2, false), Err(RetargetError::HardwareFailure));
    assert_eq!(rec.enable[0], 0);
}

#[def_test]
fn unsupported_route_mode_is_distinct_from_fixed_node_core() {
    assert_ne!(
        EiointcRouteMode::Unsupported,
        EiointcRouteMode::FixedNodeCoreSingleNode
    );
}

#[def_test]
fn retarget_route_boundaries_and_all_core_encodings() {
    for irq in [0usize, 3, 4, 63] {
        for raw in 0..CORES_PER_EIO_NODE {
            let mut rec = RecorderEiointc::<32>::new();
            assert_eq!(retarget_irq_impl(&mut rec, irq, raw, true), Ok(()));
            let route_addr = EIOINTC_REG_ROUTE + (irq / VECTORS_PER_ROUTE) * 4;
            let shift = (irq % VECTORS_PER_ROUTE) * 8;
            assert_eq!(
                (rec.route[(route_addr - EIOINTC_REG_ROUTE) / 4] >> shift) & 0xff,
                1 << raw
            );
        }
    }
}

#[def_test]
fn retarget_initially_disabled_preserve_stays_disabled() {
    let mut rec = RecorderEiointc::<32>::new();
    assert_eq!(retarget_irq_impl(&mut rec, 1, 2, false), Ok(()));
    assert_eq!(rec.enable[0], 0);
    assert_eq!(rec.route[0], 4 << 8);
}

#[def_test]
fn restore_confirmation_failure_remasks_and_reports_failure() {
    let mut rec = RecorderEiointc::<32>::new();
    rec.enable[0] = 1 << 1;
    rec.fail_restore_enable = true;
    assert_eq!(retarget_irq_impl(&mut rec, 1, 2, false), Err(RetargetError::HardwareFailure));
    assert_eq!(rec.enable[0], 0);
    assert!(rec.restore_failed);
}

#[def_test]
fn restore_applied_but_readback_failed_then_cleanup_unconfirmed() {
    let mut rec = RecorderEiointc::<32>::new();
    rec.enable[0] = 1 << 1;
    rec.fail_restore_readback = true;
    rec.fail_cleanup_write = true;
    assert_eq!(retarget_irq_impl(&mut rec, 1, 2, false), Err(RetargetError::HardwareFailure));
    assert!(rec.enable[0] & (1 << 1) != 0);
    assert!(rec.restore_pending);
    assert!(rec.cleanup_attempted);
}

#[def_test]
fn restore_applied_but_readback_failed_then_cleanup_succeeds() {
    let mut rec = RecorderEiointc::<32>::new();
    rec.enable[0] = 1 << 1;
    rec.fail_restore_readback = true;
    assert_eq!(retarget_irq_impl(&mut rec, 1, 2, false), Err(RetargetError::HardwareFailure));
    assert_eq!(rec.enable[0], 0);
    assert!(rec.restore_pending);
    assert!(rec.cleanup_attempted);
}

#[def_test]
fn failure_disable_keep_masked_retry_then_explicit_enable_keeps_route() {
    let mut rec = RecorderEiointc::<32>::new();
    rec.enable[0] = 1 << 1;
    rec.fail_restore_enable = true;
    assert_eq!(retarget_irq_impl(&mut rec, 1, 2, false), Err(RetargetError::HardwareFailure));

    // Ordinary production disable, then a masked retry.
    disable_irq_impl(&mut rec, 1);
    assert_eq!(rec.enable[0], 0);
    rec.fail_restore_enable = false;
    assert_eq!(retarget_irq_impl(&mut rec, 1, 3, true), Ok(()));
    assert_eq!(rec.enable[0], 0);
    assert_eq!(rec.route[0], 8 << 8);
    assert!(!rec.fail_restore_enable);

    // Explicit production enable uses the retained route, not CPU0.
    enable_irq_impl(&mut rec, 1);
    assert_eq!(rec.enable[0], 1 << 1);
    assert_eq!(rec.bounce[0], 1 << 1);
    assert_eq!(rec.route[0], 8 << 8);
}

#[def_test]
fn capability_helpers_reject_unsupported_mode_and_cpu_encodings() {
    assert!(route_mode_supported(EiointcRouteMode::FixedNodeCoreSingleNode));
    assert!(!route_mode_supported(EiointcRouteMode::Unsupported));
    for raw in 0..4 {
        assert!(raw_cpu_supported(raw));
    }
    for raw in [4usize, 31, 32, usize::MAX] {
        assert!(!raw_cpu_supported(raw));
        assert_eq!(route_byte(raw), None);
    }
}

#[def_test]
fn initialization_confirmation_checks_enable_and_route_surface() {
    let mut rec = RecorderEiointc::<256>::new();
    init_impl(&mut rec);
    assert!(verify_affinity_surface(&mut rec));
    rec.route[0] ^= 0x0100_0000;
    assert!(!verify_affinity_surface(&mut rec));
}

#[def_test]
fn topology_and_execution_gate_reject_missing_unsupported_and_foreign_cpus() {
    assert!(!topology_supported(&[]));
    assert!(topology_supported(&[Some(0), Some(3)]));
    assert!(!topology_supported(&[Some(0), Some(4)]));
    assert!(!topology_supported(&[Some(0), None]));
    assert!(!execution_cpu_supported(None));
    assert!(execution_cpu_supported(Some(0)));
    assert!(!execution_cpu_supported(Some(4)));
}

#[def_test]
fn initialization_confirmation_rejects_nonzero_enable() {
    let mut rec = RecorderEiointc::<128>::new();
    init_impl(&mut rec);
    rec.enable[0] = 1;
    assert!(!verify_affinity_surface(&mut rec));
}
