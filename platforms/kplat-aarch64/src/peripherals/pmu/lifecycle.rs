// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PMU-owned global retirement; KIRQ owns neither counter stop nor CPU calls.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use kerrno::{KError, KResult};
use kirq::NmiShutdown;

struct Closing {
    claim: Option<NmiShutdown>,
    pending: [bool; kbuild_config::NR_CPUS],
}

enum State {
    Registered,
    Closing(Closing),
    Stopped,
}

static OWNER: klazy::Lazy<kmutex::Mutex<State>> =
    klazy::Lazy::new(|| kmutex::Mutex::new(State::Registered));

// Local start checks this under its whole-transaction CPU/IRQ guard. An older
// start must finish before the same CPU can execute the synchronous stop call.
// Release/acquire publishes the gate; IPI completion, not this flag, proves stop.
// Visit every CPU allowed to start, even without a preparation record. Its stop
// callback drains any old pinned arm before restore can reopen this gate.
static IS_CLOSED: AtomicBool = AtomicBool::new(false);

pub(crate) fn can_start() -> bool {
    !IS_CLOSED.load(Ordering::Acquire)
}

// Tests model a remote gate transition inside a pinned local setup without
// incorrectly running sleepable global shutdown from that setup's context.
#[cfg(unittest)]
pub(super) fn set_closed_for_test(is_closed: bool) {
    IS_CLOSED.store(is_closed, Ordering::Release);
}

fn validate_context() -> KResult<()> {
    if !kiface_task_api::TaskBlockingContextIf::is_sleepable_context() {
        return Err(KError::InvalidInput);
    }
    if kplat::nm_irq::mode() == kplat::nm_irq::NmiMode::None {
        return Err(KError::Unsupported);
    }
    Ok(())
}

pub(crate) fn shutdown() -> KResult<()> {
    shutdown_with(stop_cpu)
}

fn shutdown_with(mut stop: impl FnMut(usize) -> KResult<()>) -> KResult<()> {
    validate_context()?;
    let mut state = OWNER.lock();
    if matches!(*state, State::Stopped) {
        return Ok(());
    }
    if matches!(*state, State::Registered) {
        IS_CLOSED.store(true, Ordering::Release);
        let desc = kirq::gic_level_irq_desc(of::pmu_irq_or(kbuild_config::PMU_IRQ));
        let claim = match kirq::begin_nmi_shutdown(desc) {
            Ok(claim) => claim,
            Err(_) => {
                IS_CLOSED.store(false, Ordering::Release);
                return Err(KError::BadState);
            }
        };
        // Presence is not readiness: stop_cpu checks the target's IPI service
        // and failure retains Closing. UP must not target dormant firmware CPUs.
        let start_cpus = if cfg!(feature = "smp") {
            kcpu_id_map::nr_cpus()
        } else {
            1
        };
        let pending = core::array::from_fn(|cpu| cpu < start_cpus || claim.is_participant(cpu));
        *state = State::Closing(Closing {
            claim: Some(claim),
            pending,
        });
    }
    let State::Closing(closing) = &mut *state else {
        return Err(KError::BadState);
    };
    for (cpu, pending) in closing.pending.iter_mut().enumerate() {
        if *pending {
            stop(cpu)?;
            *pending = false;
        }
    }
    let claim = closing.claim.take().ok_or(KError::BadState)?;
    if let Err(claim) = claim.finish() {
        closing.claim = Some(claim);
        return Err(KError::ResourceBusy);
    }
    *state = State::Stopped;
    Ok(())
}

fn stop_cpu(cpu: usize) -> KResult<()> {
    let result = Arc::new(AtomicBool::new(false));
    let completed = result.clone();
    // OWNER retains the claim across the entire synchronous operation. The
    // callback never takes OWNER; no sleepable lock is held on the target CPU.
    kipi::run_on_cpu_sync(kcpu_id_map::LogicalCpuId::new(cpu), move || {
        completed.store(super::shutdown_periodic_source(), Ordering::Release);
    })?;
    if result.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err(KError::ResourceBusy)
    }
}

pub(crate) fn restore() -> KResult<()> {
    validate_context()?;
    let mut state = OWNER.lock();
    if !matches!(*state, State::Stopped) {
        return Err(KError::BadState);
    }
    let desc = kirq::gic_level_irq_desc(of::pmu_irq_or(kbuild_config::PMU_IRQ));
    let handler = Arc::new(|_| {
        super::handle_overflows();
        kirq::IrqEvent::HANDLED
    });
    if !kirq::register_nmi(desc, handler) {
        return Err(KError::ResourceBusy);
    }
    *state = State::Registered;
    IS_CLOSED.store(false, Ordering::Release);
    Ok(())
}

unittest::path_mod_test!(tests, "../../../ut-tests/pmu_lifecycle.rs");
