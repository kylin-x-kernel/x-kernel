// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::{
    collections::VecDeque,
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::mem;

use khal::paging::MappingFlags;
use klazy::Once;
use kspin::{SpinNoIrq, SpinRaw, static_lock};
use ktask::current;
use ktracepoint::{
    KernelTraceOps, TraceCmdLineCache, TraceEntryParser, TracePipeSnapshot, TracingEventsManager,
    global_init_events,
};
use memaddr::{PAGE_SIZE_4K, VirtAddr};

const TRACE_RECORD_CAPACITY: usize = 4096;
// Reserve before registering hooks; retention evicts in O(1), without moving
// thousands of records on every scheduler event.
static_lock! {
    static TRACE_RAW_PIPE: SpinNoIrq<VecDeque<Vec<u8>>> = SpinNoIrq::new(VecDeque::new());
}

static_lock! {
    static TRACE_CMDLINE_CACHE: SpinNoIrq<TraceCmdLineCache> =
        SpinNoIrq::new(TraceCmdLineCache::new(1024));
}

static TRACE_MANAGER: Once<TracingEventsManager<TraceRawLock, Kops>> = Once::new();

/// Lock adapter used by `ktracepoint` internals.
pub struct TraceRawLock(SpinRaw<()>);

// SAFETY: `TraceRawLock` implements the `lock_api::RawMutex` protocol by
// leaking the RAII guard on successful lock acquisition and pairing it with
// `SpinLock::force_unlock` in `unlock`. The tracepoint lock is raw by design:
// callers must use it through `lock_api`, which guarantees `unlock` is called
// only after a successful `lock` or `try_lock`.
unsafe impl lock_api::RawMutex for TraceRawLock {
    type GuardMarker = lock_api::GuardSend;

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
        // SAFETY: required by the `RawMutex::unlock` contract. `lock` and
        // successful `try_lock` above intentionally forget the guard, so this
        // path still owns the raw spin lock and may release its atomic flag.
        unsafe { self.0.force_unlock() }
    }

    fn is_locked(&self) -> bool {
        self.0.is_locked()
    }
}

ktracepoint::define_event_trace!(
    sched_wakeup,
    TP_lock(crate::TraceRawLock),
    TP_kops(crate::Kops),
    TP_system(sched),
    TP_PROTO(woken_tid: u64),
    TP_STRUCT__entry { woken_tid: u64, ts_ns: u64 },
    TP_fast_assign {
        woken_tid: woken_tid,
        ts_ns: khal::time::monotonic_time().as_nanos_u64_saturating(),
    },
    TP_ident(__entry),
    TP_printk(format_args!(
        "woken_tid={} ts_ns={}",
        __entry.woken_tid,
        __entry.ts_ns
    ))
);

ktracepoint::define_event_trace!(
    sched_switch,
    TP_lock(crate::TraceRawLock),
    TP_kops(crate::Kops),
    TP_system(sched),
    TP_PROTO(prev_tid: u64, next_tid: u64, prev_state: u64),
    TP_STRUCT__entry {
        prev_tid: u64,
        next_tid: u64,
        prev_state: u64,
        ts_ns: u64,
    },
    TP_fast_assign {
        prev_tid: prev_tid,
        next_tid: next_tid,
        prev_state: prev_state,
        ts_ns: khal::time::monotonic_time().as_nanos_u64_saturating(),
    },
    TP_ident(__entry),
    TP_printk(format_args!(
        "prev_tid={} next_tid={} prev_state={} ts_ns={}",
        __entry.prev_tid,
        __entry.next_tid,
        __entry.prev_state,
        __entry.ts_ns
    ))
);

/// Initialize global tracepoint manager once.
pub fn trace_point_manager() -> &'static TracingEventsManager<TraceRawLock, Kops> {
    TRACE_MANAGER.call_once(|| {
        TRACE_RAW_PIPE.lock().reserve(TRACE_RECORD_CAPACITY);
        static_keys::global_init();
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: ktracepoint global_init_events \
                      constructs the linker-registered event map and unconditionally returns Ok"
        )]
        let manager =
            global_init_events::<TraceRawLock, Kops>().expect("failed to initialize trace events");
        ktask::register_sched_trace_hooks(trace_sched_wakeup, trace_sched_switch);
        manager
    })
}

/// Dump current trace pipe records in parsed text format.
pub fn dump_trace_records() -> String {
    let manager = trace_point_manager();
    let map = manager.tracepoint_map();
    let records: Vec<_> = TRACE_RAW_PIPE.lock().iter().cloned().collect();
    let mut out = trace_header(records.len());
    // Consuming a Vec by iteration is linear. The third-party snapshot's pop()
    // removes index zero and would otherwise make every read quadratic.
    // Keep each command-cache formatting critical section to one record.
    // The raw-ring snapshot above still clones all records under its own IRQ
    // lock. Hoisting the command-cache lock would also mask interrupts while
    // formatting up to 4096 records.
    // Per-record acquisition deliberately trades lock overhead for IRQ latency.
    // Names are best-effort: concurrent cache updates or eviction can change
    // name resolution between records, including producing missing-name markers.
    // The generated bytes freeze on open; name lookup has no shared dump epoch.
    for event in records {
        let cmdline = TRACE_CMDLINE_CACHE.lock();
        out.push_str(&TraceEntryParser::parse::<Kops, _>(&map, &cmdline, &event));
    }
    out
}

