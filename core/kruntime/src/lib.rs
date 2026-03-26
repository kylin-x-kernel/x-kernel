// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! # Cargo Features
//!
//! - `alloc`: Enable global memory allocator.
//! - `paging`: Enable page table manipulation support.
//! - `smp`: Enable SMP (symmetric multiprocessing) support.
//! - `fs`: Enable filesystem support.
//! - `net`: Enable networking support.
//! - `display`: Enable graphics support.
//!
//! All the features are optional and disabled by default.

#![cfg_attr(not(test), no_std)]
#![feature(doc_cfg)]

#[macro_use]
extern crate klogger;

#[cfg(all(target_os = "none", not(test)))]
mod lang_items;

#[cfg(feature = "smp")]
mod mp;

mod init_setup;

use kernel_boot::{PRIMARY_KERNEL_ENTRY, register_boot_init};

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
            is_init_ok().then_some(khal::percpu::this_cpu_id())
        }

        #[cfg(not(feature = "smp"))]
        {
            Some(0)
        }
    }

    fn task_id() -> Option<u64> {
        if is_init_ok() {
            ktask::current_may_uninit().map(|curr| curr.id().as_u64())
        } else {
            None
        }
    }
}

use core::sync::atomic::{AtomicUsize, Ordering};

static INITED_CPUS: AtomicUsize = AtomicUsize::new(0);

fn is_init_ok() -> bool {
    INITED_CPUS.load(Ordering::Acquire) == kbuild_config::CPU_NUM
}

struct DmaPageTableImpl;

#[crate_interface::impl_interface]
impl kdma::DmaPageTableIf for DmaPageTableImpl {
    fn protect(
        vaddr: memaddr::VirtAddr,
        size: usize,
        flags: khal::paging::MappingFlags,
    ) -> kerrno::KResult {
        memspace::kernel_layout().lock().protect(vaddr, size, flags)
    }
}

