# Observation integrity

Capture losses are visible through `/proc/tracing/capture_stats`: retained,
overwritten, oversized and uninitialized counts are snapshotted under the ring
lock, then formatted outside it. The file is read-only and refreshes on reopen.
It cannot establish atomic correspondence with another trace-file snapshot.
No warning is emitted from the scheduler capture path, avoiding logging reentry.

Scheduler trace callbacks run in non-sleepable context, including completion
notification under its internal lock. The 4096-slot ring is fully reserved before
hook publication. Each slot contains fixed bytes and metadata with no owned heap
payload or destructor, so neither insertion nor oldest-record eviction allocates
or frees memory. Task state is copied into the event while the scheduler owns
the switch decision. Capture borrows the current task and copies its name under
the name lock, then releases that lock before taking the ring lock.

Names are truncated at a UTF-8 boundary into zero-filled 16-byte storage. This
also prevents the upstream command-cache parser from panicking on a split code
point. The captured ID and name are observation metadata, not task ownership.
The upstream parser receives 8-byte-aligned records of the original length;
current scheduler event headers and fixed payloads are fully initialized.
The raw-record callback remains a trusted kernel interface, not a decoder for
arbitrary user bytes. New events must preserve parser layout/length invariants.

Records larger than 64 bytes are rejected and counted without disturbing retained
events. Direct capture before ring initialization is also rejected and counted.
Eviction and rejection counters saturate instead of wrapping. The stable snapshot
header still reports retained counts; it is not a cumulative loss report.

Snapshot allocation precedes the IRQ-safe ring lock. The bounded copy remains
under that lock; reader-local name-cache allocation and formatting happen after
unlocking. No formatting or arbitrary deferred callbacks enter the capture path.

The allocation-free claim covers the default unfiltered scheduler hooks. Audited
ktracepoint 0.5.5 still clones an allocating AST for an installed filter; filter
evaluation errors may allocate too. Custom callbacks can also violate the host
contract. No filter-setting endpoint is exposed by this module; kernel callers
must not install allocating, unwinding, blocking or reentrant callbacks/filters
on these scheduler paths. Initialization, configuration, task creation/renaming
and snapshot readers are outside the per-event allocation-free boundary.

Trace readers receive an owned snapshot and cannot mutate the ring through it.
Every concurrently open snapshot retains its own bounded allocation until
close. Fairness consumers must detect overwritten records, verify clock and
single-CPU assumptions, and reject incomplete evidence rather than infer equal
service from a truncated trace.

The 4096-event limit is per snapshot, not a system-wide memory quota. N
independent readable opens retain N generated buffers, and formatting CPU
work scales with those opens. This change accepts that resource cost to
preserve independent per-open generations; it does not add a global or
per-inode snapshot quota or claim resistance to repeated-open exhaustion.
The default simple-file permissions remain unchanged. Process descriptor
limits constrain open handles, not aggregate snapshot bytes; permission and
memory-budget policy are separate from observation consistency. Sharing a
cloned snapshot Arc retains one generation without allocating another buffer.
