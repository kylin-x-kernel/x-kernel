# Observation integrity

Scheduler trace callbacks run in non-sleepable context. The record ring and
command cache retain their existing bounded sizes and locking. Task state is
copied into the event while the scheduler owns the switch decision.

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