fn trace_header(record_count: usize) -> String {
    // Keep the upstream columns and flags legend without cloning records just
    // for formatting. As upstream does, entries-written is the snapshot count,
    // not a cumulative loss counter.
    TracePipeSnapshot::new(Vec::new())
        .default_fmt_str()
        .replacen(
            "entries-in-buffer/entries-written: 0/0",
            &format!("entries-in-buffer/entries-written: {record_count}/{record_count}"),
            1,
        )
}

fn with_event<R>(
    subsystem: &str,
    event: &str,
    f: impl FnOnce(&ktracepoint::EventInfo<TraceRawLock, Kops>) -> R,
) -> Option<R> {
    let manager = trace_point_manager();
    let subsystem = manager.get_subsystem(subsystem)?;
    let event = subsystem.get_event(event)?;
    Some(f(&event))
}

/// Return all tracing subsystem names.
pub fn subsystem_names() -> Vec<String> {
    trace_point_manager().subsystem_names()
}

/// Return all event names under a subsystem.
pub fn event_names(subsystem: &str) -> Vec<String> {
    trace_point_manager()
        .get_subsystem(subsystem)
        .map(|subsystem| subsystem.event_names())
        .unwrap_or_default()
}

/// Read the textual enable state of an event.
pub fn event_enable_state(subsystem: &str, event: &str) -> Option<String> {
    with_event(subsystem, event, |event| {
        event.enable_file().read().to_string()
    })
}

/// Update an event's enable state using the first byte of the user input.
pub fn write_event_enable(subsystem: &str, event: &str, data: &[u8]) -> bool {
    let Some(enable) = data
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        .map(char::from)
    else {
        // `echo 1 > enable` uses O_TRUNC; VFS may issue an empty `write` while truncating.
        // Treat whitespace-only payloads as a successful no-op so redirection works.
        return true;
    };
    with_event(subsystem, event, |event| {
        event.enable_file().write(enable);
        match enable {
            '1' => event.tracepoint().enable_event(),
            '0' => event.tracepoint().disable_event(),
            _ => {}
        }
    })
    .is_some()
}

/// Read an event's format description.
pub fn event_format(subsystem: &str, event: &str) -> Option<String> {
    with_event(subsystem, event, |event| event.format_file().read())
}

/// Read an event's numeric ID.
pub fn event_id(subsystem: &str, event: &str) -> Option<String> {
    with_event(subsystem, event, |event| event.id_file().read())
}

pub struct Kops;

impl KernelTraceOps for Kops {
    fn time_now() -> u64 {
        khal::time::monotonic_time().as_nanos_u64_saturating()
    }

    fn cpu_id() -> u32 {
        khal::percpu::this_cpu_id().as_usize() as u32
    }

    fn current_pid() -> u32 {
        current().trace_id() as u32
    }

    fn trace_pipe_push_raw_record(buf: &[u8]) {
        let mut records = TRACE_RAW_PIPE.lock();
        if records.len() == TRACE_RECORD_CAPACITY {
            records.pop_front();
        }
        records.push_back(buf.to_vec());
    }

    fn trace_cmdline_push(pid: u32) {
        let comm = current().name();
        TRACE_CMDLINE_CACHE.lock().insert(pid, comm);
    }

    fn write_kernel_text(ptr: *mut core::ffi::c_void, data: &[u8]) {
        let addr = ptr as usize;
        let start = addr & !(PAGE_SIZE_4K - 1);
        let end = (addr + data.len() + PAGE_SIZE_4K - 1) & !(PAGE_SIZE_4K - 1);
        let span = end.saturating_sub(start);
        if span == 0 {
            return;
        }

        let start_va = VirtAddr::from(start);
        let old_flags = {
            let layout = memspace::kernel_layout().lock();
            let Some(vma) = layout.find_vma(start_va) else {
                return;
            };
            vma.flags()
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

        // SAFETY: the destination range is the kernel text range selected by
        // the static-key patching caller. The page range has been temporarily
        // made writable above, and the source/destination ranges do not
        // overlap.
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), ptr.cast::<u8>(), data.len()) };

        {
            let mut layout = memspace::kernel_layout().lock();
            // Patched text cannot be rolled back; restoring protection is best effort.
            let _ = layout.protect(start_va, span, old_flags);
        }

        karch::flush_icache_range(VirtAddr::from(addr), data.len());
    }
}

#[cfg(all(unittest, feature = "smp"))]
#[path = "test_ktracing.rs"]
mod tests;
