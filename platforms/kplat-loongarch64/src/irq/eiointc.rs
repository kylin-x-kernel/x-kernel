// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LoongArch EIOINTC (extended I/O interrupt controller) driver.
//!
//! The EIOINTC owns [`VEC_COUNT`] = 256 interrupt vectors accessed via IOCSR.
//! Every public mutator rejects an out-of-range `hwirq` *before* any IOCSR
//! access, so an abnormal source number can neither compute an out-of-window
//! IOCSR offset nor corrupt an unrelated vector's enable/bounce/ISR state.
//! Rejection is reported via `warn!` and never silently truncated, taken
//! modulo the vector count, or remapped onto another legal vector.
//!
//! Affinity support is limited to the fixed node/core route mode selected by
//! the QEMU platform. The mode is a platform contract, not hardware detection;
//! CPU-encoded/virtual EIOINTC and multi-node routing are not implemented.

use core::sync::atomic::{AtomicBool, Ordering};

use kspin::{SpinNoIrq, static_lock};
use loongArch64::iocsr::{iocsr_read_d, iocsr_read_w, iocsr_write_d, iocsr_write_w};

const LOONGARCH_IOCSR_MISC_FUNC: usize = 0x420;
const IOCSR_MISC_FUNC_EXT_IOI_EN: u64 = 1 << 48;
const EIOINTC_REG_NODEMAP: usize = 0x14a0;
const EIOINTC_REG_IPMAP: usize = 0x14c0;
const EIOINTC_REG_ENABLE: usize = 0x1600;
const EIOINTC_REG_BOUNCE: usize = 0x1680;
const EIOINTC_REG_ISR: usize = 0x1800;
const EIOINTC_REG_ROUTE: usize = 0x1c00;
const VEC_REG_COUNT: usize = 4;
const VEC_COUNT_PER_REG: usize = 64;
const VEC_COUNT: usize = VEC_REG_COUNT * VEC_COUNT_PER_REG;
pub(super) const CORES_PER_EIO_NODE: usize = 4;
const VECTORS_PER_ROUTE: usize = 4;
const ROUTE_BYTE_MASK: u32 = 0xff;

/// The only route mode this backend implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EiointcRouteMode {
    /// Fixed node/core bitmap, single EIOINTC node, four cores per node.
    FixedNodeCoreSingleNode,
    /// A mode this backend cannot safely address, including virtual CPU encode.
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RetargetError {
    /// No register write occurred.
    Rejected,
    /// A mask/route/mask-restore write may have occurred and confirmation failed.
    HardwareFailure,
}

const UNSUPPORTED_ROUTE_MODE: EiointcRouteMode = EiointcRouteMode::Unsupported;

static AFFINITY_READY: AtomicBool = AtomicBool::new(false);

static_lock! {
    // Serialize this platform's shared router configuration, not per-CPU ISR
    // observation/W1C completion. The guard also pins local IOCSR transactions.
    static REGISTER_LOCK: SpinNoIrq<()> = SpinNoIrq::new(());
}

/// Register access surface for the EIOINTC.
///
/// The production implementation ([`IocsrEiointc`]) talks to IOCSR; tests
/// substitute a recording backend to assert that legal `hwirq` values touch
/// the expected offsets and that illegal values touch no register at all.
trait EiointcRegs {
    /// Read a 64-bit IOCSR register at byte offset `addr`.
    fn read_d(&mut self, addr: usize) -> u64;
    /// Write a 64-bit IOCSR register at byte offset `addr`.
    fn write_d(&mut self, addr: usize, val: u64);
    /// Read a 32-bit IOCSR register at byte offset `addr`.
    fn read_w(&mut self, addr: usize) -> u32;
    /// Write a 32-bit IOCSR register at byte offset `addr`.
    fn write_w(&mut self, addr: usize, val: u32);
}

/// IOCSR-backed register access; the production backend.
struct IocsrEiointc;

impl EiointcRegs for IocsrEiointc {
    fn read_d(&mut self, addr: usize) -> u64 {
        iocsr_read_d(addr)
    }

    fn write_d(&mut self, addr: usize, val: u64) {
        iocsr_write_d(addr, val);
    }

    fn read_w(&mut self, addr: usize) -> u32 {
        iocsr_read_w(addr)
    }

