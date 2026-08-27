// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VM control block.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};

use crate::{
    arch::VmmArch,
    mm::{GuestMem, mmio::MmioBus},
    vcpu::{MAX_VCPUS, Vcpu},
    vcpu_state::{VcpuRunState, VcpuStats},
    vdev::VmDevices,
    vm_info::{VmInfo, register_vm},
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
    devices: VmDevices<A>,
    vcpu_pcpu: [AtomicI32; MAX_VCPUS],
    /// Coarse per-vCPU execution state (see [`VcpuRunState`]).
    vcpu_run_state: [AtomicU8; MAX_VCPUS],
    /// Kernel task that owns and runs each vCPU, published once the vCPU
    /// thread is spawned. Lets [`inject_irq`](Self::inject_irq) wake a vCPU
    /// parked in the WFI path.
    vcpu_tasks: [ksync::Mutex<Option<ktask::KtaskRef>>; MAX_VCPUS],
    /// PSCI power state for each vCPU. vCPU0 is marked on when boot starts;
    /// secondaries transition from off to on through CPU_ON.
    cpu_on: [AtomicBool; MAX_VCPUS],
    /// Per-vCPU profiling counters surfaced via `/proc/kvmm`.
    stats: [VcpuStats; MAX_VCPUS],
    /// Host monotonic ticks at VM creation, for uptime reporting.
    created_ticks: u64,
    nr_vcpus: usize,
    /// Cleared by [`shutdown`](Self::shutdown) so the VM drops out of
    /// `/proc/kvmm`.
    active: AtomicBool,
    /// Set by [`request_stop`](Self::request_stop) to ask every vCPU thread to
    /// leave its run loop at the next iteration so the VM can be torn down.
    stop_requested: AtomicBool,
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
        self.devices.mmio_bus()
    }

    pub fn devices(&self) -> &VmDevices<A> {
        &self.devices
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

    /// Record the coarse execution state for vCPU `id`.
    pub fn set_vcpu_run_state(&self, id: u32, state: VcpuRunState) {
        self.vcpu_run_state[id as usize].store(state as u8, Ordering::Release);
    }

    /// Get the coarse execution state for vCPU `id`.
    pub fn vcpu_run_state(&self, id: u32) -> VcpuRunState {
        VcpuRunState::from_raw(self.vcpu_run_state[id as usize].load(Ordering::Acquire))
    }

    /// Publish the kernel task that owns and runs vCPU `id`.
    pub fn set_vcpu_task(&self, id: u32, task: ktask::KtaskRef) {
        if (id as usize) < self.nr_vcpus {
            *self.vcpu_tasks[id as usize].lock() = Some(task);
        }
    }

    /// Atomically mark a vCPU as powered on. Returns true if this call won the
    /// transition from off to on.
    pub fn try_mark_cpu_on(&self, id: u32) -> bool {
        if id as usize >= self.nr_vcpus {
            return false;
        }
        self.cpu_on[id as usize]
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Inject a virtual IRQ into the target vCPU.
    ///
    /// Records the pending bit in the vGIC (delivered via the GICH list
    /// registers on the next guest entry) and, if the target is parked in the
    /// VMM WFI path, wakes it so delivery is not delayed.
    pub fn inject_irq(&self, vcpu_id: u32, irq: u32) {
        if vcpu_id as usize >= self.nr_vcpus {
            return;
        }
        self.devices.inject_irq(vcpu_id, irq);
        // TODO(irq): if the target vCPU is RunningGuest on another pCPU, send a
        //            physical IPI to force an exit + re-injection (kill 0-1 tick
        //            latency); host-side exit handling needs no wake.
        if self.vcpu_run_state(vcpu_id) == VcpuRunState::WfiSleeping
            && let Some(task) = self.vcpu_tasks[vcpu_id as usize].lock().clone()
        {
            ktask::interrupt_task(&task, true);
        }
    }

    fn wake_vcpu_if_waiting(&self, vcpu_id: u32) {
        if vcpu_id as usize >= self.nr_vcpus {
            return;
        }
        match self.vcpu_run_state(vcpu_id) {
            VcpuRunState::WfiSleeping => {
                if let Some(task) = self.vcpu_tasks[vcpu_id as usize].lock().clone() {
                    ktask::interrupt_task(&task, true);
                }
            }
            VcpuRunState::RunningGuest => {
                let pcpu = self.vcpu_pcpu(vcpu_id);
                if pcpu >= 0 {
                    kirq::notify_cpu(
                        kbuild_config::IPI_IRQ,
                        kirq::TargetCpu::Specific(pcpu as usize),
                    );
                }
            }
            _ => {}
        }
    }

    /// Per-vCPU profiling stats.
    pub fn vcpu_stats(&self, id: u32) -> &VcpuStats {
        &self.stats[id as usize]
    }

    /// Mark this VM as shut down so it no longer appears in `/proc/kvmm`.
    pub fn shutdown(&self) {
        self.active.store(false, Ordering::Release);
    }

    /// Ask every vCPU thread to leave its run loop.
    ///
    /// Sets the stop flag observed at the top of [`vmm_run_vcpu`] and wakes any
    /// vCPU parked in the WFI path so it re-checks the flag promptly. This only
    /// requests the stop; use [`Vm::stop_and_join`] to also wait for the vCPU
    /// threads to exit.
    ///
    /// A vCPU spinning in guest mode without taking any exit will not observe
    /// the request until its next exit (timer tick, hypercall, etc.).
    pub fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::Release);
        self.active.store(false, Ordering::Release);
        for slot in &self.vcpu_tasks {
            if let Some(task) = slot.lock().clone() {
                ktask::interrupt_task(&task, true);
            }
        }
    }

    /// Whether a stop has been requested via [`request_stop`](Self::request_stop).
    pub fn is_stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::Acquire)
    }
}

