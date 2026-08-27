// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host IRQ routes that wake vCPU threads for guest interrupt delivery.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU8, Ordering};

use kirq::IrqEvent;
use kspin::SpinNoIrq;

pub(crate) const HOST_VTIMER_IRQ: u32 = 27;
const ROUTE_UNUSED: u8 = 0;
const ROUTE_REGISTERING: u8 = 1;
const ROUTE_REGISTERED: u8 = 2;

static HOST_VTIMER_ROUTE_STATE: AtomicU8 = AtomicU8::new(ROUTE_UNUSED);

static OWNER_TASKS: [SpinNoIrq<Option<ktask::KtaskRef>>; kbuild_config::NR_CPUS] =
    [const { SpinNoIrq::new(None) }; kbuild_config::NR_CPUS];

/// Publish the current task as the vCPU owner for the current physical CPU.
pub(crate) fn publish_owner_for_current_cpu() {
    let cpu = khal::percpu::this_cpu_id().as_usize();
    *OWNER_TASKS[cpu].lock() = Some(ktask::current().clone());
}

/// Remove the current task from every per-CPU owner slot.
pub(crate) fn clear_owner_for_current_task() {
    let current = ktask::current().clone();
    for owner in &OWNER_TASKS {
        let mut guard = owner.lock();
        if guard
            .as_ref()
            .is_some_and(|task| Arc::ptr_eq(task, &current))
        {
            *guard = None;
        }
    }
}

/// Register or enable the host-backed guest virtual timer IRQ route.
pub(crate) fn set_host_vtimer_irq_enabled(enabled: bool) {
    let desc = kirq::gic_level_irq_desc(HOST_VTIMER_IRQ as usize);
    if enabled {
        if !ensure_host_vtimer_route_registered(desc) {
            return;
        }
    } else if HOST_VTIMER_ROUTE_STATE.load(Ordering::Acquire) != ROUTE_REGISTERED {
        return;
    }
    kirq::enable(desc, enabled);
}

/// Return the hardware INTID backing `guest_irq`, if this route uses GIC LR HW mode.
pub(crate) fn host_hwirq_for_guest_irq(guest_irq: u32) -> Option<u32> {
    (guest_irq == HOST_VTIMER_IRQ
        && HOST_VTIMER_ROUTE_STATE.load(Ordering::Acquire) == ROUTE_REGISTERED)
        .then_some(HOST_VTIMER_IRQ)
}

fn ensure_host_vtimer_route_registered(desc: kirq::IrqDesc) -> bool {
    match HOST_VTIMER_ROUTE_STATE.compare_exchange(
        ROUTE_UNUSED,
        ROUTE_REGISTERING,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {}
        Err(ROUTE_REGISTERED) => return true,
        Err(_) => return false,
    }

    let virq = match kirq::try_map(desc) {
        Ok(virq) => virq,
        Err(err) => {
            HOST_VTIMER_ROUTE_STATE.store(ROUTE_UNUSED, Ordering::Release);
            log::warn!(
                "[kvmm] failed to map host vtimer IRQ hwirq {}: {:?}",
                HOST_VTIMER_IRQ,
                err,
            );
            return false;
        }
    };

    if !kirq::register_disabled(desc, Arc::new(move |irq| host_irq_route_handler(irq, virq))) {
        HOST_VTIMER_ROUTE_STATE.store(ROUTE_UNUSED, Ordering::Release);
        log::warn!(
            "[kvmm] failed to register host vtimer IRQ hwirq {}",
            HOST_VTIMER_IRQ,
        );
        return false;
    }

    HOST_VTIMER_ROUTE_STATE.store(ROUTE_REGISTERED, Ordering::Release);
    true
}

fn host_irq_route_handler(irq: usize, route_virq: usize) -> IrqEvent {
    if irq != route_virq {
        return IrqEvent::NOT_HANDLED;
    }

    let task = {
        let cpu = khal::percpu::this_cpu_id().as_usize();
        OWNER_TASKS[cpu].lock().clone()
    };
    if let Some(task) = task {
        ktask::interrupt_task(&task, true);
    }
    IrqEvent::HANDLED
}
