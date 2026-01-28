// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// Copyright (C) 2025 Yuekai Jia <equation618@gmail.com>
// See LICENSE for license details.
//
// This file has been modified by KylinSoft on 2025.

//! Runtime library of [ArceOS](https://github.com/arceos-org/arceos).
//!
//! Any application uses ArceOS should link this library. It does some
//! initialization work before entering the application's `main` function.
//!
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

#[cfg(feature = "smp")]
pub use self::mp::rust_main_secondary;

const LOGO: &str = r#"
       d8888                            .d88888b.   .d8888b.
      d88888                           d88P" "Y88b d88P  Y88b
     d88P888                           888     888 Y88b.
    d88P 888 888d888  .d8888b  .d88b.  888     888  "Y888b.
   d88P  888 888P"   d88P"    d8P  Y8b 888     888     "Y88b.
  d88P   888 888     888      88888888 888     888       "888
 d8888888888 888     Y88b.    Y8b.     Y88b. .d88P Y88b  d88P
d88P     888 888      "Y8888P  "Y8888   "Y88888P"   "Y8888P"
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
        if is_init_ok() {
            Some(khal::percpu::this_cpu_id())
        } else {
            None
        }
        #[cfg(not(feature = "smp"))]
        Some(0)
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
    INITED_CPUS.load(Ordering::Acquire) == platconfig::plat::CPU_NUM
}

/// The main entry point of the ArceOS runtime.
///
/// It is called from the bootstrapping code in the specific platform crate (see
/// [`kplat::main`]).
///
/// `cpu_id` is the logic ID of the current CPU, and `arg` is passed from the
/// bootloader (typically the device tree blob address).
///
/// In multi-core environment, this function is called on the primary core, and
/// secondary cores call [`rust_main_secondary`].
#[cfg_attr(not(test), kplat::main)]
pub fn rust_main(cpu_id: usize, arg: usize) -> ! {
    unsafe { khal::mem::clear_bss() };
    khal::percpu::init_primary(cpu_id);
    khal::early_init(cpu_id, arg);

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
        platconfig::ARCH,
        platconfig::PLATFORM,
        option_env!("AX_TARGET").unwrap_or(""),
        option_env!("AX_MODE").unwrap_or(""),
        option_env!("AX_LOG").unwrap_or(""),
        backtrace::is_enabled(),
        platconfig::plat::CPU_NUM,
    );
    #[cfg(feature = "rtc")]
    kprintln!(
        "Boot at {}\n",
        chrono::DateTime::from_timestamp_nanos(khal::time::wall_time_nanos() as _),
    );

    klogger::init_klogger();
    klogger::set_log_level(option_env!("AX_LOG").unwrap_or("")); // no effect if set `log-level-*` features
    info!("Logging is enabled.");
    info!("Primary CPU {cpu_id} started, arg = {arg:#x}.");

    khal::mem::init();
    info!("Found physcial memory regions:");
    for r in khal::mem::memory_regions() {
        info!(
            "  [{:x?}, {:x?}) {} ({:?})",
            r.paddr,
            r.paddr + r.size,
            r.name,
            r.flags
        );
    }

    #[cfg(feature = "alloc")]
    init_allocator();

    {
        use core::ops::Range;

        unsafe extern "C" {
            safe static _stext: [u8; 0];
            safe static _etext: [u8; 0];
            safe static _edata: [u8; 0];
        }

        let ip_range = Range {
            start: _stext.as_ptr() as usize,
            end: _etext.as_ptr() as usize,
        };

        let fp_range = Range {
            start: _edata.as_ptr() as usize,
            end: usize::MAX,
        };

        backtrace::init(ip_range, fp_range);
    }

    #[cfg(feature = "paging")]
    axmm::init_memory_management();

    info!("Initialize platform devices...");
    khal::final_init(cpu_id, arg);

    ktask::init_scheduler();

    #[cfg(any(feature = "fs", feature = "net", feature = "display"))]
    {
        #[allow(unused_variables)]
        let all_devices = kdriver::init_drivers();

        #[cfg(feature = "fs")]
        axfs::init_filesystems(all_devices.block);

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
    self::mp::start_secondary_cpus(cpu_id);

    info!("Initialize interrupt handlers...");
    init_interrupt();

    #[cfg(feature = "watchdog")]
    axwatchdog::init_primary();

    ctor_bare::call_ctors();

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
}

fn init_interrupt() {
    // Setup timer interrupt handler
    const PERIODIC_INTERVAL_NANOS: u64 =
        khal::time::NANOS_PER_SEC / platconfig::TICKS_PER_SEC as u64;

    #[percpu::def_percpu]
    static NEXT_DEADLINE: u64 = 0;

    fn update_timer() {
        let now_ns = khal::time::monotonic_time_nanos();
        // Safety: we have disabled preemption in IRQ handler.
        let mut deadline = unsafe { NEXT_DEADLINE.read_current_raw() };
        if now_ns >= deadline {
            deadline = now_ns + PERIODIC_INTERVAL_NANOS;
        }
        unsafe { NEXT_DEADLINE.write_current_raw(deadline + PERIODIC_INTERVAL_NANOS) };
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
    khal::irq::register(platconfig::devices::PMU_IRQ, || {
        debug!(
            "PMU interrupt received on cpu {}",
            khal::percpu::this_cpu_id()
        );
        khal::pmu::dispatch_irq_overflows();
    });

    // Enable IRQs before starting app
    khal::asm::enable_local();
}
