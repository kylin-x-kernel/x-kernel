// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::string::String;
use core::mem;

use khal::paging::MappingFlags;
use kspin::{SpinNoIrq, SpinRaw};
use ktask::current;
use ktracepoint::{
    KernelTraceOps, TraceCmdLineCache, TraceEntryParser, TracePipeOps, TracePipeRaw,
    TracingEventsManager, global_init_events,
};
use ktypes::Once;
use memaddr::VirtAddr;

static TRACE_RAW_PIPE: SpinNoIrq<TracePipeRaw> = SpinNoIrq::new(TracePipeRaw::new(4096));

static TRACE_CMDLINE_CACHE: SpinNoIrq<TraceCmdLineCache> =
    SpinNoIrq::new(TraceCmdLineCache::new(1024));

static TRACE_MANAGER: Once<TracingEventsManager<TraceRawLock, Kops>> = Once::new();

/// Lock adapter used by `ktracepoint` internals.
pub struct TraceRawLock(SpinRaw<()>);

unsafe impl lock_api::RawMutex for TraceRawLock {
    type GuardMarker = lock_api::GuardSend;

    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = Self(SpinRaw::new(()));

    fn lock(&self) {
        let guard = self.0.lock();
        mem::forget(guard);
    }

    fn try_lock(&self) -> bool {
        if let Some(guard) = self.0.try_lock() {
            mem::forget(guard);
            true
        } else {
            false
        }
    }

    unsafe fn unlock(&self) {
        unsafe { self.0.force_unlock() }
    }

    fn is_locked(&self) -> bool {
        self.0.is_locked()
    }
}

/// Initialize global tracepoint manager once.
pub fn trace_point_manager() -> &'static TracingEventsManager<TraceRawLock, Kops> {
    TRACE_MANAGER.call_once(|| {
        static_keys::global_init();
        let manager =
            global_init_events::<TraceRawLock, Kops>().expect("failed to initialize trace events");
        manager
    })
}

/// Dump current trace pipe records in parsed text format.
pub fn dump_trace_records() -> String {
    let manager = trace_point_manager();
    let map = manager.tracepoint_map();
    let cmdline = TRACE_CMDLINE_CACHE.lock();
    let mut snapshot = TRACE_RAW_PIPE.lock().snapshot();
    let mut out = String::new();
    out.push_str(&snapshot.default_fmt_str());
    while let Some(event) = snapshot.peek() {
        out.push_str(&TraceEntryParser::parse::<Kops, _>(&map, &cmdline, event));
        snapshot.pop();
    }
    out
}

pub struct Kops;

impl KernelTraceOps for Kops {
    fn time_now() -> u64 {
        khal::time::monotonic_time_nanos()
    }

    fn cpu_id() -> u32 {
        khal::percpu::this_cpu_id() as u32
    }

    fn current_pid() -> u32 {
        current().id().as_u64() as u32
    }

    fn trace_pipe_push_raw_record(buf: &[u8]) {
        TRACE_RAW_PIPE.lock().push_event(buf.to_vec());
    }

    fn trace_cmdline_push(pid: u32) {
        let comm = current().name();
        TRACE_CMDLINE_CACHE.lock().insert(pid, comm);
    }

    fn write_kernel_text(_addr: *mut core::ffi::c_void, _data: &[u8]) {
        const PAGE_SIZE: usize = 4096;
        let addr = _addr as usize;
        let start = addr & !(PAGE_SIZE - 1);
        let end = (addr + _data.len() + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let span = end.saturating_sub(start);
        if span == 0 {
            return;
        }

        let start_va = VirtAddr::from(start);
        let old_flags = {
            let layout = memspace::kernel_layout().lock();
            let Some(area) = layout.find_area(start_va) else {
                return;
            };
            area.flags()
        };

        // static-keys patches instructions in kernel text, which is normally RX.
        // Temporarily add WRITE permission for the touched page range.
        {
            let mut layout = memspace::kernel_layout().lock();
            if layout
                .protect(start_va, span, old_flags | MappingFlags::WRITE)
                .is_err()
            {
                return;
            }
        }

        unsafe {
            core::ptr::copy_nonoverlapping(_data.as_ptr(), _addr.cast::<u8>(), _data.len());
        }

        {
            let mut layout = memspace::kernel_layout().lock();
            let _ = layout.protect(start_va, span, old_flags);
        }

        #[cfg(target_arch = "aarch64")]
        karch::flush_icache_all();
    }
}
