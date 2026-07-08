// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime orchestration for x-kernel: primary/secondary CPU bring-up, subsystem
//! init ordering, and handoff to the application [`main`] symbol (provided by the
//! `entry` crate).
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
//!
//! All features are optional and disabled by default.

#![cfg_attr(not(test), no_std)]

#[macro_use]
extern crate klogger;

#[cfg(all(target_os = "none", not(test)))]
mod lang_items;

#[cfg(feature = "smp")]
mod mp;

mod dma_integration;
mod init_setup;

use boot_info::BootConsoleTransport;
use kernel_boot::{PRIMARY_KERNEL_ENTRY, register_boot_init};
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

// SAFETY: The linker script exports the final application entry symbol as a
// valid `extern "C" fn()` named `main`.
unsafe extern "C" {
    /// Application's entry point.
    fn main();
}

struct LogIfImpl;

#[crate_interface::impl_interface]
impl klogger::LoggerAdapter for LogIfImpl {
    fn write_str(s: &str) {
        khal::console::write_data(s.as_bytes());
    }

    fn now() -> core::time::Duration {
        khal::time::monotonic_time()
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

static INITED_CPUS: AtomicUsize = AtomicUsize::new(0);

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
#[register_boot_init(PRIMARY_KERNEL_ENTRY)]
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
    // Install the OS-agnostic resource provider before any driver (including
    // early device interrupts like the console input line) acquires a resource.
    kdriver::install_resource_provider();
    khal::early_driver_init();

    kprintln!("{}", LOGO);
    #[cfg(feature = "rtc")]
    kprintln!(
        "Boot at {}\n",
        chrono::DateTime::from_timestamp_nanos(khal::time::wall_time_nanos() as _),
    );

    klogger::init_klogger();
    klogger::set_log_level(configured_log_level()); // no effect if set `log-level-*` features
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

    #[cfg(feature = "smp")]
    {
        self::mp::start_secondary_cpus(cpu_id).unwrap_or_else(|err| {
            panic!(
                "failed to start secondary CPUs after boot CPU {} init: {err:?}",
                cpu_id.as_usize()
            )
        });
    }

    kdriver::init_drivers();
    #[cfg(feature = "char")]
    ktty::tty::try_handoff_console();

    #[cfg(feature = "display")]
    fbdevice::fb_init();
    #[cfg(feature = "input")]
    inputdev::init_input();

    #[cfg(feature = "fs")]
    kfs::init_filesystems();

    #[cfg(feature = "fs9p")]
    kfs::mount_9pfilesystems("/mnt/hostshare");

    #[cfg(feature = "net")]
    knet::init_network();
    #[cfg(feature = "vsock")]
    knet::init_vsock();

    #[cfg(feature = "ipi")]
    kipi::init();

    #[cfg(feature = "smp")]
    kipi::tlb::mark_all_cpus_started();

    info!("Initialize interrupt handlers...");
    init_interrupt();

    #[cfg(feature = "watchdog")]
    watchdog::init_primary();

    init_setup::init_cb();
    finish_allocator_init();
    log_memory_regions();

    info!("Primary CPU {} init OK.", cpu_id.as_usize());
    INITED_CPUS.fetch_add(1, Ordering::Release);

    while !is_init_ok() {
        core::hint::spin_loop();
    }

    // SAFETY: The linker exported `main` as the final application entry, and
    // runtime initialization above has established the execution environment it
    // expects before control is transferred.
    unsafe { main() };

    ktask::exit(0);
}

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
    // Setup timer interrupt handler
    const PERIODIC_INTERVAL_NANOS: u64 =
        khal::time::NANOS_PER_SEC / kbuild_config::TICKS_PER_SECOND as u64;
    #[percpu::def_percpu]
    static NEXT_DEADLINE: u64 = 0;

    fn update_timer(now_ns: u64) {
        // SAFETY: timer IRQ handlers run with preemption disabled on the
        // current CPU, so raw per-CPU access cannot race migration to another
        // CPU.
        let current_deadline = unsafe { NEXT_DEADLINE.read_current_raw() };

        // Use the later of the existing deadline or "now + interval" as
        // the timer deadline, and record the following tick accordingly.
        let deadline = if now_ns < current_deadline {
            current_deadline
        } else {
            now_ns + PERIODIC_INTERVAL_NANOS
        };

        let next_deadline = deadline + PERIODIC_INTERVAL_NANOS;
        // SAFETY: timer IRQ handlers run with preemption disabled on the
        // current CPU, so writing the current CPU's raw per-CPU slot is safe.
        unsafe { NEXT_DEADLINE.write_current_raw(next_deadline) };
        khal::time::arm_timer(deadline);
    }

    khal::irq::register(khal::time::interrupt_id(), || {
        let now_ns = khal::time::monotonic_time_nanos();
        update_timer(now_ns);
        ktask::on_timer_tick();
    });

    #[cfg(feature = "ipi")]
    khal::irq::register(kbuild_config::IPI_IRQ, || {
        #[cfg(feature = "arm-timer-resume-fixup")]
        timer_driver::arm_generic::handle_ipi_fixup();
        #[cfg(feature = "ipi")]
        kipi::ipi_handler();
    });

    #[cfg(feature = "pmu")]
    khal::irq::register(kbuild_config::PMU_IRQ, || {
        debug!(
            "PMU interrupt received on cpu {}",
            khal::percpu::this_cpu_id().as_usize()
        );
        khal::pmu::dispatch_irq_overflows();
    });

    // Enable IRQs before starting app
    karch::enable_local_irq();
}