    fn write_w(&mut self, addr: usize, val: u32) {
        iocsr_write_w(addr, val);
    }
}

fn verify_affinity_surface<R: EiointcRegs>(regs: &mut R) -> bool {
    for i in 0..VEC_REG_COUNT {
        if regs.read_d(EIOINTC_REG_ENABLE + i * 8) != 0 {
            return false;
        }
    }
    // init_impl routes every vector to node0/core0. Reading it back confirms
    // register content is accessible and matches the programmed value; it does
    // not identify the node. Node selection remains the platform contract.
    let expected_route = 0x0101_0101u32;
    (0..VEC_COUNT / 4).all(|i| regs.read_w(EIOINTC_REG_ROUTE + i * 4) == expected_route)
}

pub(super) fn init(mode: EiointcRouteMode) {
    init_impl(&mut IocsrEiointc);
    let supported = route_mode_supported(mode) && {
        let mut regs = IocsrEiointc;
        verify_affinity_surface(&mut regs)
    };
    // Publish the verified affinity-surface result to supports_affinity().
    // That observer Acquire-loads this flag before trusting the initialization
    // data; the Release/Acquire pair orders the verification and publication.
    AFFINITY_READY.store(supported, Ordering::Release);
}

/// Checks only the present-CPU raw-id topology used by the stable gate.
///
/// It does not inspect firmware, the controller variant or the current CPU.
/// Missing/empty maps and any raw ID outside node0 fail closed.
pub(super) fn topology_supported(raw_cpus: &[Option<usize>]) -> bool {
    !raw_cpus.is_empty()
        && raw_cpus
            .iter()
            .all(|raw| raw.is_some_and(raw_cpu_supported))
}

pub(super) const fn execution_cpu_supported(raw_cpu: Option<usize>) -> bool {
    matches!(raw_cpu, Some(id) if raw_cpu_supported(id))
}

/// Returns whether stable platform facts permit this backend to expose affinity.
///
/// This deliberately does not inspect the current CPU or target; those are
/// per-call checks in [`current_cpu_can_route`] and the retarget transaction.
/// Unsupported modes, unconfirmed initialization, missing CPU maps and
/// multi-node topologies close the capability without turning them into a boot
/// failure. This is not firmware or controller-variant detection.
///
/// The Acquire load synchronizes with [`init`]'s Release store, so the verified
/// affinity-surface result is visible before the capability is trusted.
pub(super) fn supports_affinity() -> bool {
    if !AFFINITY_READY.load(Ordering::Acquire) {
        return false;
    }
    let mut raw_cpus = [None; kbuild_config::NR_CPUS];
    let mut present = 0usize;
    kcpu_id_map::for_each_present_logical_cpu(|_, logical, _| {
        if present < raw_cpus.len() {
            raw_cpus[present] = kcpu_id_map::raw_cpu_id(logical).map(|raw| raw.as_usize());
            present += 1;
        } else {
            // More present CPUs than the compile-time array: fail closed.
            present = raw_cpus.len().saturating_add(1);
        }
    });
    if present > raw_cpus.len() {
        return false;
    }
    topology_supported(&raw_cpus[..present])
}

/// Checks the CPU executing the affinity transaction.
///
/// Local IOCSR route access only reaches this fixed node. A CPU outside that
/// node must be rejected before any write; there is no remote-node send here.
///
/// Reads the current CPU id and may run in task or IRQ context. It does not
/// sleep, allocate or take `REGISTER_LOCK`; callers must already pin the
/// current CPU (or otherwise exclude migration), must not hold
/// `REGISTER_LOCK`, and must call it only from a CPU that can access this
/// controller node. A missing mapping or unsupported raw id returns `false`.
pub(super) fn current_cpu_can_route() -> bool {
    execution_cpu_supported(
        kcpu_id_map::raw_cpu_id(khal::percpu::this_cpu_id()).map(|raw| raw.as_usize()),
    )
}

