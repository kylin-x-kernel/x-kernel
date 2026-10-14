// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel-lifetime execution services and atomic default-queue readiness publication.

use alloc::{sync::Arc, vec::Vec};
use core::num::NonZeroUsize;

use kcpu_id_map::LogicalCpuId;
use kirq::softirq::{self, SoftirqVec};
use klazy::Once;
use ktime_types::TimeSpan;
use kworkerpool_ng::{
    BottomHalfPrepareError, BottomHalfPriority, BottomHalfServices, NormalRuntimeError,
    NormalServices,
};
use kworkqueue_ng::{
    CpuPolicy, QueueConfig, QueueConfigError,
    service::{self, Queue, QueuePartitionSource},
};

/// Callback execution context selected when creating an independent queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionKind {
    /// Shared worker tasks execute callbacks in sleepable context.
    Normal,
    /// Shared BH services execute non-sleeping callbacks at this priority.
    BottomHalf(BottomHalfPriority),
}

/// Independent queue creation failed before exposing any queue or accepting work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueCreateError {
    /// Queue preparation requires sleepable task context.
    InvalidContext,
    /// Kernel initialization has not published usable execution services.
    ServicesUnavailable,
    /// A configured CPU has no matching execution service.
    CpuUnavailable(LogicalCpuId),
}

/// Prepares an independent queue against the registered service set.
/// The caller drains/detaches this queue before provider teardown. Production
/// services are kernel-lifetime; creation does not grant default-queue authority.
pub(crate) fn prepare_queue(
    name: &str,
    config: QueueConfig,
    kind: ExecutionKind,
) -> Result<Arc<Queue>, QueueCreateError> {
    if !ktask::is_sleepable_context() {
        return Err(QueueCreateError::InvalidContext);
    }
    with_services(|services| {
        let services = services.ok_or(QueueCreateError::ServicesUnavailable)?;
        // Validate complete coverage before constructing any source. CPU membership
        // is immutable, so the subsequent lookup cannot lose a service.
        for &cpu in config.cpus() {
            if services.binding(cpu, kind).is_none() {
                return Err(QueueCreateError::CpuUnavailable(cpu));
            }
        }
        let binding = |cpu| {
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: The complete immutable CPU \
                          service coverage was validated before binding the queue"
            )]
            services
                .binding(cpu, kind)
                .expect("validated service coverage")
        };
        #[cfg(feature = "stall-check")]
        let queue =
            service::bind_queue_with_diagnostics(config, name, &services.diagnostics, binding);
        #[cfg(not(feature = "stall-check"))]
        let queue = {
            let _ = name;
            service::bind_queue(config, binding)
        };
        Ok(queue)
    })
}

struct Services {
    defaults: [Arc<Queue>; 3],
    #[cfg(feature = "stall-check")]
    diagnostics: kworkqueue_ng::service::Directory,
    normal: NormalServices<QueuePartitionSource>,
    bottom_half: BottomHalfServices<QueuePartitionSource>,
}

impl Services {
    fn binding(
        &self,
        cpu: LogicalCpuId,
        kind: ExecutionKind,
    ) -> Option<(
        Arc<kworkerpool_ng::PartitionDispatch<QueuePartitionSource>>,
        Arc<core::task::Waker>,
    )> {
        match kind {
            ExecutionKind::Normal => self.normal.binding(cpu),
            ExecutionKind::BottomHalf(priority) => self
                .bottom_half
                .service(cpu, priority)
                .map(|service| (service.dispatch(), service.notification_waker())),
        }
    }

    fn prepare(
        limit: NonZeroUsize,
        idle_for: TimeSpan,
        budget: NonZeroUsize,
    ) -> Result<Self, InitError> {
        Self::prepare_with_limits(
            limit,
            idle_for,
            budget,
            kbuild_config::KWORKNG_NORMAL_MAX_ACTIVE,
            kbuild_config::KWORKNG_BH_MAX_ACTIVE,
        )
    }

