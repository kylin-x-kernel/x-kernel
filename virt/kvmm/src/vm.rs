// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VM control block.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicI32, Ordering};

use crate::{
    arch::VmmArch,
    mm::{GuestMem, mmio::MmioBus},
    vcpu::{MAX_VCPUS, Vcpu},
};

/// VM configuration.
#[derive(Debug, Clone)]
pub struct VmConfig {
    mem_base: u64,
    mem_size: u64,
    nr_vcpus: usize,
}

impl VmConfig {
    pub fn new(mem_base: u64, mem_size: u64, nr_vcpus: usize) -> Self {
        Self {
            mem_base,
            mem_size,
            nr_vcpus,
        }
    }

    pub fn mem_base(&self) -> u64 {
        self.mem_base
    }

    pub fn mem_size(&self) -> u64 {
        self.mem_size
    }

    pub fn nr_vcpus(&self) -> usize {
        self.nr_vcpus
    }
}

const PCPU_NONE: i32 = -1;

/// Shared VM state accessible from every vCPU via `Arc`.
///
/// Contains all VM-level resources: guest memory, MMIO bus, and
/// per-vCPU physical CPU tracking. vCPU exit handlers access this
/// through `vcpu.vm` to dispatch MMIO, query sibling vCPUs, etc.
pub struct VmShared<A: VmmArch> {
    cfg: VmConfig,
    guest_mem: Option<A::GuestMem>,
    mmio_bus: ksync::Mutex<MmioBus>,
    vcpu_pcpu: [AtomicI32; MAX_VCPUS],
    nr_vcpus: usize,
}

/// Reference-counted handle to a VM's shared state.
pub type VmRef<A> = Arc<VmShared<A>>;

impl<A: VmmArch> VmShared<A> {
    pub fn cfg(&self) -> &VmConfig {
        &self.cfg
    }

    pub fn guest_mem(&self) -> Option<&A::GuestMem> {
        self.guest_mem.as_ref()
    }

    pub fn mmio_bus(&self) -> &ksync::Mutex<MmioBus> {
        &self.mmio_bus
    }

    pub fn nr_vcpus(&self) -> usize {
        self.nr_vcpus
    }

    /// Record that vCPU `id` is now running on physical CPU `pcpu`.
    pub fn set_vcpu_pcpu(&self, id: u32, pcpu: i32) {
        self.vcpu_pcpu[id as usize].store(pcpu, Ordering::Release);
    }

    /// Get the physical CPU that vCPU `id` is running on, or -1.
    pub fn vcpu_pcpu(&self, id: u32) -> i32 {
        self.vcpu_pcpu[id as usize].load(Ordering::Acquire)
    }
}

/// Virtual machine creator handle.
///
/// Holds an `Arc<VmShared>` and provides methods to create vCPUs
/// that share the same VM context. The `Vm` handle is typically
/// kept alive while the VM is running; dropping it only decrements
/// the reference count (vCPU-held `Arc`s keep the VM alive).
pub struct Vm<A: VmmArch> {
    shared: Arc<VmShared<A>>,
}

impl<A: VmmArch> Vm<A> {
    /// Create a new VM with the given configuration.
    pub fn new(cfg: VmConfig) -> Option<Self> {
        if cfg.nr_vcpus == 0 || cfg.nr_vcpus > MAX_VCPUS {
            log::error!("[vmm] vm_create: invalid nr_vcpus={}", cfg.nr_vcpus);
            return None;
        }

        let guest_mem = if cfg.mem_size > 0 {
            let vmid = crate::mm::alloc_vmid();
            let gm = A::GuestMem::new(cfg.mem_base, cfg.mem_size, vmid);
            if gm.is_none() {
                log::error!("[vmm] vm_create: guest_mem alloc failed");
                return None;
            }
            gm
        } else {
            None
        };

        let nr_vcpus = cfg.nr_vcpus;

        log::info!(
            "[vmm] vm_create: {} vCPU(s), mem={:#x}+{:#x} guest_mem={}",
            nr_vcpus,
            cfg.mem_base,
            cfg.mem_size,
            if guest_mem.is_some() { "yes" } else { "no" },
        );

        let shared = Arc::new(VmShared {
            cfg,
            guest_mem,
            mmio_bus: ksync::Mutex::new(MmioBus::new()),
            vcpu_pcpu: core::array::from_fn(|_| AtomicI32::new(PCPU_NONE)),
            nr_vcpus,
        });

        Some(Self { shared })
    }

    /// Get a reference to the shared VM state.
    pub fn shared(&self) -> &Arc<VmShared<A>> {
        &self.shared
    }

    /// Create a vCPU bound to this VM.
    pub fn create_vcpu(&self, id: u32) -> Vcpu<A> {
        Vcpu::new(id, Arc::clone(&self.shared))
    }

    /// Get mutable access to the guest memory (only valid before vCPUs are created).
    pub fn guest_mem_mut(&mut self) -> Option<&mut A::GuestMem> {
        Arc::get_mut(&mut self.shared)?.guest_mem.as_mut()
    }

    /// Activate the second-stage page table for a specific vCPU.
    pub fn activate_vcpu_guest_mem(&self, vcpu: &mut Vcpu<A>) {
        if let Some(gm) = &self.shared.guest_mem {
            A::activate_guest_mem(vcpu, gm);
        }
    }
}
