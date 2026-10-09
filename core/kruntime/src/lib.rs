// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime orchestration for x-kernel: primary/secondary CPU bring-up, subsystem
//! init ordering, and handoff to the system-init entry supplied by the `entry`
//! crate.
//!
//! For architecture, boot flow, and security analysis, see `docs/design.md` and
//! `docs/security.md` in the crate source directory.
//!
//! # Cargo Features
//!
//! - `smp`: SMP bring-up (`rust_main_secondary`, `mp`)
//! - `ipi`: Inter-processor interrupts (`kipi`)
//! - `fs` / `fs9p` / `net` / `vsock` / `display` / `input`: driver and subsystem init
//! - `rtc`: Wall-clock banner at boot
//! - `watchdog` / `watchdog_hardlockup`: Watchdog on primary/secondary CPUs
//! - `pmu`: PMU overflow IRQ handler
//! - `arm-timer-resume-fixup`: repair virtual timer regressions after idle/WFI
//! - `rootfs-secondary-block`: mount the secondary block device as the root FS
//! - `workqueue-bench`: opt-in release workqueue benchmarks before userspace
//!
//! All features are optional and disabled by default.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

/// Runs the explicitly configured workqueue test workload before userspace.
///
/// Call once in sleepable boot context after runtime initialization. With
/// `workqueue-bench` enabled outside unit tests, completion powers off the
/// machine; otherwise this is a no-op.
///
/// # Panics
/// The opt-in runner panics on invalid duration, submission/drain errors,
/// callback count or placement mismatches, and failed task joins.
pub fn run_boot_benchmarks() {
    #[cfg(all(feature = "workqueue-bench", not(unittest)))]
    {
        kworkng::benchmarks::run(kbuild_config::WORKQUEUE_SOAK_SECONDS);
        khal::power::power_off();
    }
}

#[cfg(unittest)]
mod tests_workqueue;

#[macro_use]
extern crate klogger;

#[cfg(all(target_os = "none", not(test)))]
mod lang_items;

#[cfg(feature = "smp")]
mod mp;

mod dma_integration;
mod init_setup;

use boot_info::BootConsoleTransport;
use kcpu_id_map::KCpuMaskExt;
use kernel_boot::PrimaryKernelEntry;
#[cfg(not(feature = "smp"))]
use kernel_boot::SecondaryKernelEntry;
use khal::mem::MemFlags;
use memaddr::{MemoryAddr, PAGE_SIZE_4K, PhysAddr, VirtAddr};

#[cfg(feature = "smp")]
pub use self::mp::rust_main_secondary;

const LOGO: &str = r#"
                 ++
             *  ***
           *******
         *******++
      ********+===
    ******#*+======
   *******#***++===
   *********+**===
  ##*********##==                  ====+
%##****++==+****#                ***+=**
#****++=====++****##            ********
####*+=======+******===-     ***#*##****
 #%#******+**+==++**=======+*+*#%
 %#****+******=====+====+++==**
  #*********##***###********##%
   ***##%#%%%%*****#*###*****#%
   *#%%%#%%%%%****   %%*******#%
   #%%%%%%          %%%##***###%%
    **#%           %%%%#% **#**##
   ###%%         %%%%%   %%%***
 %%%%%%        %%%%%%%  %%%%#*
 %%%%%%                %%%%%%+=
"#;

#[kiface::provide]
impl klogger::LoggerAdapter {
    fn write_str(s: &str) {
        khal::console::write_data(s.as_bytes());
    }

    fn now() -> ktime_types::TimeSpan {
        khal::time::monotonic_time().span_since_origin()
    }

    fn cpu_id() -> Option<usize> {
        #[cfg(feature = "smp")]
        {
            is_init_ok().then(|| khal::percpu::this_cpu_id().as_usize())
        }

        #[cfg(not(feature = "smp"))]
        {
            Some(0)
        }
    }

    fn task_id() -> Option<u64> {
        if is_init_ok() {
            ktask::current_may_uninit().map(|curr| curr.owner_key())
        } else {
            None
        }
    }
}