    fn prepare_with_limits(
        limit: NonZeroUsize,
        idle_for: TimeSpan,
        budget: NonZeroUsize,
        normal_max_active: usize,
        bh_max_active: usize,
    ) -> Result<Self, InitError> {
        if !ktask::is_sleepable_context() {
            return Err(InitError::BottomHalf(
                BottomHalfPrepareError::InvalidContext,
            ));
        }
        let mut cpus = Vec::new();
        kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
        let config = |max_active| {
            QueueConfig::new(&cpus, CpuPolicy::CurrentCpu, max_active)
                .map_err(InitError::Configuration)
        };
        // Validate every configuration before creating any worker task.
        let configs = [
            config(normal_max_active)?,
            config(bh_max_active)?,
            config(bh_max_active)?,
        ];
        let bottom_half = BottomHalfServices::prepare(budget).map_err(InitError::BottomHalf)?;
        let normal = NormalServices::start(limit, idle_for).map_err(InitError::Normal)?;
        #[cfg(feature = "stall-check")]
        let diagnostics = kworkqueue_ng::service::Directory::new();
        let create = |config, name, kind| {
            let binding = |cpu| match kind {
                ExecutionKind::Normal =>
                {
                    #[expect(
                        clippy::expect_used,
                        reason = "CG-021-no-panic-on-reachable-failure: Successful normal and \
                                  bottom-half startup covers every CPU in the same boot CPU \
                                  configuration"
                    )]
                    normal.binding(cpu).expect("normal boot CPU coverage")
                }
                ExecutionKind::BottomHalf(priority) => {
                    #[expect(
                        clippy::expect_used,
                        reason = "CG-021-no-panic-on-reachable-failure: Successful normal and \
                                  bottom-half startup covers every CPU in the same boot CPU \
                                  configuration"
                    )]
                    let service = bottom_half
                        .service(cpu, priority)
                        .expect("BH boot CPU coverage");
                    (service.dispatch(), service.notification_waker())
                }
            };
            #[cfg(feature = "stall-check")]
            let queue = service::bind_queue_with_diagnostics(config, name, &diagnostics, binding);
            #[cfg(not(feature = "stall-check"))]
            let queue = {
                let _ = name;
                service::bind_queue(config, binding)
            };
            queue
        };
        let [normal_config, bh_config, high_config] = configs;
        let defaults = [
            create(normal_config, "system", ExecutionKind::Normal),
            create(
                bh_config,
                "system-bh",
                ExecutionKind::BottomHalf(BottomHalfPriority::Normal),
            ),
            create(
                high_config,
                "system-bh-high",
                ExecutionKind::BottomHalf(BottomHalfPriority::High),
            ),
        ];
        Ok(Self {
            defaults,
            #[cfg(feature = "stall-check")]
            diagnostics,
            normal,
            bottom_half,
        })
    }
}

#[cfg(feature = "stall-check")]
#[kiface::provide]
impl watchdog::WorkqueueStallCheck {
    fn inspect(cpu: LogicalCpuId, now: ktime_types::MonotonicInstant, threshold: TimeSpan) {
        inspect_stalls(cpu, now, threshold, |name, cpu, elapsed| {
            log::warn!(
                "[kworkng] queue '{}' cpu {} backlog stalled for {} ms",
                name,
                cpu.as_usize(),
                elapsed.as_millis(),
            );
        });
    }
}

#[cfg(feature = "stall-check")]
fn inspect_stalls(
    cpu: LogicalCpuId,
    now: ktime_types::MonotonicInstant,
    threshold: TimeSpan,
    report: impl FnMut(&str, LogicalCpuId, TimeSpan),
) -> bool {
    with_services(|services| {
        services.is_some_and(|services| services.diagnostics.inspect(cpu, now, threshold, report))
    })
}

static SERVICES: Once<Services> = Once::new();

// The closure cannot leak a borrow of a replaceable test fixture. Production
// uses the permanent Once directly, with no lock or reference-count operation.
fn with_registry<R>(read: impl FnOnce(&Once<Services>) -> R) -> R {
    #[cfg(any(not(unittest), feature = "system"))]
    {
        read(&SERVICES)
    }
    #[cfg(all(unittest, not(feature = "system")))]
    {
        let registry = services_unittest::registry();
        read(registry.as_deref().unwrap_or(&SERVICES))
    }
}

fn with_services<R>(read: impl FnOnce(Option<&Services>) -> R) -> R {
    with_registry(|registry| read(registry.get()))
}

/// Service initialization failed without replacing an existing installation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitError {
    /// A default queue's CPU set or configured release limit is invalid.
    Configuration(QueueConfigError),
    /// BH preparation rejected the execution context or CPU configuration.
    BottomHalf(BottomHalfPrepareError),
    /// Threaded service preparation failed, including an invalid idle interval.
    Normal(NormalRuntimeError),
    /// Another initialization already published the kernel-lifetime services.
    AlreadyInitialized,
    /// At least one required BH action belongs to another provider.
    ActionsOccupied,
}

