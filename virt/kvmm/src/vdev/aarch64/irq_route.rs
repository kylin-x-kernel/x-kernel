// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host IRQ routes that wake vCPU threads for guest interrupt delivery.

use alloc::sync::{Arc, Weak};

use kirq::IrqEvent;
use ksync::{Mutex, static_lock};
use lazyinit::LazyInit;

pub(crate) const HOST_VTIMER_IRQ: u32 = 27;

static_lock! {
    static ROUTE_REGISTRATION: Mutex<bool> = Mutex::new(false);
}

unittest::path_mod_test!(
    registration_tests,
    "../../../ut-tests/route_registration.rs"
);

#[cfg(all(unittest, feature = "kernel-tests"))]
kcpu_slot::cpu_slot_cell! { static TEST_DELIVERIES: usize = 0; }

#[cfg(feature = "kernel-tests")]
unittest::path_mod_test!(tests, "../../../ut-tests/irq_route.rs");

struct LocalOwner {
    task: Option<ktask::WeakKtaskRef>,
    timer: Option<Weak<super::vtimer::TimerResidency>>,
}

kcpu_slot::cpu_slot! {
    static LOCAL_OWNER: kspin::SpinNoIrq<crate::vdev::aarch64::irq_route::LocalOwner> =
        kspin::SpinNoIrq::new(crate::vdev::aarch64::irq_route::LocalOwner { task: None, timer: None });
}
static FORWARDING: LazyInit<kirq::PerCpuForwarding> = LazyInit::new();
kcpu_slot::cpu_slot_cell! { static IS_LOCAL_ROUTE_ENABLED: u8 = 0; }

/// Publish the current task as the vCPU owner for the current physical CPU.
pub(crate) fn publish_owner_for_current_cpu() {
    let _guard = kspin::IrqSave::new();
    LOCAL_OWNER.with_current_irq_disabled(|owner| {
        owner.lock().task = Some(ktask::KtaskRef::downgrade(&ktask::current()));
    });
}

pub(crate) fn publish_timer_for_current_cpu(timer: Weak<super::vtimer::TimerResidency>) {
    let _guard = kspin::IrqSave::new();
    LOCAL_OWNER.with_current_irq_disabled(|owner| owner.lock().timer = Some(timer));
    publish_owner_for_current_cpu();
}

/// Retains the executing CPU's resident timer, not another task's saved state.
/// Runs in the owning task or ordinary IRQ context, never NMI, after CPU-slot
/// initialization. Temporarily masks IRQs for lookup; does not sleep or allocate.
/// The returned Arc retains storage, not CPU residency: callers performing local
/// hardware operations must keep IRQ/migration exclusion across lookup and use.
/// Callers must not hold LOCAL_OWNER's lock; it is released before returning,
/// so taking the timer's state lock does not nest it under the owner-slot lock.
fn current_timer() -> Option<Arc<super::vtimer::TimerResidency>> {
    LOCAL_OWNER
        .with_current_irq_disabled(|owner| owner.lock().timer.as_ref().and_then(Weak::upgrade))
}

pub(crate) fn set_guest_active(is_active: bool) {
    let _guard = kspin::IrqSave::new();
    if let Some(timer) = current_timer() {
        timer.set_guest_active(is_active);
    }
}

/// Detach the current resident before this CPU can be reused.
pub(crate) fn clear_owner_for_current_task() {
    LOCAL_OWNER.with_current_irq_disabled(|owner| {
        *owner.lock() = LocalOwner {
            task: None,
            timer: None,
        };
    });
}

/// Update the resident timer's enablement after sleepable route preparation.
pub(crate) fn set_host_vtimer_irq_enabled(enabled: bool) {
    if FORWARDING.get().is_none() {
        return;
    }
    let _guard = kspin::IrqSave::new();
    if let Some(timer) = current_timer() {
        timer.set_irq_enabled(enabled);
    }
}

pub(crate) fn set_local_forwarding(is_resident: bool, enabled: bool, is_active: bool) -> bool {
    let Some(forwarding) = FORWARDING.get() else {
        return !enabled;
    };
    let desc = kirq::gic_level_irq_desc(HOST_VTIMER_IRQ as usize);
    if IS_LOCAL_ROUTE_ENABLED.load_current() != 0 {
        if kirq::try_enable(desc, false).is_err() {
            return false;
        }
        IS_LOCAL_ROUTE_ENABLED.store_current(0);
    }
    // SAFETY: the route is permanently registered and exclusively owned here.
    // Callers hold IRQ exclusion; puts stop CNTV before revocation. Load/enable
    // reconstructs active state while delivery is masked, before re-enabling.
    unsafe {
        forwarding.set_local_state(if is_resident {
            kirq::PerCpuForwardingState::Guest { is_active }
        } else {
            kirq::PerCpuForwardingState::Host
        });
    }
    if enabled {
        if kirq::try_enable(desc, true).is_err() {
            return false;
        }
        IS_LOCAL_ROUTE_ENABLED.store_current(1);
    }
    true
}