use core::sync::atomic::{AtomicUsize, Ordering};

// The primary publishes first to release AP service bring-up. Each AP adds its
// completion after local services exist; the final acquire covers them all.
static INITED_CPUS: AtomicUsize = AtomicUsize::new(0);

/// High-level system-init handoff performed after runtime bring-up.
///
/// The provider owns policy-level startup such as runtime feature init and
/// spawning the first user program (PID 1) as a fresh user task. The provider
/// returns once init has been published and activated.
#[kiface::interface]
pub trait SystemInitEntry {
    /// Spawn PID 1 and return once it is runnable.
    fn enter();
}

const MAX_REGION_LOG_SUMMARIES: usize = 64;
const REGION_LOG_SUMMARY_THRESHOLD: usize = 8;

const fn configured_log_level() -> &'static str {
    if kbuild_config::LOG_LEVEL_ERROR {
        "error"
    } else if kbuild_config::LOG_LEVEL_WARN {
        "warn"
    } else if kbuild_config::LOG_LEVEL_INFO {
        "info"
    } else if kbuild_config::LOG_LEVEL_DEBUG {
        "debug"
    } else if kbuild_config::LOG_LEVEL_TRACE {
        "trace"
    } else {
        "off"
    }
}

/// Resolve the boot-time log level: a `loglevel=` kernel command-line
/// parameter overrides the Kconfig default, in the style of Linux's
/// `loglevel=` parameter but using X-Kernel's own level values (`0..=5` in
/// off/error/warn/info/debug/trace order, or a level name). `quiet` and
/// `debug` are accepted as shorthand for `loglevel=2` (warn) and
/// `loglevel=4` (debug) respectively. Invalid `loglevel=` values are
/// reported and ignored, falling back to the Kconfig default.
fn resolve_boot_log_level() -> &'static str {
    if let Some(cmdline) = khal::cmdline() {
        for token in cmdline.split_ascii_whitespace() {
            if let Some(value) = token.strip_prefix("loglevel=")
                && !value.is_empty()
            {
                return if klogger::parse_log_level(value).is_some() {
                    value
                } else {
                    warn!(
                        "ignoring invalid loglevel={value} on the kernel command line; expected a \
                         level 0..=5 or off/error/warn/info/debug/trace"
                    );
                    configured_log_level()
                };
            }
        }
        for token in cmdline.split_ascii_whitespace() {
            match token {
                "quiet" => return "2",
                "debug" => return "4",
                _ => {}
            }
        }
    }
    configured_log_level()
}

#[derive(Debug, Clone, Copy)]
struct RegionLogSummary {
    name: &'static str,
    flags: MemFlags,
    count: usize,
    total_size: usize,
    first_start: usize,
    last_end: usize,
}

fn is_init_ok() -> bool {
    // Wait for all *discovered* CPUs (enumerated from the DT/ACPI at boot),
    // not the compile-time `NR_CPUS` cap. Using the cap would deadlock whenever
    // the platform describes fewer CPUs than `NR_CPUS` (e.g. a smaller QEMU
    // `-smp`), because those slots never increment the counter.
    INITED_CPUS.load(Ordering::Acquire) == kcpu_id_map::nr_cpus()
}