fn init_impl<R: EiointcRegs>(regs: &mut R) {
    let _guard = REGISTER_LOCK.lock();
    // Establish source masks before changing routes or enabling the router;
    // firmware's enable bits are not our activation contract.
    for i in 0..VEC_REG_COUNT {
        regs.write_d(EIOINTC_REG_ENABLE + i * 8, 0);
    }
    let misc = regs.read_d(LOONGARCH_IOCSR_MISC_FUNC);
    regs.write_d(LOONGARCH_IOCSR_MISC_FUNC, misc | IOCSR_MISC_FUNC_EXT_IOI_EN);
    let index = 0;
    for i in 0..(VEC_COUNT / 32) {
        let data = ((1 << (i * 2 + 1)) << 16) | (1 << (i * 2));
        regs.write_w(EIOINTC_REG_NODEMAP + i * 4, data);
    }
    for i in 0..(VEC_COUNT / 32 / 4) {
        let bit = 1 << (1 + index);
        let data = bit | (bit << 8) | (bit << 16) | (bit << 24);
        regs.write_w(EIOINTC_REG_IPMAP + i * 4, data);
    }
    for i in 0..(VEC_COUNT / 4) {
        let bit = 1;
        let data = bit | (bit << 8) | (bit << 16) | (bit << 24);
        regs.write_w(EIOINTC_REG_ROUTE + i * 4, data);
    }
    for i in 0..(VEC_COUNT / 32) {
        regs.write_w(EIOINTC_REG_BOUNCE + i * 4, u32::MAX);
    }
}

fn split_bit(irq: usize) -> (usize, u64) {
    (irq / VEC_COUNT_PER_REG * 8, 1 << (irq % VEC_COUNT_PER_REG))
}

/// Reject an out-of-range `hwirq` before any IOCSR access.
///
/// Returns `true` when `irq` is a valid EIOINTC vector index. Shared by the
/// enable, disable and complete paths so the failure path provably performs
/// no register read or write.
fn check_hwirq(irq: usize) -> bool {
    irq < VEC_COUNT
}

fn enable_irq_impl<R: EiointcRegs>(regs: &mut R, irq: usize) {
    if !check_hwirq(irq) {
        warn!("eiointc: reject out-of-range hwirq {irq} (valid 0..{VEC_COUNT})");
        return;
    }
    let _guard = REGISTER_LOCK.lock();
    let (offset, bit) = split_bit(irq);
    for base in [EIOINTC_REG_ENABLE, EIOINTC_REG_BOUNCE] {
        let addr = base + offset;
        let val = regs.read_d(addr);
        regs.write_d(addr, val | bit);
    }
}

fn disable_irq_impl<R: EiointcRegs>(regs: &mut R, irq: usize) {
    if !check_hwirq(irq) {
        warn!("eiointc: reject out-of-range hwirq {irq} (valid 0..{VEC_COUNT})");
        return;
    }
    let _guard = REGISTER_LOCK.lock();
    let (offset, bit) = split_bit(irq);
    let addr = EIOINTC_REG_ENABLE + offset;
    let val = regs.read_d(addr);
    regs.write_d(addr, val & !bit);
}

fn complete_irq_impl<R: EiointcRegs>(regs: &mut R, irq: usize) {
    if !check_hwirq(irq) {
        warn!("eiointc: reject out-of-range hwirq {irq} (valid 0..{VEC_COUNT})");
        return;
    }
    // ISR is W1C: acknowledge only this bit, without a control-register RMW
    // or lock shared with enable/disable. Dispatch keeps its CPU context pinned.
    let (offset, bit) = split_bit(irq);
    regs.write_d(EIOINTC_REG_ISR + offset, bit);
}

pub(super) const fn route_mode_supported(mode: EiointcRouteMode) -> bool {
    !matches!(mode, UNSUPPORTED_ROUTE_MODE)
}

pub(super) const fn raw_cpu_supported(raw_cpu: usize) -> bool {
    raw_cpu < CORES_PER_EIO_NODE
}

fn route_byte(raw_cpu: usize) -> Option<u32> {
    if !raw_cpu_supported(raw_cpu) {
        return None;
    }
    Some(1u32 << raw_cpu)
}

fn read_enable<R: EiointcRegs>(regs: &mut R, irq: usize) -> u64 {
    let (offset, bit) = split_bit(irq);
    regs.read_d(EIOINTC_REG_ENABLE + offset) & bit
}

fn write_enable<R: EiointcRegs>(regs: &mut R, irq: usize, enable: bool) -> bool {
    let (offset, bit) = split_bit(irq);
    let current = regs.read_d(EIOINTC_REG_ENABLE + offset);
    let value = if enable {
        current | bit
    } else {
        current & !bit
    };
    regs.write_d(EIOINTC_REG_ENABLE + offset, value);
    (read_enable(regs, irq) != 0) == enable
}

