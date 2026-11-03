// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Completed calls on boot-static CPUs; no IRQ-source ownership is implied.

use alloc::sync::Arc;

use kcompletion::Completion;
use kcpu_id_map::LogicalCpuId;
use kerrno::{KError, KResult};
use kiface_task_wait_api::TaskCompletionWaitIf;

use crate::Callback;

/// Runs a callback on the selected CPU and waits until its body has returned.
///
/// Unlike [`crate::run_on_cpu`], successful return observes the callback's
/// writes and capture destruction, not merely acceptance into an IPI queue.
/// It does not wait for the surrounding IPI handler/controller completion or
/// unrelated interrupts. Both local and remote bodies run CPU-pinned with
/// ordinary IRQs masked. Bodies and captured destructors must not sleep,
/// enable IRQs, reenter an IPI queue, or wait for work queued to another CPU.
///
/// Requires sleepable task context, with no lock needed by the callback or its
/// wakeup path. Owner storage is prepared before publication using the KERNEL
/// allocation policy. Callers own source stop/re-enable serialization and the
/// relevant CPU set; present CPUs alone are not evidence of service readiness.
/// The target must have initialized its queue and enabled fixed notifications.
/// This supports boot-static CPUs only: callers must exclude system stop and
/// cannot use this as a CPU-hotplug lease or a hardware-source drain barrier.
///
/// # Errors
/// Returns InvalidInput for invalid context/CPU, BadState for an unprepared
/// target, or an allocation error before publication. On error the callback is
/// not invoked. Once published, waiting cannot be interrupted or time out into
/// a false completion; a stalled target prevents successful return.
pub fn run_on_cpu_sync<F: FnOnce() + Send + 'static>(cpu: LogicalCpuId, body: F) -> KResult<()> {
    let mut body = Some(body);
    let mut result = Err(KError::BadState);
    TaskCompletionWaitIf::with_prepared_wait(&mut |wait| {
        // The provider invokes this body exactly once. Taking ownership also
        // prevents a faulty repeated invocation from publishing work twice.
        let Some(body) = body.take() else { return };
        result = (|| {
            validate_target(cpu)?;
            let completion = Arc::new(Completion::new());
            let done = completion.clone();
            let callback = Callback::new(move || {
                body();
                // FnOnce consumes/drops its captures before returning here.
                done.complete_all();
            })?;
            {
                // CPU selection and local execution form one pinned operation;
                // never carry IRQ/preemption exclusion into the sleeping wait.
                let _pin = kspin::NoPreemptIrqSave::new();
                if cpu == khal::percpu::this_cpu_id() {
                    callback.call();
                } else {
                    crate::enqueue_callback(cpu, callback);
                }
            }
            wait.wait_for_completion(&completion);
            Ok(())
        })();
    })?;
    result
}

fn validate_target(cpu: LogicalCpuId) -> KResult<()> {
    if cpu.as_usize() >= kbuild_config::NR_CPUS || kcpu_id_map::raw_cpu_id(cpu).is_none() {
        return Err(KError::InvalidInput);
    }
    if !crate::is_ipi_queue_ready(cpu) || !crate::notification::is_delivery_enabled(cpu) {
        return Err(KError::BadState);
    }
    Ok(())
}

unittest::path_mod_test!(tests, "../ut-tests/sync_call.rs");