/// Run late subsystem initialization on the PID-less late-init bootstrap thread.
///
/// The thread starts in kernel context, brings up secondary CPUs and late
/// subsystems, then crosses [`SystemInitEntry`], which spawns PID 1 init, before
/// returning. Because the late-init thread holds an `Internal` identity and
/// allocates no root PID itself, init receives the system's first root PID.
fn late_init_main(cpu_id: kcpu_id_map::LogicalCpuId) {
    #[cfg(feature = "smp")]
    {
        // Returns only after every present secondary CPU has registered its
        // run queue, so cross-CPU spawn during the driver/subsystem init
        // below cannot land on an unregistered CPU.
        #[expect(
            clippy::panic,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the platform cannot start \
                      a present secondary CPU (boot_ap error); CPU readiness and task placement \
                      assume every present CPU runs, and late init has no error return or recovery"
        )]
        self::mp::start_secondary_cpus(cpu_id).unwrap_or_else(|err| {
            panic!(
                "failed to start secondary CPUs after boot CPU {} init: {err:?}",
                cpu_id.as_usize()
            )
        });
    }

    #[cfg(all(unittest, feature = "smp"))]
    self::mp::tests_startup_order::queue_early_tasks(cpu_id);

    init_cpu_services();

    // All target CPUs can now receive notifications and run worker tasks.
    // Publish services before exposing any work-producing drivers.
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: 64 is a nonzero fixed workqueue batch \
                  limit"
    )]
    let batch_limit = core::num::NonZeroUsize::new(64).unwrap();
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: system worker services cannot \
                  initialize; the runtime entry has no error return and cannot start drivers \
                  without workers"
    )]
    kworkng::runtime::init(
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Kconfig constrains \
                      WORKQUEUE_WORKERS_PER_POOL to a positive value"
        )]
        core::num::NonZeroUsize::new(kbuild_config::WORKQUEUE_WORKERS_PER_POOL)
            .expect("positive configured worker limit"),
        ktime_types::TimeSpan::from_secs(30),
        batch_limit,
    )
    .expect("system workqueue services must be ready before driver initialization");

    #[cfg(all(unittest, feature = "smp"))]
    self::mp::tests_startup_order::record_service_barrier(cpu_id);

    #[cfg(feature = "fs")]
    fs_boot::init_device_discovery();
    kdriver::init_drivers();
    #[cfg(feature = "char")]
    ktty::tty::try_handoff_console();

    // Probe CPU / SMCCC / VirtIO / jitter sources after drivers are visible.
    // Must run before TEE register_init paths that call fill_random().
    entropy::init();

    // Filesystem implementations and other built-in subsystems own their
    // registration callbacks, matching Linux initcall ownership.
    init_setup::init_cb();

    #[cfg(feature = "display")]
    fbdevice::fb_init();
    #[cfg(feature = "input")]
    inputdev::init_input();

    #[cfg(feature = "fs")]
    {
        fs_boot::prepare_namespace();
        fs_boot::mount_virtual_filesystems();
    }

    #[cfg(all(feature = "fs9p", not(unittest)))]
    fs_boot::mount_host_share("/mnt/hostshare");

    #[cfg(feature = "net")]
    knet::init_network();
    #[cfg(feature = "vsock")]
    knet::init_vsock();

    finish_allocator_init();
    log_memory_regions();

    info!("Primary CPU {} init OK.", cpu_id.as_usize());
    // CPU services are ready before drivers; the network service itself must
    // also exist before the bridge starts.
    #[cfg(feature = "vsock_tipc_bridge")]
    knet::start_vsock_bridge();

    // Spawn PID 1 after all PID-less late initialization has completed.
    // `enter()` returns after the init task has been published and activated.
    SystemInitEntry::enter();

    // The late-init thread has finished its job; `task_entry` will exit it.
}

/// Completes boot CPU services before drivers can publish cross-CPU work.
fn init_cpu_services() {
    #[cfg(feature = "ipi")]
    kipi::init();

    #[cfg(feature = "ipi")]
    kipi::enable_fixed_notifications();

    #[cfg(feature = "watchdog")]
    watchdog::init_primary();

    // Keep the PMU PPI masked until watchdog setup has selected NMI delivery.
    #[cfg(feature = "pmu")]
    khal::pmu::enable_irq();

    // APs cannot complete while the primary is still zero. Release them before
    // drivers, which may themselves wait for work on those CPUs.
    INITED_CPUS.fetch_add(1, Ordering::Release);
    while !is_init_ok() {
        core::hint::spin_loop();
    }

    #[cfg(feature = "smp")]
    kipi::tlb::mark_all_cpus_started();
}