/// Return the hardware INTID backing `guest_irq`, if this route uses GIC LR HW mode.
pub(crate) fn host_hwirq_for_guest_irq(guest_irq: u32) -> Option<u32> {
    let _guard = kspin::IrqSave::new();
    (guest_irq == HOST_VTIMER_IRQ
        && FORWARDING.get().is_some()
        && current_timer().is_some_and(|timer| timer.is_resident()))
    .then_some(HOST_VTIMER_IRQ)
}

/// Serializes route preparation and retains failed rollback responsibility.
/// The initializer marks pending rollback immediately after action installation.
/// LazyInit publishes the completed capability to lock-free IRQ-side readers;
/// the mutex orders failed attempts and lets contenders sleep until retry.
/// Every initializer of `ready` must use this same gate.
fn prepare_route_once<T>(
    ready: &LazyInit<T>,
    gate: &Mutex<bool>,
    initialize: impl FnOnce(&mut bool) -> Option<T>,
    mut rollback: impl FnMut() -> Result<Option<kirq::Handler>, kirq::IrqDescError>,
) -> bool {
    if ready.get().is_some() {
        return true;
    }
    loop {
        let mut has_pending_rollback = gate.lock();
        if ready.get().is_some() {
            return true;
        }
        if *has_pending_rollback {
            let Some(handler) = finish_route_rollback(&mut has_pending_rollback, rollback()) else {
                return false;
            };
            // Final handler destruction may reenter preparation. Recheck the
            // publication and rollback state after reacquiring the gate.
            drop(has_pending_rollback);
            drop(handler);
            continue;
        }
        if let Some(capability) = initialize(&mut has_pending_rollback) {
            ready.init_once(capability);
            *has_pending_rollback = false;
            return true;
        }
        let removed = if *has_pending_rollback {
            finish_route_rollback(&mut has_pending_rollback, rollback())
        } else {
            None
        };
        drop(has_pending_rollback);
        drop(removed);
        return false;
    }
}

fn finish_route_rollback(
    has_pending_rollback: &mut bool,
    result: Result<Option<kirq::Handler>, kirq::IrqDescError>,
) -> Option<kirq::Handler> {
    match result {
        Ok(Some(handler)) => {
            *has_pending_rollback = false;
            Some(handler)
        }
        Ok(None) => {
            log::warn!("[kvmm] host timer rollback removed no action; preparation remains blocked");
            None
        }
        Err(error) => {
            log::warn!("[kvmm] host timer rollback failed; preparation remains blocked: {error:?}");
            None
        }
    }
}

fn register_host_vtimer_route(
    desc: kirq::IrqDesc,
    has_pending_rollback: &mut bool,
) -> Option<kirq::PerCpuForwarding> {
    let virq = match kirq::try_map(desc) {
        Ok(virq) => virq,
        Err(err) => {
            log::warn!(
                "[kvmm] failed to map host vtimer IRQ hwirq {}: {:?}",
                HOST_VTIMER_IRQ,
                err,
            );
            return None;
        }
    };

    match kirq::try_register_percpu(desc, Arc::new(move |irq| host_irq_route_handler(irq, virq))) {
        Ok(true) => *has_pending_rollback = true,
        Ok(false) => {
            log::warn!("[kvmm] host vtimer IRQ registration rejected: hwirq={HOST_VTIMER_IRQ}");
            return None;
        }
        Err(err) => {
            log::warn!(
                "[kvmm] host vtimer IRQ registration failed: hwirq={HOST_VTIMER_IRQ}, \
                 error={err:?}"
            );
            return None;
        }
    }

    match kirq::prepare_percpu_forwarding(desc) {
        Ok(forwarding) => Some(forwarding),
        Err(err) => {
            log::warn!("[kvmm] host timer forwarding is unavailable: {:?}", err);
            None
        }
    }
}

/// Prepares the permanent route in sleepable task context, without IRQ/spin
/// guards or locks needed by IRQ management. Concurrent callers wait for the
/// current attempt. Failed rollback blocks new registration until cleanup succeeds.
pub(crate) fn prepare_host_vtimer_route() -> bool {
    let desc = kirq::gic_level_irq_desc(HOST_VTIMER_IRQ as usize);
    prepare_route_once(
        &FORWARDING,
        &ROUTE_REGISTRATION,
        |pending| register_host_vtimer_route(desc, pending),
        || {
            // SAFETY: ROUTE_REGISTRATION serializes failed preparations.
            // FORWARDING is still unpublished, so local activation has never
            // occurred and no action execution has been admitted. Rejected
            // claims complete under KIRQ's Line lock. Successful permanent
            // routes never enter this rollback path.
            unsafe { kirq::try_free_percpu_irq(desc) }
        },
    )
}

fn host_irq_route_handler(irq: usize, route_virq: usize) -> IrqEvent {
    if irq != route_virq {
        return IrqEvent::NOT_HANDLED;
    }

    #[cfg(all(unittest, feature = "kernel-tests"))]
    TEST_DELIVERIES.store_current(TEST_DELIVERIES.load_current() + 1);

    let task = LOCAL_OWNER.with_current_irq_disabled(|owner| {
        owner
            .lock()
            .task
            .as_ref()
            .and_then(ktask::WeakKtaskRef::upgrade)
    });
    if let Some(task) = task {
        ktask::interrupt_task(&task, true);
    }
    IrqEvent::HANDLED
}