impl<A: VmmArch> crate::vdev::VcpuWaker for VmShared<A> {
    fn wake_vcpu(&self, vcpu_id: u32) {
        self.wake_vcpu_if_waiting(vcpu_id);
    }
}

impl<A: VmmArch> VmInfo for VmShared<A> {
    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn nr_vcpus(&self) -> usize {
        self.nr_vcpus
    }

    fn mem_base(&self) -> u64 {
        self.cfg.mem_base
    }

    fn mem_size(&self) -> u64 {
        self.cfg.mem_size
    }

    fn created_ticks(&self) -> u64 {
        self.created_ticks
    }

    fn vcpu_pcpu(&self, id: u32) -> i32 {
        VmShared::vcpu_pcpu(self, id)
    }

    fn vcpu_run_state(&self, id: u32) -> VcpuRunState {
        VmShared::vcpu_run_state(self, id)
    }

    fn vcpu_guest_ticks(&self, id: u32) -> u64 {
        self.stats[id as usize].guest_ticks.load(Ordering::Relaxed)
    }

    fn vcpu_exit_ticks(&self, id: u32) -> u64 {
        self.stats[id as usize].exit_ticks.load(Ordering::Relaxed)
    }

    fn vcpu_exit_count(&self, id: u32) -> u64 {
        self.stats[id as usize].exit_count.load(Ordering::Relaxed)
    }

    fn vcpu_exit_breakdown(&self, id: u32) -> [u64; 5] {
        let s = &self.stats[id as usize];
        [
            s.exits_halt.load(Ordering::Relaxed),
            s.exits_hypercall.load(Ordering::Relaxed),
            s.exits_mmio.load(Ordering::Relaxed),
            s.exits_interrupt.load(Ordering::Relaxed),
            s.exits_other.load(Ordering::Relaxed),
        ]
    }

    fn device_names(&self) -> Vec<(String, u64)> {
        self.devices.device_names()
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

impl<A: VmmArch + 'static> Vm<A> {
    /// Create a new VM with the given configuration.
    pub fn new(cfg: VmConfig) -> Option<Self> {
        if cfg.nr_vcpus == 0 || cfg.nr_vcpus > MAX_VCPUS {
            log::error!("[vmm] vm_create: invalid nr_vcpus={}", cfg.nr_vcpus);
            return None;
        }

        let guest_mem = if cfg.mem_size > 0 {
            if !crate::mm::reserve_guest_ram(cfg.mem_base, cfg.mem_size) {
                log::error!("[vmm] vm_create: guest RAM reservation failed");
                return None;
            }
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
            devices: VmDevices::new(),
            vcpu_pcpu: core::array::from_fn(|_| AtomicI32::new(PCPU_NONE)),
            vcpu_run_state: core::array::from_fn(|_| AtomicU8::new(VcpuRunState::Offline as u8)),
            vcpu_tasks: core::array::from_fn(|_| ksync::Mutex::new(None)),
            cpu_on: core::array::from_fn(|_| AtomicBool::new(false)),
            stats: core::array::from_fn(|_| VcpuStats::new()),
            created_ticks: khal::time::now_ticks().as_raw(),
            nr_vcpus,
            active: AtomicBool::new(true),
            stop_requested: AtomicBool::new(false),
        });

        Some(Self { shared })
    }

    /// Publish this VM in the global registry so it appears in `/proc/kvmm`.
    ///
    /// Call this once, after any `guest_mem_mut()`-based setup — the registry
    /// holds a `Weak`, and an outstanding `Weak` would make `Arc::get_mut`
    /// (used by [`guest_mem_mut`](Self::guest_mem_mut)) fail.
    pub fn register(&self) {
        register_vm(&(self.shared.clone() as Arc<dyn VmInfo>));
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

    /// Mark this VM as shut down (removes it from `/proc/kvmm`).
    pub fn shutdown(&self) {
        self.shared.shutdown();
    }

    /// Request every vCPU thread to stop and block until they have all exited.
    ///
    /// After this returns, no vCPU thread holds a reference to this VM's shared
    /// state, so dropping the last `Vm` handle releases the guest memory. Safe
    /// to call more than once; already-exited vCPUs join immediately.
    pub fn stop_and_join(&self) {
        self.shared.request_stop();
        for slot in &self.shared.vcpu_tasks {
            let task = slot.lock().take();
            if let Some(task) = task {
                task.join();
            }
        }
    }
}
