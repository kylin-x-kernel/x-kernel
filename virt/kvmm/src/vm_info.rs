// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Type-erased VM introspection registry used by `/proc/kvmm`.

use alloc::{
    fmt::Write,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};

use crate::vcpu_state::VcpuRunState;

/// Type-erased VM information for the global registry, used by `/proc/kvmm`.
pub trait VmInfo: Send + Sync {
    fn is_active(&self) -> bool;
    fn nr_vcpus(&self) -> usize;
    fn mem_base(&self) -> u64;
    fn mem_size(&self) -> u64;
    fn created_ticks(&self) -> u64;
    fn vcpu_pcpu(&self, id: u32) -> i32;
    fn vcpu_run_state(&self, id: u32) -> VcpuRunState;
    fn vcpu_guest_ticks(&self, id: u32) -> u64;
    fn vcpu_exit_ticks(&self, id: u32) -> u64;
    fn vcpu_exit_count(&self, id: u32) -> u64;
    fn vcpu_exit_breakdown(&self, id: u32) -> [u64; 5];
    fn device_names(&self) -> Vec<(String, u64)>;
}

static VM_REGISTRY: ksync::Mutex<Vec<Weak<dyn VmInfo>>> = ksync::Mutex::new(Vec::new());

pub(crate) fn register_vm(vm: &Arc<dyn VmInfo>) {
    VM_REGISTRY.lock().push(Arc::downgrade(vm));
}

/// Format a snapshot of all live VMs for `/proc/kvmm`.
pub fn dump_vm_info() -> String {
    let mut reg = VM_REGISTRY.lock();
    reg.retain(|w| w.upgrade().is_some_and(|vm| vm.is_active()));

    let freq = khal::time::freq();
    let now = khal::time::now_ticks();
    let mut out = String::new();
    let _ = writeln!(out, "VMs: {}", reg.len());

    for (idx, weak) in reg.iter().enumerate() {
        let Some(vm) = weak.upgrade() else { continue };

        let uptime_ms = ticks_to_us(now.as_raw().wrapping_sub(vm.created_ticks()), freq) / 1000;
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "[VM {}] up {}.{:03}s",
            idx,
            uptime_ms / 1000,
            uptime_ms % 1000,
        );
        let nr = vm.nr_vcpus();
        let _ = writeln!(out, "  vCPUs: {}", nr);
        let _ = writeln!(out, "  Memory: {:#x} + {:#x}", vm.mem_base(), vm.mem_size());
        for i in 0..nr as u32 {
            let pcpu = vm.vcpu_pcpu(i);
            let state = vm.vcpu_run_state(i);
            let exits = vm.vcpu_exit_count(i);
            let guest_t = vm.vcpu_guest_ticks(i);
            let exit_t = vm.vcpu_exit_ticks(i);
            let guest_us = ticks_to_us(guest_t, freq);
            let exit_us = ticks_to_us(exit_t, freq);
            let total = guest_t + exit_t;
            let util = (guest_t * 100).checked_div(total).unwrap_or(0);
            let bd = vm.vcpu_exit_breakdown(i);

            if pcpu >= 0 {
                let _ = write!(out, "  vCPU {}: pCPU {} state={}", i, pcpu, state.as_str());
            } else {
                let _ = write!(out, "  vCPU {}: offline state={}", i, state.as_str());
            }
            let _ = writeln!(
                out,
                " util={}% exits={} guest={}.{:03}ms exit={}.{:03}ms",
                util,
                exits,
                guest_us / 1000,
                guest_us % 1000,
                exit_us / 1000,
                exit_us % 1000,
            );
            let _ = writeln!(
                out,
                "    halt={} hcall={} mmio={} irq={} other={}",
                bd[0], bd[1], bd[2], bd[3], bd[4],
            );
        }
        let devices = vm.device_names();
        if devices.is_empty() {
            let _ = writeln!(out, "  Devices: (none)");
        } else {
            for (name, base) in &devices {
                let _ = writeln!(out, "  Device: {} @ {:#x}", name, base);
            }
        }
    }

    out
}

fn ticks_to_us(ticks: u64, freq: u64) -> u64 {
    if freq == 0 {
        return 0;
    }
    ticks * 1_000_000 / freq
}