/// Prepare the PID-less late-init bootstrap thread on the boot CPU.
///
/// This task runs all late subsystem initialization, spawns PID 1 init, then
/// exits. It carries an `Internal` identity, so it consumes no root PID number.
/// Secondary run queues are not registered at activation time, so the thread
/// is pinned to the boot CPU via a one-shot CPU mask.
fn prepare_late_init_on_primary(cpu_id: kcpu_id_map::LogicalCpuId) -> ktask::KtaskRef {
    let task = ktask::TaskInner::new_pidless_kthread(
        move || late_init_main(cpu_id),
        "late_init".into(),
        kbuild_config::TASK_STACK_SIZE,
    );
    let task = ktask::prepare_task(task);
    task.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu_id));
    task
}

fn register_boot_console_runtime_region(boot_info: &boot_info::BootInfo) {
    if boot_info.boot_console_transport != BootConsoleTransport::Mmio {
        return;
    }
    if boot_info.boot_console_addr == 0
        || boot_info.boot_console_vaddr == 0
        || boot_info.boot_console_size == 0
    {
        return;
    }

    let paddr = PhysAddr::from_usize(boot_info.boot_console_addr);
    let vaddr = VirtAddr::from_usize(boot_info.boot_console_vaddr);
    assert_eq!(
        paddr.align_offset_4k(),
        vaddr.align_offset_4k(),
        "boot console MMIO VA/PA offset mismatch"
    );

    let start = paddr.align_down_4k();
    let size = (paddr.align_offset_4k() + boot_info.boot_console_size).align_up(PAGE_SIZE_4K);
    let mapped = vaddr.align_down_4k();
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the boot UART runtime mapping \
                  cannot be registered; runtime initialization has no error return or recovery"
    )]
    memspace::register_fixed_device_region(start, size, "boot-uart", mapped)
        .expect("failed to register boot console runtime region");
}

/// The main entry point of the runtime.
///
/// It is called from the bootstrapping code in the specific platform crate (see
/// `kplat::main`).
///
/// `arg` is the unified bootloader handoff payload (`BootInfo*`) for the
/// primary CPU. Secondary cores call [`rust_main_secondary`].
pub fn rust_main(arg: usize) -> ! {
    let boot_info = khal::boot_info(arg);
    let cpu_id = boot_info.cpu_id;

    kernel_boot::bootln!(
        "kruntime primary start cpu={} boot_info={arg:#x}",
        cpu_id.as_usize()
    );
    khal::firmware::init(boot_info);
    khal::percpu::init_primary(cpu_id);
    kcpu::init_trap();
    khal::mem::init(boot_info);
    kernel_boot::bootln!("kruntime memory regions ready");
    init_allocator();
    kernel_boot::bootln!("kruntime allocator ready");
    register_boot_console_runtime_region(boot_info);
    memspace::init_memory_management();
    kernel_boot::bootln!("memory space map ready");
    khal::early_driver_init();

    kprintln!("{}", LOGO);
    #[cfg(feature = "rtc")]
    kprintln!(
        "Boot at {}\n",
        chrono::DateTime::from_timestamp_nanos(
            i64::try_from(ktime::realtime().unix_nanos()).unwrap_or(i64::MAX)
        ),
    );

    klogger::init_klogger();
    klogger::set_log_level(resolve_boot_log_level());
    info!("Logging is enabled.");
    info!(
        "Primary CPU {} started, boot_info = {arg:#x}.",
        cpu_id.as_usize()
    );
    {
        use core::ops::Range;

        // SAFETY: The `_stext` and `_etext` symbols are guaranteed to be valid by the linker.
        unsafe extern "C" {
            safe static _stext: [u8; 0];
            safe static _etext: [u8; 0];
        }

        let ip_range = Range {
            start: _stext.as_ptr() as usize,
            end: _etext.as_ptr() as usize,
        };

        // fp_range must cover both:
        //   - KIMAGE_VADDR stacks (primary CPU task stacks, linked at KIMAGE_VADDR)
        //   - Linear-map stacks (secondary CPU boot stacks, at PA + PAGE_OFFSET)
        // So start from PAGE_OFFSET rather than _edata.
        let fp_range = Range {
            start: kaddr_layout::PAGE_OFFSET,
            end: usize::MAX,
        };

        backtrace::init(ip_range, fp_range);
    }

    info!("Initialize platform devices...");
    khal::final_init(boot_info);

    ktask::init_scheduler();
    // Prepare IRQ services and the late-init task before any newly activated
    // task can preempt this bootstrap frame at a nested guard release.
    let startup_guard = kspin::NoPreempt::new();
    info!("Initialize interrupt handlers...");
    init_interrupt();

    // PID 0 is the scheduler-internal boot task. Activate a single PID-less
    // late-init bootstrap thread (Internal identity, no root PID) that brings
    // up late subsystems and then spawns PID 1 init. Pinning it to the boot CPU
    // keeps it independent of secondary run queues, which are not registered
    // yet.
    let late_init = prepare_late_init_on_primary(cpu_id);
    ktask::activate_task(&late_init);
    // The late-init thread is now owned by the run queue; drop our reference
    // so its task can be reclaimed after exit instead of being pinned by this
    // permanently parked frame.
    drop(late_init);
    info!("Started late-init bootstrap thread.");
    // Late initialization itself may block on I/O, so it runs preemptibly.
    drop(startup_guard);

    // The boot task is PID 0 and remains outside the ordinary PID namespace.
    // Once the late-init thread is runnable it has no further initialization
    // role.
    ktask::future::block_on(core::future::pending::<()>());
    unreachable!("PID 0 boot task must remain parked");
}