/// Requests a final mask after a restore confirmation failure. Returns whether
/// the mask was *confirmed*; false never means the hardware is safe.
fn cleanup_mask<R: EiointcRegs>(regs: &mut R, irq: usize) -> bool {
    if write_enable(regs, irq, false) {
        return true;
    }
    let (offset, bit) = split_bit(irq);
    let current = regs.read_d(EIOINTC_REG_ENABLE + offset);
    regs.write_d(EIOINTC_REG_ENABLE + offset, current & !bit);
    let confirmed = read_enable(regs, irq) == 0;
    if !confirmed {
        warn!("eiointc: affinity mask cleanup unconfirmed for vector {irq}");
    }
    confirmed
}

fn retarget_irq_impl<R: EiointcRegs>(
    regs: &mut R,
    irq: usize,
    raw_cpu: usize,
    keep_masked: bool,
) -> Result<(), RetargetError> {
    if !check_hwirq(irq) {
        return Err(RetargetError::Rejected);
    }
    let route_byte = route_byte(raw_cpu).ok_or(RetargetError::Rejected)?;
    let _guard = REGISTER_LOCK.lock();
    let was_enabled = read_enable(regs, irq) != 0;

    if !write_enable(regs, irq, false) {
        // Re-request suppression before reporting failure. A failed cleanup is
        // still only a request, never proof that the vector is safe.
        let _ = cleanup_mask(regs, irq);
        return Err(RetargetError::HardwareFailure);
    }

    let route_addr = EIOINTC_REG_ROUTE + (irq / VECTORS_PER_ROUTE) * 4;
    let shift = (irq % VECTORS_PER_ROUTE) * 8;
    let mask = ROUTE_BYTE_MASK << shift;
    let old = regs.read_w(route_addr);
    let updated = (old & !mask) | (route_byte << shift);
    regs.write_w(route_addr, updated);
    if regs.read_w(route_addr) != updated {
        let _ = cleanup_mask(regs, irq);
        return Err(RetargetError::HardwareFailure);
    }

    if keep_masked || !was_enabled {
        if write_enable(regs, irq, false) {
            return Ok(());
        }
        let _ = cleanup_mask(regs, irq);
        return Err(RetargetError::HardwareFailure);
    }

    if write_enable(regs, irq, true) {
        Ok(())
    } else {
        // The enable write may have become effective before readback failed.
        // Re-request suppression and report failure; confirmation is absent.
        let _ = cleanup_mask(regs, irq);
        Err(RetargetError::HardwareFailure)
    }
}

/// Retargets one EIOINTC vector on the current CPU.
///
/// Must run on a CPU whose local IOCSR reaches this fixed EIOINTC node; the
/// caller must check [`current_cpu_can_route`] before calling. It revalidates
/// the vector and raw target, may run in task or IRQ context, and does not
/// sleep or allocate. It takes `REGISTER_LOCK` internally, so callers must not
/// already hold that lock; ordinary IRQ/migration exclusion and the KIRQ Line
/// lock remain caller obligations.
pub(super) fn retarget_irq(
    irq: usize,
    raw_cpu: usize,
    keep_masked: bool,
) -> Result<(), RetargetError> {
    retarget_irq_impl(&mut IocsrEiointc, irq, raw_cpu, keep_masked)
}

pub(super) fn enable_irq(irq: usize) {
    enable_irq_impl(&mut IocsrEiointc, irq);
}

pub(super) fn disable_irq(irq: usize) {
    disable_irq_impl(&mut IocsrEiointc, irq);
}

pub(super) fn complete_irq(irq: usize) {
    complete_irq_impl(&mut IocsrEiointc, irq);
}

pub(super) fn claim_irq() -> Option<usize> {
    for i in 0..(VEC_COUNT / 64) {
        let flags = iocsr_read_d(EIOINTC_REG_ISR + i * 8);
        if flags != 0 {
            return Some(flags.trailing_zeros() as usize + 64 * i);
        }
    }
    None
}

unittest::path_mod_test!(tests, "../../ut-tests/irq_eiointc.rs");
