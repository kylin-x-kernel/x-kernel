# Scheduler trace observations

`sched_switch` records outgoing/incoming task IDs, the outgoing `TaskState`
(1 running, 2 ready, 3 blocked, 4 exited), and the event's monotonic `ts_ns`.
`sched_wakeup` records the newly ready task and its monotonic `ts_ns`.
Consumers use the embedded timestamp: the third-party text formatter's outer
CPU and timestamp describe the reader. A global stream cannot currently be
split into per-CPU streams using those outer fields.

The trace ring retains 4096 events in a pre-reserved bounded
`VecDeque<TraceRecord>`. Every record embeds 64 raw bytes (`MAX_RECORD_BYTES`),
its actual length, the capturing task's numeric ID and a 16-byte name prefix.
The current wakeup and switch records occupy 24 and 40 bytes respectively,
including the unchanged 8-byte upstream header. Records are 8-byte aligned so
both the upstream header and u64 payload parser see aligned storage.
Initialization reserves the entire ring before installing scheduler hooks.
Capture and oldest-record eviction copy inline data and neither allocate nor
free memory. Insertion/eviction is O(1); snapshot output uses linear iteration,
avoiding repeated front removal in the measurement path. `/proc/tracing/trace` captures a stable,
read-only snapshot per open; `/proc/tracing/snapshot_abi` advertises version 1.
Continuous single-CPU observers verify overlapping snapshots and switch-chain
continuity. The formatted entries-written header is not a cumulative loss
counter. Lack of overlap means the observer cannot claim complete coverage.

`TaskInner::copy_name` copies the longest UTF-8 prefix fitting the destination
under the task-name lock, without allocating or changing the remaining bytes.
Capture supplies a zero-filled 16-byte destination and releases that lock before
taking the ring lock. Each record therefore retains its capture-time name even
after renaming or task destruction. `trace_cmdline_push` is a no-op: there is no
global allocating command cache in the capture path.

The reader allocates a full-capacity snapshot vector before acquiring the ring
lock, then copies inline records under that lock. This bounded batch copy still
masks IRQs, but does not allocate there. Parsing and string formatting run after
unlocking. A reader-local one-entry `TraceCmdLineCache` receives each record's
name immediately before parsing it; this avoids both stale same-ID names and
upstream byte truncation splitting UTF-8. Reader allocations remain intentional.

Oversized records are rejected whole, without evicting a valid record or copying
a truncated payload. `trace_capture_stats()` exposes saturating counters for
oversized rejection, pre-initialization rejection and oldest-record eviction,
plus the retained count. Normal hooks cannot run before reservation, but a direct
early raw-record call is counted and rejected rather than growing the ring.
Counters do not change the snapshot ABI or its retained-count header.
`/proc/tracing/capture_stats` exposes all four counters as a read-only per-open
text snapshot (`field: value` lines). Formatting occurs after releasing the ring
lock. Reopen to refresh; it is not atomic with a separately opened trace snapshot.
This makes overflow and oversize loss observable without logging recursively in
the scheduler/IRQ capture path. New event definitions must fit the inline limit
or revise that limit and its parser/alignment tests before use.

Trace output preserves the upstream snapshot header, columns and flags legend.
Both entries-in-buffer and entries-written are retained record counts, not
cumulative drop counters. Leave-decision states are passed through switch
callbacks so a wake-before-switch cannot overwrite a Blocked reason.

## Locked trace-pipe dependency contract

The workspace requirement is `ktracepoint = "0.5"`; Cargo.lock selects 0.5.5.
The 0.5 requirement excludes 0.6. In the locked 0.5.5 implementation,
`TracePipeSnapshot::new(event_buf: Vec<Vec<u8>>) -> Self` accepts owned byte
records and `TraceEntryParser::parse<K, L>(..., entry: &[u8]) -> String`
accepts one byte slice. The ring supplies each aligned inline byte slice directly
to the parser; the empty upstream snapshot is used only to generate the header.
`TracePipeRecord` APIs from 0.6 are not the dependency contract of this change.
The header regression test compares against the same locked upstream formatter;
normal Make/Kconfig build and clippy also compile the actual record calls.

The audited default macro path constructs `Entry`/`FullEntry` and callback
arguments on the stack, borrows callback maps under their locks, and passes a
borrowed byte slice to `Kops`. With no filter installed,
`TracePoint::get_compiled_expr()` clones `None`, without allocating. The two
scheduler events have fixed integer fields; their formatting functions run only
on the reader path. Event enablement and manager/static-key setup are control
operations and may allocate before capture begins.

Upstream 0.5.5 clones `Compiled` when a filter is installed. Its expression owns
Strings and Boxes, so that path allocates and frees per event; evaluator error
construction can also allocate. The repository exposes no trace filter setter,
and this change does not add one. The allocation-free guarantee covers the
default unfiltered scheduler capture, not externally installed filters or custom
tracepoint callbacks. Such extensions must meet the scheduler callback contract
before installation; no callbacks are deferred or suppressed by this change.

Tests cover capacity and allocation-address stability across eviction, rejection
without eviction, counter saturation, UTF-8 name boundaries, both event payload
formats and per-record renames. A serial test enables scheduler events, parks a
real `CompletionWait` task, wakes it from a timer IRQ, verifies wakeup/switch
records in the text snapshot, and restores both prior event enable states.

## Multiple-open resource tradeoff

Each independently opened trace description freezes its own generation; N
readable opens therefore retain N buffers and perform N formatting passes.
The 4096-record ring bounds each pass only. This PR accepts linear retention
until close rather than silently regenerating on read, which would invalidate
the snapshot ABI. It does not introduce a per-inode aggregate quota, change
the default simple-file permissions, or treat descriptor limits as a byte
budget. Sharing an existing snapshot through clone keeps one allocation.
