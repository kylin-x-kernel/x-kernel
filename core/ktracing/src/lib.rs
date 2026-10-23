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
const MAX_RECORD_BYTES: usize = 64;
const TASK_NAME_BYTES: usize = 16;

const _: () = assert!(mem::size_of::<ktracepoint::TraceEntry>() == 8);
const _: () = assert!(MAX_RECORD_BYTES >= 8 + 4 * mem::size_of::<u64>());

#[derive(Clone, Copy)]
#[repr(C, align(8))]
struct TraceRecord {
    bytes: [u8; MAX_RECORD_BYTES],
    length: usize,
    pid: u32,
    name: [u8; TASK_NAME_BYTES],
}

/// Capture counters; the snapshot text header continues to report retained entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TraceCaptureStats {
    /// Records currently retained, in capture order.
    pub retained_records: usize,
    /// Oldest records evicted when the bounded ring was full.
    pub overwritten_records: usize,
    /// Records rejected because their bytes exceeded the fixed inline capacity.
    pub oversized_records: usize,
    /// Records rejected before initialization reserved the ring storage.
    pub uninitialized_records: usize,
}

impl core::fmt::Display for TraceCaptureStats {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        writeln!(formatter, "retained_records: {}", self.retained_records)?;
        writeln!(
            formatter,
            "overwritten_records: {}",
            self.overwritten_records
        )?;
        writeln!(formatter, "oversized_records: {}", self.oversized_records)?;
        writeln!(
            formatter,
            "uninitialized_records: {}",
            self.uninitialized_records
        )
    }
}

struct TraceRing {
    records: VecDeque<TraceRecord>,
    overwritten_records: usize,
    oversized_records: usize,
    uninitialized_records: usize,
}

impl TraceRing {
    const fn new() -> Self {
        Self {
            records: VecDeque::new(),
            overwritten_records: 0,
            oversized_records: 0,
            uninitialized_records: 0,
        }
    }

    fn reserve(&mut self) {
        self.records.reserve_exact(TRACE_RECORD_CAPACITY);
    }

    fn push(&mut self, bytes: &[u8], pid: u32, name: [u8; TASK_NAME_BYTES]) {
        if bytes.len() > MAX_RECORD_BYTES {
            self.oversized_records = self.oversized_records.saturating_add(1);
            return;
        }
        if self.records.capacity() < TRACE_RECORD_CAPACITY {
            self.uninitialized_records = self.uninitialized_records.saturating_add(1);
            return;
        }
        let mut record = TraceRecord {
            bytes: [0; MAX_RECORD_BYTES],
            length: bytes.len(),
            pid,
            name,
        };
        record.bytes[..bytes.len()].copy_from_slice(bytes);
        if self.records.len() == TRACE_RECORD_CAPACITY {
            self.records.pop_front();
            self.overwritten_records = self.overwritten_records.saturating_add(1);
        }
        self.records.push_back(record);
    }

    fn stats(&self) -> TraceCaptureStats {
        TraceCaptureStats {
            retained_records: self.records.len(),
            overwritten_records: self.overwritten_records,
            oversized_records: self.oversized_records,
            uninitialized_records: self.uninitialized_records,
        }
    }
}

static_lock! {
    static TRACE_RAW_PIPE: SpinNoIrq<TraceRing> = SpinNoIrq::new(TraceRing::new());
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
        let mut ring = TraceRing::new();
        ring.reserve();
        mem::swap(&mut TRACE_RAW_PIPE.lock().records, &mut ring.records);
        drop(ring);
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
    let mut records = Vec::with_capacity(TRACE_RECORD_CAPACITY);
    records.extend(TRACE_RAW_PIPE.lock().records.iter().copied());
    let mut out = trace_header(records.len());
    let mut cmdline = TraceCmdLineCache::new(1);
    for event in records {
        cmdline.insert(event.pid, String::from_utf8_lossy(&event.name).into_owned());
        out.push_str(&TraceEntryParser::parse::<Kops, _>(
            &map,
            &cmdline,
            &event.bytes[..event.length],
        ));
    }
    out
}

/// Returns capture and loss counters without initializing tracing or allocating.
pub fn trace_capture_stats() -> TraceCaptureStats {
    TRACE_RAW_PIPE.lock().stats()
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
        let task = current();
        let mut name = [0; TASK_NAME_BYTES];
        task.copy_name(&mut name);
        TRACE_RAW_PIPE
            .lock()
            .push(buf, task.trace_id() as u32, name);
    }

    fn trace_cmdline_push(_pid: u32) {}

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

#[cfg(feature = "smp")]
unittest::path_mod_test!(ktracing_unittest, "../ut-tests/ktracing_unittest.rs");