#[kiface::provide]
impl PrimaryKernelEntry {
    fn enter(boot_info: usize) -> ! {
        rust_main(boot_info)
    }
}

#[cfg(not(feature = "smp"))]
#[kiface::provide]
impl SecondaryKernelEntry {
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: A uniprocessor build never calls boot_ap, \
                  so no secondary CPU is released into this entry; it has no per-CPU runtime to \
                  join"
    )]
    fn enter(_logical_cpu_id: kcpu_id_map::LogicalCpuId) -> ! {
        panic!("secondary CPU entered a uniprocessor kernel")
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "CG-028-focused-functions: TODO(cg): legacy memory-region logger; split summary \
              collection and printing"
)]
fn log_memory_regions() {
    use heapless::Vec;

    fn log_region(region: &khal::mem::MemoryRegion) {
        let reserved_source = if region.flags.contains(khal::mem::MemFlags::RSVD) {
            khal::mem::reserved::describe_reserved_memory_region(region)
                .map(|reserved| reserved.source)
        } else {
            None
        };
        if let Some(source) = reserved_source {
            info!("  {} [source={}]", region, source);
        } else {
            info!("  {}", region);
        }
    }

    fn log_device_region(region: &memspace::DeviceRegion) {
        if let Some(vaddr) = region.vaddr {
            info!(
                "  [PA:{:#x}, PA:{:#x}) [VA:{:#x}, VA:{:#x}) {}",
                region.paddr.as_usize(),
                region.paddr.as_usize() + region.size,
                vaddr.as_usize(),
                vaddr.as_usize() + region.size,
                region.name,
            );
        } else {
            info!(
                "  [PA:{:#x}, PA:{:#x}) {}",
                region.paddr.as_usize(),
                region.paddr.as_usize() + region.size,
                region.name,
            );
        }
    }

    let mut summaries = Vec::<RegionLogSummary, MAX_REGION_LOG_SUMMARIES>::new();
    for region in khal::mem::memory_regions() {
        let start = region.paddr.as_usize();
        let end = start + region.size;
        if let Some(summary) = summaries.iter_mut().find(|summary| {
            summary.name == region.name && summary.flags.bits() == region.flags.bits()
        }) {
            summary.count += 1;
            summary.total_size += region.size;
            summary.first_start = summary.first_start.min(start);
            summary.last_end = summary.last_end.max(end);
        } else {
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: more than 64 distinct \
                          memory-region groups exhaust early diagnostic storage; boot memory \
                          reporting has no error return or recovery"
            )]
            summaries
                .push(RegionLogSummary {
                    name: region.name,
                    flags: region.flags,
                    count: 1,
                    total_size: region.size,
                    first_start: start,
                    last_end: end,
                })
                .expect("too many region log summaries");
        }
    }

    info!("Found physcial memory regions:");
    for region in khal::mem::memory_regions() {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: The preceding pass inserted a summary \
                      for every group in the unchanged memory-region list"
        )]
        let summary = summaries
            .iter()
            .find(|summary| {
                summary.name == region.name && summary.flags.bits() == region.flags.bits()
            })
            .expect("missing memory region log summary");
        if should_summarize_region(summary) {
            continue;
        }
        log_region(&region);
    }

    for summary in summaries {
        if !should_summarize_region(&summary) {
            continue;
        }
        info!(
            "  {}: {} regions, total {:#x}, span [PA:{:#x}, PA:{:#x}) ({:?})",
            summary.name,
            summary.count,
            summary.total_size,
            summary.first_start,
            summary.last_end,
            summary.flags
        );
    }

    let device_regions = memspace::device_regions().collect::<Vec<_, MAX_REGION_LOG_SUMMARIES>>();
    if !device_regions.is_empty() {
        info!("Found runtime device/iomap regions:");
        for region in device_regions {
            log_device_region(&region);
        }
    }
}