/// Starts normal services and installs both BH entries for every boot CPU.
///
/// Called by kernel startup in sleepable task context after the boot CPU map,
/// scheduler, timer and softirq providers are initialized on every present CPU,
/// and before starting work-producing drivers. Do not install legacy Tasklet or
/// High softirq actions when selecting this provider. CPU hotplug is unsupported.
///
/// `limit` bounds worker tasks per CPU. `idle_for` is the positive worker idle
/// retirement interval. `budget` bounds BH source inspections per dispatch pass,
/// not callback time. Default queue release limits come from Kconfig and are
/// independent of these execution-service parameters.
///
/// Success publishes all defaults together and retains services for kernel life;
/// there is no public unregister. Ordinary users neither initialize services nor
/// receive a queue-wide shutdown capability for defaults.
///
/// # Errors
/// Returns [`InitError`] and emits a warning on invalid preparation, duplicate
/// initialization or occupied BH actions. A failed candidate stops its own
/// prepared workers without changing existing services/actions. Default access
/// stays unready unless an earlier initialization succeeded. Allocation failures
/// follow the kernel allocator policy rather than this error protocol.
///
/// # Panics
/// Panics if boot/provider invariants are violated, including inconsistent CPU
/// coverage or inability to join unpublished workers during rollback. Callers
/// must finish the platform prerequisites above before calling this function.
pub fn init(
    limit: NonZeroUsize,
    idle_for: TimeSpan,
    budget: NonZeroUsize,
) -> Result<(), InitError> {
    let result = Services::prepare(limit, idle_for, budget).and_then(install);
    if let Err(error) = result {
        log::warn!("[kworkng] service initialization failed: {error:?}");
    }
    result
}

pub(crate) fn with_default_queue<R>(
    kind: crate::DefaultQueue,
    read: impl FnOnce(Option<&Arc<Queue>>) -> R,
) -> R {
    let index = match kind {
        crate::DefaultQueue::Normal => 0,
        crate::DefaultQueue::BottomHalf => 1,
        crate::DefaultQueue::HighBottomHalf => 2,
    };
    with_services(|services| read(services.map(|services| &services.defaults[index])))
}

fn install(services: Services) -> Result<(), InitError> {
    with_registry(|registry| {
        install_with_actions(registry, services, || {
            softirq::open_softirqs(&[
                (SoftirqVec::Tasklet, run_normal),
                (SoftirqVec::High, run_high),
            ])
        })
    })
}

fn install_with_actions(
    registry: &Once<Services>,
    services: Services,
    register: impl FnOnce() -> bool,
) -> Result<(), InitError> {
    let mut candidate = Some(services);
    let result = {
        // Once contenders spin. Keep the short, nonblocking initializer from
        // being preempted by another initializer on this CPU; readers never wait.
        let _preempt = kspin::NoPreempt::new();
        registry.try_call_once(|| {
            if !register() {
                return Err(InitError::ActionsOccupied);
            }
            // Once publishes with Release only after both actions are installed.
            // Until then readers return unready; the private queues accept no work.
            Ok({
                #[expect(
                    clippy::expect_used,
                    reason = "CG-021-no-panic-on-reachable-failure: try_call_once invokes this \
                              closure at most once and candidate starts as Some"
                )]
                candidate.take().expect("initializing candidate")
            })
        })
    };
    let error = match result {
        Ok(_) if candidate.is_none() => return Ok(()),
        Ok(_) => InitError::AlreadyInitialized,
        Err(error) => error,
    };
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: A published candidate returned above; all \
                  remaining outcomes retain the unpublished candidate"
    )]
    let retired = candidate.expect("failed candidate was not published");
    // Once has ended initialization; join tasks and timer cleanup without locks.
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Rollback runs in the same sleepable task \
                  after dropping the preemption guard, with fresh inline completion registrations"
    )]
    retired
        .normal
        .stop()
        .expect("registration rollback remains sleepable");
    drop(retired);
    Err(error)
}

fn run_normal() {
    run(BottomHalfPriority::Normal);
}

fn run_high() {
    run(BottomHalfPriority::High);
}

fn run(priority: BottomHalfPriority) {
    // The kirq softirq runner pins this CPU for the complete action invocation.
    with_services(|services| {
        // A stale pending bit may invoke a newly installed action before publication.
        // Do not wait here: default access is still unready and no work was accepted.
        let Some(services) = services else {
            return;
        };
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Bottom-half registration installs \
                      services for every immutable boot CPU before publishing actions"
        )]
        let service = services
            .bottom_half
            .service(khal::percpu::this_cpu_id(), priority)
            .expect("BH registration covers every boot CPU");
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: The registered softirq action runs on \
                      its pinned owner CPU in bottom-half context"
        )]
        service.run_pass().expect("registered local softirq entry");
    });
}

#[cfg(not(feature = "system"))]
unittest::path_mod_test!(
    pub(crate) mod services_unittest,
    "../ut-tests/services_unittest.rs",
);
