// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VM control block.

use crate::{
    arch::VmmArch,
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

/// Virtual machine control block.
pub struct Vm<A: VmmArch> {
    cfg: VmConfig,
    vcpus: [Vcpu<A>; MAX_VCPUS],
    nr_vcpus: usize,
}

impl<A: VmmArch> Vm<A> {
    /// Create a new VM with the given configuration.
    ///
    /// Initializes `nr_vcpus` vCPU control blocks with zeroed state.
    pub fn new(cfg: VmConfig) -> Option<Self> {
        if cfg.nr_vcpus == 0 || cfg.nr_vcpus > MAX_VCPUS {
            log::error!("[vmm] vm_create: invalid nr_vcpus={}", cfg.nr_vcpus);
            return None;
        }

        let vcpus = core::array::from_fn(|i| Vcpu::new(i as u32));
        let nr_vcpus = cfg.nr_vcpus;

        log::info!(
            "[vmm] vm_create: {} vCPU(s), mem={:#x}+{:#x}",
            nr_vcpus,
            cfg.mem_base,
            cfg.mem_size,
        );

        Some(Self {
            cfg,
            vcpus,
            nr_vcpus,
        })
    }

    pub fn cfg(&self) -> &VmConfig {
        &self.cfg
    }

    pub fn vcpu(&self, id: usize) -> Option<&Vcpu<A>> {
        if id < self.nr_vcpus {
            Some(&self.vcpus[id])
        } else {
            None
        }
    }

    pub fn vcpu_mut(&mut self, id: usize) -> Option<&mut Vcpu<A>> {
        if id < self.nr_vcpus {
            Some(&mut self.vcpus[id])
        } else {
            None
        }
    }

    pub fn nr_vcpus(&self) -> usize {
        self.nr_vcpus
    }
}