fn should_summarize_region(summary: &RegionLogSummary) -> bool {
    summary.count >= REGION_LOG_SUMMARY_THRESHOLD && summary.name.starts_with("uefi ")
}

fn init_allocator() {
    use khal::mem::{MemFlags, memory_regions, p2v};

    info!("Initialize global memory allocator...");
    info!("  use {} allocator.", kalloc::global_allocator().name());
    let mut free_regions = memory_regions().filter(|r| r.flags.contains(MemFlags::FREE));
    if let Some(region) = free_regions.next() {
        kalloc::global_init(p2v(region.paddr).as_usize(), region.size);
    }
    for region in free_regions {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: a free memory region \
                      cannot be added to the allocator; allocator initialization has no error \
                      return or recovery"
        )]
        kalloc::global_add_memory(p2v(region.paddr).as_usize(), region.size)
            .expect("failed to add free region to allocator");
    }
}

fn finish_allocator_init() {
    let allocator = kalloc::global_allocator();
    info!(
        "Allocator state: heap used={:#x}, heap avail={:#x}, pages used={}, pages avail={}, \
         usages={:?}",
        allocator.used_bytes(),
        allocator.available_bytes(),
        allocator.used_pages(),
        allocator.available_pages(),
        allocator.usages()
    );
}

fn init_interrupt() {
    if !kirq::softirq::init() {
        warn!("softirq hardirq-exit runner was already installed");
    }

    // Timer interrupt handler. All hardware-timer driving — periodic-tick
    // bookkeeping, soft-timer wheel drain, and hardware re-arm — lives in
    // `ktask::on_timer_fire`.
    kirq::register(
        khal::time::interrupt_id(),
        alloc::sync::Arc::new(|_| {
            ktask::on_timer_fire();
            kirq::IrqEvent::HANDLED
        }),
    );

    #[cfg(feature = "ipi")]
    kirq::register(
        kbuild_config::IPI_IRQ,
        alloc::sync::Arc::new(|_| {
            #[cfg(feature = "arm-timer-resume-fixup")]
            timer_driver::arm_generic::handle_ipi_fixup();
            #[cfg(feature = "ipi")]
            kipi::ipi_handler();
            kirq::IrqEvent::HANDLED
        }),
    );

    // PMU overflow dispatch.  Perf counters report overflow on the PMU
    // interrupt line; register the dispatch handler under the `pmu` feature
    // so it exists independently of the watchdog/hardlockup NMI path.  The
    // platform owns the IRQ wiring; kruntime only triggers it.
    #[cfg(feature = "pmu")]
    khal::pmu::register_overflow_dispatch();

    ktask::init_softirqd_current_cpu();
    // Enable IRQs before starting app
    karch::enable_local_irq();
}
