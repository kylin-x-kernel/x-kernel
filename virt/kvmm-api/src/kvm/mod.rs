// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal Linux KVM-compatible control device (`/dev/kvm`).
//!
//! This module exposes the fd shape expected by kvmtool/lkvm: a system fd creates
//! VM fds, VM fds create vCPU fds, and vCPU fds own `KVM_RUN` plus the shared
//! `struct kvm_run` mmap page. It implements only the ioctl subset needed for
//! the lkvm bring-up path.
//!
//! Everything that depends on the guest's register ABI, exit encoding or device
//! model lives in the per-architecture [`arch`] module selected below; the code
//! here is architecture-independent. The `struct kvm_run` layout, the generic
//! ioctl numbers and the FDT patcher are shared because Linux defines them
//! identically for every architecture.

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
use aarch64 as arch;
#[cfg(target_arch = "riscv64")]
mod riscv64;
use alloc::{string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anon_inodefs::AnonInodeFs;
use kvfs::{
    DeviceFileOps, FMode, FileOperations, MmapMapper, NodeFlags, OpenFlags, VfsError, VfsFile,
    VfsFileBuilder, VfsInode, VfsResult,
};
use kvmm::{
    Vcpu, Vm, VmConfig,
    arch::{CurrentArch, VmmArch},
    mm::{GuestMem, GuestPerm},
    vcpu_state::VcpuRunState,
};
use memaddr::{MemoryAddr, PhysAddrRange, VirtAddr};
use posix_types::{UserConstPtr, UserPtr, UserRead};
#[cfg(target_arch = "riscv64")]
use riscv64 as arch;

const KVM_API_VERSION: usize = 12;
const KVM_GET_API_VERSION: u32 = 0xae00;
const KVM_CREATE_VM: u32 = 0xae01;
const KVM_CHECK_EXTENSION: u32 = 0xae03;
const KVM_GET_VCPU_MMAP_SIZE: u32 = 0xae04;
const KVM_CREATE_VCPU: u32 = 0xae41;
const KVM_SET_USER_MEMORY_REGION: u32 = 0x4020_ae46;
const KVM_RUN: u32 = 0xae80;
const KVM_IOEVENTFD: u32 = 0x4040_ae79;
const KVM_EXIT_MMIO: u32 = 6;
const KVM_EXIT_SYSTEM_EVENT: u32 = 24;
const KVM_SYSTEM_EVENT_SHUTDOWN: u32 = 1;
const KVM_GET_ONE_REG: u32 = 0x4010_aeab;
const KVM_SET_ONE_REG: u32 = 0x4010_aeac;
const KVM_CREATE_DEVICE: u32 = 0xc00c_aee0;
const KVM_SET_DEVICE_ATTR: u32 = 0x4020_aee1;
const KVM_GET_DEVICE_ATTR: u32 = 0x4020_aee2;
const KVM_HAS_DEVICE_ATTR: u32 = 0x4020_aee3;

const KVM_CAP_USER_MEMORY: usize = 3;
const KVM_CAP_NR_VCPUS: usize = 9;
const KVM_CAP_NR_MEMSLOTS: usize = 10;
const KVM_CAP_IOEVENTFD: usize = 36;
const KVM_CAP_MAX_VCPUS: usize = 66;
const KVM_CAP_ONE_REG: usize = 70;

/// Guest physical RAM base both architectures' lkvm builds load at (kvmtool's
/// `ARM64_RAM_BASE` and `RISCV_RAM` are the same value).
const KVMTOOL_RAM_BASE: u64 = 0x8000_0000;
const KVMTOOL_RAM_SIZE: u64 = 0x8000_0000;
const DEFAULT_VCPUS: usize = 1;
const KVM_RUN_MMAP_SIZE: usize = 4096;
const KVM_RUN_MAX_INTERNAL_EXITS: usize = 256;
const KVM_RUN_EXIT_REASON_OFF: usize = 8;
const KVM_RUN_MMIO_OFF: usize = 32;
const KVM_RUN_MMIO_PHYS_ADDR_OFF: usize = KVM_RUN_MMIO_OFF;
const KVM_RUN_MMIO_DATA_OFF: usize = KVM_RUN_MMIO_OFF + 8;
const KVM_RUN_MMIO_LEN_OFF: usize = KVM_RUN_MMIO_OFF + 16;
const KVM_RUN_MMIO_IS_WRITE_OFF: usize = KVM_RUN_MMIO_OFF + 20;
const KVM_RUN_SYSTEM_EVENT_OFF: usize = 32;
const KVM_RUN_SYSTEM_EVENT_TYPE_OFF: usize = KVM_RUN_SYSTEM_EVENT_OFF;
const KVM_RUN_SYSTEM_EVENT_NDATA_OFF: usize = KVM_RUN_SYSTEM_EVENT_OFF + 4;

/// `/dev/kvm` system device.
pub struct KvmDevice;

struct KvmVmFile {
    vm: ksync::Mutex<Vm<CurrentArch>>,
    next_vcpu_id: AtomicUsize,
    user_memory_regions: ksync::Mutex<Vec<KvmUserspaceMemoryRegion>>,
    /// Legacy in-kernel GICv2 state, aarch64 only: lkvm requests the irqchip
    /// with `KVM_CREATE_IRQCHIP` and then supplies both bank addresses, so the
    /// real wiring has to wait until the second one arrives.
    #[cfg(target_arch = "aarch64")]
    legacy_irqchip_requested: AtomicBool,
    #[cfg(target_arch = "aarch64")]
    legacy_gic_addrs: ksync::Mutex<LegacyGicAddrs>,
    #[cfg(target_arch = "aarch64")]
    gic_created: AtomicBool,
}

#[cfg(target_arch = "aarch64")]
#[derive(Default)]
struct LegacyGicAddrs {
    dist: Option<u64>,
    cpu: Option<u64>,
}

struct KvmVcpuFile {
    vm_file: Arc<KvmVmFile>,
    vcpu: ksync::Mutex<Option<Vcpu<CurrentArch>>>,
    run_page: kalloc::PageRef,
    pending_mmio_read: ksync::Mutex<Option<PendingMmioRead>>,
    run_calls: AtomicUsize,
    kernel_mmio_logs: AtomicUsize,
    userspace_mmio_logs: AtomicUsize,
    dtb_patched: AtomicBool,
    /// Level of the guest's supervisor external interrupt line, driven by
    /// `KVM_INTERRUPT` (riscv only) and applied on the owning pCPU just before
    /// entering the guest.
    #[cfg(target_arch = "riscv64")]
    external_irq_pending: AtomicBool,
}

#[derive(Clone, Copy)]
struct PendingMmioRead {
    reg: usize,
    len: usize,
}

/// A guest MMIO access that the kernel did not claim, to be emulated by lkvm.
///
/// Produced by the per-architecture exit decoder in either direction: a write
/// carries the value to store, a read leaves `value` at zero and fills it with
/// lkvm's answer on the next `KVM_RUN`.
struct MmioExit {
    gpa: u64,
    is_write: bool,
    size: u8,
    value: u64,
    /// Index into the architecture's guest GPR file. This may name the
    /// architecture's hardwired-zero register; use
    /// [`arch::mmio_reg_is_discarded`] rather than comparing against zero.
    reg: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KvmUserspaceMemoryRegion {
    slot: u32,
    flags: u32,
    guest_phys_addr: u64,
    memory_size: u64,
    userspace_addr: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KvmOneReg {
    id: u64,
    addr: u64,
}

// SAFETY: mirrors Linux `struct kvm_userspace_memory_region`; all fields are
// explicit integer ABI carriers and every byte is initialized.
unsafe impl UserRead for KvmUserspaceMemoryRegion {}
// SAFETY: mirrors Linux `struct kvm_one_reg`, two initialized `u64` fields.
unsafe impl UserRead for KvmOneReg {}

struct KvmVmFops;
struct KvmVcpuFops;

impl KvmDevice {
    /// Create a new KVM-compatible system device instance.
    pub fn new() -> Self {
        Self
    }

    fn create_vm_fd(&self, file: &VfsFile, vm_type: usize) -> VfsResult<usize> {
        if vm_type != 0 {
            return Err(VfsError::InvalidInput);
        }

        let cfg = VmConfig::new(KVMTOOL_RAM_BASE, KVMTOOL_RAM_SIZE, DEFAULT_VCPUS);
        let vm = Vm::<CurrentArch>::new_unreserved(cfg).ok_or(VfsError::NoMemory)?;
        let vm_file = AnonInodeFs::global().get_file(
            "[kvmm-kvm-vm]",
            Arc::new(KvmVmFops),
            Arc::new(KvmVmFile {
                vm: ksync::Mutex::new(vm),
                next_vcpu_id: AtomicUsize::new(0),
                user_memory_regions: ksync::Mutex::new(Vec::new()),
                #[cfg(target_arch = "aarch64")]
                legacy_irqchip_requested: AtomicBool::new(false),
                #[cfg(target_arch = "aarch64")]
                legacy_gic_addrs: ksync::Mutex::new(LegacyGicAddrs::default()),
                #[cfg(target_arch = "aarch64")]
                gic_created: AtomicBool::new(false),
            }),
            FMode::READ | FMode::WRITE | FMode::STREAM,
            file.flags() & OpenFlags::NONBLOCK,
            file.cred().clone(),
        )?;
        Ok(kprocess::current_files().add_file(vm_file, false)? as usize)
    }

    fn check_extension(cap: usize) -> usize {
        match cap {
            KVM_CAP_USER_MEMORY => 1,
            KVM_CAP_NR_VCPUS | KVM_CAP_MAX_VCPUS => DEFAULT_VCPUS,
            KVM_CAP_NR_MEMSLOTS => 32,
            KVM_CAP_IOEVENTFD => 1,
            KVM_CAP_ONE_REG => 1,
            // Everything else is architecture-specific, including
            // `KVM_CAP_COALESCED_MMIO`, which must stay 0: lkvm reads it as a
            // page offset and would place its coalesced ring one page past the
            // single page we map.
            _ => arch::check_extension(cap),
        }
    }
}

impl Default for KvmDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceFileOps for KvmDevice {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        file.stream_open();
        Ok(())
    }

    fn ioctl(&self, file: &VfsFile, _cred: &kcred::Cred, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            KVM_GET_API_VERSION => Ok(KVM_API_VERSION),
            KVM_CREATE_VM => self.create_vm_fd(file, arg),
            KVM_CHECK_EXTENSION => Ok(Self::check_extension(arg)),
            KVM_GET_VCPU_MMAP_SIZE => Ok(KVM_RUN_MMAP_SIZE),
            _ => Err(VfsError::NotATty),
        }
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

impl KvmVmFile {
    fn from_file(file: &VfsFile) -> VfsResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(VfsError::BadFileDescriptor)
    }

    fn create_vcpu_fd(self: &Arc<Self>, file: &VfsFile, vcpu_id: usize) -> VfsResult<usize> {
        let mut vm = self.vm.lock();
        if vcpu_id >= vm.shared().nr_vcpus() {
            return Err(VfsError::InvalidInput);
        }
        let expected = self.next_vcpu_id.load(Ordering::Acquire);
        if vcpu_id != expected {
            return Err(VfsError::InvalidInput);
        }
        self.next_vcpu_id
            .compare_exchange(expected, expected + 1, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| VfsError::ResourceBusy)?;

        // Must run before `register`/`create_vcpu`: the device wiring needs
        // exclusive access to the VM's guest memory.
        arch::prepare_vm(&mut vm, self)?;
        vm.register();
        let vcpu = vm.create_vcpu(vcpu_id as u32);
        drop(vm);

        let run_page = kalloc::alloc_zeroed_page_ref(kalloc::UsageKind::Global)
            .map_err(|_| VfsError::NoMemory)?;
        let vcpu_file = AnonInodeFs::global().get_file(
            "[kvmm-kvm-vcpu]",
            Arc::new(KvmVcpuFops),
            Arc::new(KvmVcpuFile {
                vm_file: Arc::clone(self),
                vcpu: ksync::Mutex::new(Some(vcpu)),
                run_page,
                pending_mmio_read: ksync::Mutex::new(None),
                run_calls: AtomicUsize::new(0),
                kernel_mmio_logs: AtomicUsize::new(0),
                userspace_mmio_logs: AtomicUsize::new(0),
                dtb_patched: AtomicBool::new(false),
                #[cfg(target_arch = "riscv64")]
                external_irq_pending: AtomicBool::new(false),
            }),
            FMode::READ | FMode::WRITE | FMode::STREAM,
            file.flags() & OpenFlags::NONBLOCK,
            file.cred().clone(),
        )?;
        Ok(kprocess::current_files().add_file(vcpu_file, false)? as usize)
    }

    fn set_user_memory_region(&self, arg: usize) -> VfsResult<usize> {
        let region = UserConstPtr::<KvmUserspaceMemoryRegion>::from(arg).read_vm()?;
        // Hold the slot table across the whole update: the record and the
        // stage-2 mappings must never disagree, including on failure paths.
        let mut regions = self.user_memory_regions.lock();
        let previous = regions
            .iter()
            .find(|entry| entry.slot == region.slot)
            .copied();

        if region.memory_size == 0 {
            // Deleting a slot must revoke its stage-2 mappings before the ioctl
            // returns: lkvm may release or reuse the backing pages immediately
            // afterwards, and a stale translation would hand them to the guest.
            if let Some(previous) = previous {
                self.unmap_user_memory_region(&previous)?;
                regions.retain(|entry| entry.slot != region.slot);
            }
            return Ok(0);
        }
        if region.flags != 0
            || region.userspace_addr & 0xfff != 0
            || region.guest_phys_addr & 0xfff != 0
            || region.memory_size & 0xfff != 0
        {
            return Err(VfsError::InvalidInput);
        }

        let (vm_base, vm_size) = {
            let vm = self.vm.lock();
            (vm.shared().cfg().mem_base(), vm.shared().cfg().mem_size())
        };
        let Some(vm_end) = vm_base.checked_add(vm_size) else {
            return Err(VfsError::InvalidInput);
        };
        if region.guest_phys_addr < vm_base
            || region
                .guest_phys_addr
                .checked_add(region.memory_size)
                .is_none_or(|end| end > vm_end)
        {
            return Err(VfsError::InvalidInput);
        }

        // Replacing a slot revokes the old range first. The mapping helpers
        // never leave partial state behind, so a failure here leaves the slot
        // unmapped rather than half old and half new.
        if let Some(previous) = previous {
            self.unmap_user_memory_region(&previous)?;
        }
        if let Err(err) = self.map_user_memory_region(region) {
            regions.retain(|entry| entry.slot != region.slot);
            return Err(err);
        }
        if let Some(existing) = regions.iter_mut().find(|entry| entry.slot == region.slot) {
            *existing = region;
        } else {
            regions.push(region);
        }
        Ok(0)
    }

    /// Revoke the stage-2 mappings of a slot that is being deleted or replaced.
    fn unmap_user_memory_region(&self, region: &KvmUserspaceMemoryRegion) -> VfsResult<()> {
        let vm = self.vm.lock();
        if !vm.unmap_guest_region(region.guest_phys_addr, region.memory_size) {
            log::error!(
                "[kvm] failed to revoke memslot {} gpa={:#x}+{:#x}",
                region.slot,
                region.guest_phys_addr,
                region.memory_size,
            );
            return Err(VfsError::NoMemory);
        }
        Ok(())
    }

    /// Map a memslot's user pages into the guest's second-stage page table.
    ///
    /// The host run is walked one page at a time so the chunk handed to
    /// [`Vm::map_guest_region`] never spans a page boundary, and each page is
    /// touched through userspace first to force it to be backed before its
    /// physical address is taken.
    ///
    /// On failure the pages mapped so far are revoked again, so the caller
    /// never has to clean up a partially installed slot.
    fn map_user_memory_region(&self, region: KvmUserspaceMemoryRegion) -> VfsResult<()> {
        let aspace_ref = kprocess::current_user_process().address_space()?;
        let vm = self.vm.lock();
        let mut mapped = 0u64;
        let result: VfsResult<()> = (|| {
            while mapped < region.memory_size {
                let user_va = VirtAddr::from((region.userspace_addr + mapped) as usize);
                let byte = UserConstPtr::<u8>::from(user_va.as_usize()).read_vm()?;
                UserPtr::<u8>::from(user_va.as_usize()).write_vm(byte)?;
                let (hpa, _flags, page_size) = {
                    let aspace = aspace_ref.lock();
                    aspace
                        .page_table()
                        .query(user_va)
                        .map_err(|_| VfsError::BadAddress)?
                };
                let page_size = usize::from(page_size) as u64;
                let page_offset = user_va.align_offset(page_size as usize) as u64;
                let chunk = (page_size - page_offset).min(region.memory_size - mapped);
                if !vm.map_guest_region(
                    region.guest_phys_addr + mapped,
                    hpa.as_usize() as u64 + page_offset,
                    chunk,
                    GuestPerm::RamRw,
                ) {
                    return Err(VfsError::NoMemory);
                }
                mapped += chunk;
            }
            Ok(())
        })();

        if result.is_err() && mapped != 0 {
            // Do not leave a partial slot mapped: the VMM was told the ioctl
            // failed and may free the backing pages.
            if !vm.unmap_guest_region(region.guest_phys_addr, mapped) {
                log::error!(
                    "[kvm] memslot {} partial-map rollback failed gpa={:#x}+{:#x}",
                    region.slot,
                    region.guest_phys_addr,
                    mapped,
                );
            }
        }
        result
    }

    /// Reverse-map a guest physical address to the userspace VA that backs it.
    ///
    /// Used to patch the guest's device tree through lkvm's own address space;
    /// that is how the boot arguments reach a VM whose RAM this kernel does not
    /// own.
    fn guest_userspace_addr(&self, gpa: u64) -> Option<usize> {
        self.user_memory_regions.lock().iter().find_map(|region| {
            let end = region.guest_phys_addr.checked_add(region.memory_size)?;
            if gpa < region.guest_phys_addr || gpa >= end {
                return None;
            }
            Some((region.userspace_addr + (gpa - region.guest_phys_addr)) as usize)
        })
    }
}

impl Drop for KvmVmFile {
    fn drop(&mut self) {
        self.vm.lock().shutdown();
    }
}

impl KvmVcpuFile {
    fn from_file(file: &VfsFile) -> VfsResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(VfsError::BadFileDescriptor)
    }

    fn run(&self) -> VfsResult<usize> {
        let mut guard = self.vcpu.lock();
        let Some(vcpu) = guard.as_mut() else {
            return Err(VfsError::ResourceBusy);
        };
        let run_call = self.run_calls.fetch_add(1, Ordering::Relaxed);
        if run_call == 0 {
            self.patch_guest_dtb_bootargs(vcpu);
            arch::log_run_start(vcpu);
        }
        // Deliver the value of a read that the previous exit bounced to lkvm.
        // The guest PC was already advanced when that exit was reported.
        self.complete_pending_mmio_read(vcpu)?;
        vcpu.vm.try_mark_cpu_on(vcpu.vcpu_id);
        for _ in 0..KVM_RUN_MAX_INTERNAL_EXITS {
            if self.run_once(vcpu)? {
                return Ok(0);
            }
        }
        // Every internal exit re-enters the guest, so a guest that traps
        // continuously must not monopolize its pCPU.
        ktask::yield_now();
        self.write_run_u32(KVM_RUN_EXIT_REASON_OFF, 0);
        Ok(0)
    }

    /// Enter the guest once and classify the resulting exit.
    ///
    /// Returns `true` when the caller must return to lkvm, `false` when the exit
    /// was handled in-kernel and the guest can be re-entered.
    fn run_once(&self, vcpu: &mut Vcpu<CurrentArch>) -> VfsResult<bool> {
        let irq_guard = ksync::spin::IrqSave::new();
        let pcpu = khal::percpu::this_cpu_id().as_usize() as i32;
        vcpu.vm.set_vcpu_pcpu(vcpu.vcpu_id, pcpu);
        if let Some(gm) = vcpu.vm.guest_mem() {
            gm.activate();
        }
        CurrentArch::restore_guest_ctx(vcpu);
        arch::apply_irq_state(self, vcpu);
        let hooks = core::mem::take(&mut vcpu.hooks);
        for mut hook in hooks {
            hook.on_entry(vcpu);
            vcpu.hooks.push(hook);
        }
        vcpu.vm
            .set_vcpu_run_state(vcpu.vcpu_id, VcpuRunState::RunningGuest);
        if !CurrentArch::enter_guest(vcpu) {
            return Err(VfsError::Io);
        }
        vcpu.vm.set_vcpu_pcpu(vcpu.vcpu_id, -1);
        vcpu.vm
            .set_vcpu_run_state(vcpu.vcpu_id, VcpuRunState::HostHandlingExit);
        CurrentArch::save_guest_ctx(vcpu);
        for hook in &mut vcpu.hooks {
            hook.on_exit(vcpu.vcpu_id);
        }
        drop(irq_guard);
        arch::complete_run_exit(self, vcpu)
    }

    fn complete_pending_mmio_read(&self, vcpu: &mut Vcpu<CurrentArch>) -> VfsResult<()> {
        let Some(pending) = self.pending_mmio_read.lock().take() else {
            return Ok(());
        };
        if pending.len == 0 || pending.len > 8 {
            return Err(VfsError::InvalidInput);
        }
        set_core_gpr(vcpu, pending.reg, self.read_run_mmio_data(pending.len))
    }

    fn run_page_ptr(&self) -> *mut u8 {
        kaddr_layout::p2v(self.run_page.phys_addr().as_usize()) as *mut u8
    }

    fn write_run_u8(&self, offset: usize, value: u8) {
        // SAFETY: `offset` names a byte field within the allocated kvm_run page.
        let ptr = unsafe { self.run_page_ptr().add(offset) };
        // SAFETY: `ptr` addresses that in-bounds byte field.
        unsafe { ptr.write(value) };
    }

    fn write_run_u32(&self, offset: usize, value: u32) {
        // SAFETY: `offset` names a naturally aligned u32 field within kvm_run.
        let ptr = unsafe { self.run_page_ptr().add(offset) } as *mut u32;
        // SAFETY: `ptr` is aligned for u32 and lies inside the run page.
        unsafe { ptr.write(value) };
    }

    fn write_run_u64(&self, offset: usize, value: u64) {
        // SAFETY: `offset` names a naturally aligned u64 field within kvm_run.
        let ptr = unsafe { self.run_page_ptr().add(offset) } as *mut u64;
        // SAFETY: `ptr` is aligned for u64 and lies inside the run page.
        unsafe { ptr.write(value) };
    }

    fn write_run_mmio_data(&self, value: u64, len: usize) {
        let bytes = value.to_le_bytes();
        // SAFETY: `data[8]` lives inside the single kvm_run page.
        let dst = unsafe { self.run_page_ptr().add(KVM_RUN_MMIO_DATA_OFF) };
        // SAFETY: `len` comes from an MMIO access size and is clamped to the
        // eight-byte data field, so source and destination do not overlap.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, len.min(8)) };
    }

    fn read_run_mmio_data(&self, len: usize) -> u64 {
        let mut bytes = [0u8; 8];
        // SAFETY: caller validates `len <= 8`; the source lies in the run page.
        let src = unsafe { self.run_page_ptr().add(KVM_RUN_MMIO_DATA_OFF) };
        // SAFETY: `len` is at most the eight-byte field; the buffers do not overlap.
        unsafe { core::ptr::copy_nonoverlapping(src, bytes.as_mut_ptr(), len) };
        u64::from_le_bytes(bytes)
    }

    /// Publish an MMIO exit to lkvm and remember a read result for writeback.
    ///
    /// lkvm never touches the guest PC itself: this kernel advances it when the
    /// access is decoded, and lkvm's answer arrives through
    /// `complete_pending_mmio_read` on the next `KVM_RUN`.
    fn report_userspace_mmio(&self, access: MmioExit) {
        self.write_run_u32(KVM_RUN_EXIT_REASON_OFF, KVM_EXIT_MMIO);
        self.write_run_u64(KVM_RUN_MMIO_PHYS_ADDR_OFF, access.gpa);
        self.write_run_u32(KVM_RUN_MMIO_LEN_OFF, u32::from(access.size));
        self.write_run_u8(KVM_RUN_MMIO_IS_WRITE_OFF, u8::from(access.is_write));
        self.write_run_mmio_data(access.value, usize::from(access.size));
        let log_index = self.userspace_mmio_logs.fetch_add(1, Ordering::Relaxed);
        if log_index < 64 {
            log::info!(
                "[kvm] userspace MMIO exit #{} gpa={:#x} {}{} value={:#x}",
                log_index,
                access.gpa,
                if access.is_write { "write" } else { "read" },
                access.size,
                access.value,
            );
        }
        // A read into the architecture's hardwired-zero register has no
        // architectural destination; skip the writeback rather than failing the
        // following `KVM_RUN` on an out-of-range index. The zero register is
        // x0 on riscv64 but XZR (31) on aarch64, so this must stay
        // architecture-specific.
        *self.pending_mmio_read.lock() = (!access.is_write
            && !arch::mmio_reg_is_discarded(access.reg))
        .then_some(PendingMmioRead {
            reg: access.reg,
            len: usize::from(access.size),
        });
    }

    fn report_shutdown(&self) {
        self.write_run_u32(KVM_RUN_EXIT_REASON_OFF, KVM_EXIT_SYSTEM_EVENT);
        self.write_run_u32(KVM_RUN_SYSTEM_EVENT_TYPE_OFF, KVM_SYSTEM_EVENT_SHUTDOWN);
        self.write_run_u32(KVM_RUN_SYSTEM_EVENT_NDATA_OFF, 0);
    }

    fn log_kernel_mmio(&self, access: &MmioExit) {
        let log_index = self.kernel_mmio_logs.fetch_add(1, Ordering::Relaxed);
        if log_index < 64 {
            log::info!(
                "[kvm] kernel MMIO #{} gpa={:#x} {}{} value={:#x}",
                log_index,
                access.gpa,
                if access.is_write { "write" } else { "read" },
                access.size,
                access.value,
            );
        }
    }

    fn one_reg(&self, cmd: u32, arg: usize) -> VfsResult<usize> {
        let reg = UserConstPtr::<KvmOneReg>::from(arg).read_vm()?;
        match cmd {
            KVM_GET_ONE_REG => {
                let value = {
                    let guard = self.vcpu.lock();
                    let Some(vcpu) = guard.as_ref() else {
                        return Err(VfsError::ResourceBusy);
                    };
                    arch::get_one_reg(vcpu, reg.id)?
                };
                UserPtr::<u64>::from(reg.addr as usize).write_vm(value)?;
                Ok(0)
            }
            KVM_SET_ONE_REG => {
                let value = UserConstPtr::<u64>::from(reg.addr as usize).read_vm()?;
                let mut guard = self.vcpu.lock();
                let Some(vcpu) = guard.as_mut() else {
                    return Err(VfsError::ResourceBusy);
                };
                arch::set_one_reg(vcpu, reg.id, value)
            }
            _ => Err(VfsError::InvalidInput),
        }
    }

    fn patch_guest_dtb_bootargs(&self, vcpu: &Vcpu<CurrentArch>) {
        if self.dtb_patched.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(dtb_user_addr) = arch::guest_dtb_user_addr(&self.vm_file, vcpu) else {
            log::info!("[kvm] DTB patch skipped: boot DTB pointer is not mapped RAM");
            return;
        };
        patch_guest_dtb(dtb_user_addr);
    }
}

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;
const FDT_SCAN_LEN: usize = 64 * 1024;

/// An in-place device tree property rewrite an architecture asks for.
struct DtbPatch {
    path: &'static str,
    prop: &'static str,
    value: &'static [u8],
    label: &'static str,
}

fn patch_guest_dtb(dtb_user_addr: usize) {
    let mut buf = Vec::new();
    for offset in 0..FDT_SCAN_LEN {
        match UserConstPtr::<u8>::from(dtb_user_addr + offset).read_vm() {
            Ok(byte) => buf.push(byte),
            Err(err) => {
                log::info!("[kvm] DTB scan stopped at {:#x}: {:?}", offset, err);
                break;
            }
        }
    }

    for patch in arch::DTB_PATCHES {
        patch_dtb_prop(
            dtb_user_addr,
            &buf,
            patch.path,
            patch.prop,
            patch.value,
            patch.label,
        );
    }
    patch_dtb_bootargs(dtb_user_addr, &buf);
}

fn patch_dtb_bootargs(dtb_user_addr: usize, buf: &[u8]) {
    let extra = arch::EXTRA_BOOTARGS;
    if extra.is_empty() {
        return;
    }
    let Some((data_off, data_len)) = find_dtb_prop(buf, "/chosen", "bootargs") else {
        log::info!("[kvm] DTB bootargs patch skipped: property not found");
        return;
    };

    let old_len = buf[data_off..data_off + data_len]
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(data_len);
    if old_len + extra.len() + 1 > data_len {
        log::info!(
            "[kvm] DTB bootargs patch skipped: appended len {} > old len {}",
            old_len + extra.len() + 1,
            data_len,
        );
        return;
    }

    for (i, byte) in extra.iter().copied().enumerate() {
        if let Err(err) = UserPtr::<u8>::from(dtb_user_addr + data_off + old_len + i).write_vm(byte)
        {
            log::info!(
                "[kvm] DTB bootargs patch failed at offset {:#x}: {:?}",
                data_off + old_len + i,
                err,
            );
            return;
        }
    }
    if let Err(err) =
        UserPtr::<u8>::from(dtb_user_addr + data_off + old_len + extra.len()).write_vm(0)
    {
        log::info!(
            "[kvm] DTB bootargs patch failed at offset {:#x}: {:?}",
            data_off + old_len + extra.len(),
            err,
        );
        return;
    }
    log::info!(
        "[kvm] DTB bootargs appended @ {:#x} len={} extra_len={}",
        data_off,
        data_len,
        extra.len(),
    );
}

/// Overwrite a property's bytes in place, padding the remainder with zeros.
fn patch_dtb_prop(
    dtb_user_addr: usize,
    buf: &[u8],
    path: &str,
    prop: &str,
    value: &[u8],
    label: &str,
) {
    let Some((data_off, data_len)) = find_dtb_prop(buf, path, prop) else {
        log::info!("[kvm] DTB {label} patch skipped: property not found");
        return;
    };
    if value.len() > data_len {
        log::info!(
            "[kvm] DTB {label} patch skipped: new len {} > old len {}",
            value.len(),
            data_len,
        );
        return;
    }

    for (i, byte) in value
        .iter()
        .copied()
        .chain(core::iter::repeat(0))
        .take(data_len)
        .enumerate()
    {
        if let Err(err) = UserPtr::<u8>::from(dtb_user_addr + data_off + i).write_vm(byte) {
            log::info!(
                "[kvm] DTB {label} patch failed at offset {:#x}: {:?}",
                data_off + i,
                err,
            );
            return;
        }
    }
    log::info!(
        "[kvm] DTB {label} patched @ {:#x} len={}",
        data_off,
        data_len,
    );
}

fn find_dtb_prop(buf: &[u8], wanted_path: &str, wanted_prop: &str) -> Option<(usize, usize)> {
    if buf.len() < 40 || fdt_be32(buf, 0) != FDT_MAGIC {
        return None;
    }
    let off_struct = fdt_be32(buf, 8) as usize;
    let off_strings = fdt_be32(buf, 12) as usize;
    let size_strings = fdt_be32(buf, 32) as usize;
    if off_struct >= buf.len() || off_strings.checked_add(size_strings)? > buf.len() {
        return None;
    }

    let strings = &buf[off_strings..off_strings + size_strings];
    let mut pos = off_struct;
    let mut path = String::new();
    let mut stack: Vec<usize> = Vec::new();
    while pos + 4 <= buf.len() {
        let token = fdt_be32(buf, pos);
        pos += 4;
        match token {
            FDT_BEGIN_NODE => {
                let name_start = pos;
                while pos < buf.len() && buf[pos] != 0 {
                    pos += 1;
                }
                if pos >= buf.len() {
                    return None;
                }
                let name = core::str::from_utf8(&buf[name_start..pos]).ok()?;
                pos = fdt_align4(pos + 1);
                stack.push(path.len());
                if !name.is_empty() {
                    path.push('/');
                    path.push_str(name);
                }
            }
            FDT_END_NODE => path.truncate(stack.pop()?),
            FDT_PROP => {
                if pos + 8 > buf.len() {
                    return None;
                }
                let len = fdt_be32(buf, pos) as usize;
                let nameoff = fdt_be32(buf, pos + 4) as usize;
                pos += 8;
                if pos.checked_add(len)? > buf.len() {
                    return None;
                }
                if path == wanted_path && fdt_string(strings, nameoff) == wanted_prop {
                    return Some((pos, len));
                }
                pos = fdt_align4(pos + len);
            }
            FDT_NOP => {}
            FDT_END => return None,
            _ => return None,
        }
    }
    None
}

fn fdt_be32(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn fdt_align4(pos: usize) -> usize {
    (pos + 3) & !3
}

fn fdt_string(strings: &[u8], off: usize) -> &str {
    if off >= strings.len() {
        return "<bad-nameoff>";
    }
    let mut end = off;
    while end < strings.len() && strings[end] != 0 {
        end += 1;
    }
    core::str::from_utf8(&strings[off..end]).unwrap_or("<bad-prop>")
}

/// Write a value into a guest general-purpose register.
///
/// Both architectures expose the guest's GPR file as `gprs`, so this is shared
/// rather than per-architecture.
fn set_core_gpr(vcpu: &mut Vcpu<CurrentArch>, reg: usize, value: u64) -> VfsResult<()> {
    if reg >= vcpu.arch.gprs.len() {
        return Err(VfsError::InvalidInput);
    }
    vcpu.arch.gprs[reg] = value;
    Ok(())
}

impl FileOperations for KvmVmFops {
    fn ioctl(&self, file: &VfsFile, _cred: &kcred::Cred, cmd: u32, arg: usize) -> VfsResult<usize> {
        let vm = KvmVmFile::from_file(file)?;
        match cmd {
            KVM_CREATE_VCPU => vm.create_vcpu_fd(file, arg),
            KVM_SET_USER_MEMORY_REGION => vm.set_user_memory_region(arg),
            // Accepted and ignored: lkvm treats a failure here as fatal when
            // setting up virtio, and our guest's queue-notify accesses still
            // reach lkvm through the ordinary MMIO exit path.
            KVM_IOEVENTFD => Ok(0),
            // Must keep failing so that lkvm's RISC-V build falls back from an
            // in-kernel AIA to its userspace-emulated PLIC.
            KVM_CREATE_DEVICE => Err(VfsError::Unsupported),
            KVM_SET_DEVICE_ATTR | KVM_GET_DEVICE_ATTR | KVM_HAS_DEVICE_ATTR => {
                Err(VfsError::InvalidInput)
            }
            _ => arch::vm_ioctl(&vm, cmd, arg).unwrap_or(Err(VfsError::NotATty)),
        }
    }
}

impl FileOperations for KvmVcpuFops {
    fn ioctl(&self, file: &VfsFile, _cred: &kcred::Cred, cmd: u32, arg: usize) -> VfsResult<usize> {
        let vcpu = KvmVcpuFile::from_file(file)?;
        match cmd {
            KVM_GET_ONE_REG | KVM_SET_ONE_REG => vcpu.one_reg(cmd, arg),
            KVM_RUN => vcpu.run(),
            _ => arch::vcpu_ioctl(&vcpu, cmd, arg).unwrap_or(Err(VfsError::NotATty)),
        }
    }

    fn mmap(&self, file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        if mapper.offset() != 0 {
            return Err(VfsError::InvalidInput);
        }
        let vcpu = KvmVcpuFile::from_file(file)?;
        mapper.map_physical(PhysAddrRange::from_start_size(
            vcpu.run_page.phys_addr(),
            KVM_RUN_MMAP_SIZE,
        ))
    }
}