/// The main entry point of the runtime.
///
/// It is called from the bootstrapping code in the specific platform crate (see
/// [`kplat::main`]).
///
/// `arg` is the unified bootloader handoff payload (`BootInfo*`) for the
/// primary CPU. Secondary cores call [`rust_main_secondary`].
#[register_boot_init(PRIMARY_KERNEL_ENTRY)]
pub fn rust_main(arg: usize) -> ! {
    let boot_info = khal::boot_info(arg);
    let cpu_id = boot_info.cpu_id;

    kaddr_layout::set_kimage_voffset(kaddr_layout::KIMAGE_VADDR - boot_info.kernel_load_paddr);
    khal::percpu::init_primary(cpu_id);
    kcpu::init_trap();
    khal::early_init(boot_info);

    kprintln!("{}", LOGO);
    kprintln!(
        indoc::indoc! {"
            arch = {}
            platform = {}
            target = {}
            build_mode = {}
            log_level = {}
            backtrace = {}
            smp = {}
        "},
        kbuild_config::ARCH,
        kbuild_config::PLATFORM,
        option_env!("K_TARGET").unwrap_or(""),
        option_env!("K_MODE").unwrap_or(""),
        option_env!("K_LOG").unwrap_or(""),
        backtrace::is_enabled(),
        kbuild_config::CPU_NUM,
    );
    #[cfg(feature = "rtc")]
    kprintln!(
        "Boot at {}\n",
        chrono::DateTime::from_timestamp_nanos(khal::time::wall_time_nanos() as _),
    );

    klogger::init_klogger();
    klogger::set_log_level(option_env!("K_LOG").unwrap_or("")); // no effect if set `log-level-*` features
    info!("Logging is enabled.");
    info!("Primary CPU {cpu_id} started, boot_info = {arg:#x}.");

    khal::mem::init();
    info!("Found physcial memory regions:");
    for memory_region in khal::mem::memory_regions() {
        info!(
            "  [{:x?}, {:x?}) {} ({:?})",
            memory_region.paddr,
            memory_region.paddr + memory_region.size,
            memory_region.name,
            memory_region.flags
        );
    }

    #[cfg(feature = "alloc")]
    init_allocator();

    {
        use core::ops::Range;

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

    #[cfg(feature = "paging")]
    memspace::init_memory_management();

    info!("Initialize platform devices...");
    khal::final_init(boot_info);

    ktask::init_scheduler();

    #[cfg(any(feature = "fs", feature = "net", feature = "display"))]
    {
        #[allow(unused_variables)]
        let all_devices = kdriver::init_drivers();

        #[cfg(feature = "fs")]
        kfs::init_filesystems(all_devices.block);

        #[cfg(feature = "net")]
        knet::init_network(all_devices.net);
        #[cfg(feature = "vsock")]
        knet::init_vsock(all_devices.vsock);

        #[cfg(feature = "display")]
        fbdevice::fb_init(all_devices.display);

        #[cfg(feature = "input")]
        inputdev::init_input(all_devices.input);
    }

    #[cfg(feature = "smp")]
    {
        self::mp::start_secondary_cpus(cpu_id);
    }

    info!("Initialize interrupt handlers...");
    init_interrupt();

    #[cfg(feature = "watchdog")]
    watchdog::init_primary();

    init_setup::init_cb();

    info!("Primary CPU {cpu_id} init OK.");
    INITED_CPUS.fetch_add(1, Ordering::Release);

    while !is_init_ok() {
        core::hint::spin_loop();
    }

    unsafe { main() };

    ktask::exit(0);
}

#[cfg(feature = "alloc")]
fn init_allocator() {
    use khal::mem::{MemFlags, memory_regions, p2v, v2p};

    info!("Initialize global memory allocator...");
    info!("  use {} allocator.", kalloc::global_allocator().name());

    let free_regions = || memory_regions().filter(|r| r.flags.contains(MemFlags::FREE));

    unsafe extern "C" {
        safe static _ekernel: [u8; 0];
    }
    let kernel_end_paddr = v2p(_ekernel.as_ptr().addr().into());

    let init_region = free_regions()
        // First try to find a free memory region after the kernel image
        .find(|r| r.paddr >= kernel_end_paddr)
        // Otherwise just use the largest free memory region
        .or_else(|| free_regions().max_by_key(|r| r.size))
        .expect("no free memory region found!!");

    kalloc::global_init(p2v(init_region.paddr).as_usize(), init_region.size);

    for r in free_regions() {
        if r.paddr != init_region.paddr {
            kalloc::global_add_memory(p2v(r.paddr).as_usize(), r.size)
                .expect("add heap memory region failed");
        }
    }

    let dma_regions = || memory_regions().filter(|r| r.flags.contains(MemFlags::UNCACHED));
    for r in dma_regions() {
        kalloc::global_init_dma_page_allocator(p2v(r.paddr).as_usize(), r.size);
    }
}

fn init_interrupt() {
    // Setup timer interrupt handler
    const PERIODIC_INTERVAL_NANOS: u64 =
        khal::time::NANOS_PER_SEC / kbuild_config::TICKS_PER_SECOND as u64;

    #[percpu::def_percpu]
    static NEXT_DEADLINE: u64 = 0;

    fn update_timer() {
        let now_ns = khal::time::monotonic_time_nanos();
        // Safety: we have disabled preemption in IRQ handler.
        let current_deadline = unsafe { NEXT_DEADLINE.read_current_raw() };

        // Use the later of the existing deadline or "now + interval" as
        // the timer deadline, and record the following tick accordingly.
        let deadline = if now_ns < current_deadline {
            current_deadline
        } else {
            now_ns + PERIODIC_INTERVAL_NANOS
        };

        let next_deadline = deadline + PERIODIC_INTERVAL_NANOS;
        unsafe { NEXT_DEADLINE.write_current_raw(next_deadline) };
        khal::time::arm_timer(deadline);
    }

    khal::irq::register(khal::time::interrupt_id(), || {
        update_timer();
        ktask::on_timer_tick();
    });

    #[cfg(feature = "ipi")]
    khal::irq::register(khal::irq::IPI_IRQ, || {
        kipi::ipi_handler();
    });

    #[cfg(feature = "pmu")]
    khal::irq::register(kbuild_config::PMU_IRQ, || {
        debug!(
            "PMU interrupt received on cpu {}",
            khal::percpu::this_cpu_id()
        );
        khal::pmu::dispatch_irq_overflows();
    });

    // Enable IRQs before starting app
    karch::enable_local_irq();
}
