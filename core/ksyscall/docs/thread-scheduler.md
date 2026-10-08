# Scheduler Thread Targets

The sched_getscheduler and sched_getparam adapters use a read-only target
helper. Zero selects the calling thread; positive identifiers select a live
published thread by TID, including a non-leader worker. Negative identifiers
return EINVAL; absent or unpublished threads return ESRCH. The lookup returns
a strong task reference, retaining the target while its scheduling state is read.
A positive getpid() value denotes the leader TID, including for a worker caller;
it does not select that worker or fall back to a representative member when the
leader is unpublished. This follows the per-thread Linux query contract:
https://man7.org/linux/man-pages/man2/sched_setscheduler.2.html and
https://man7.org/linux/man-pages/man2/sched_setparam.2.html .

This corrects worker-thread queries used by pthread_getschedparam; process-only
lookup incorrectly returned ESRCH for a live non-leader thread.

The sched_setscheduler and sched_setparam adapters retain the baseline
target_task lookup and policy/priority validation. Zero selects the caller.
A positive identifier equal to the current caller's process PID also selects
the caller, including a worker caller. Other positive identifiers resolve a
live process and its representative published task. A worker TID with no
process of that PID remains ESRCH; negative identifiers remain ESRCH. This
query fix adds no writable target and no owner/capability/RT permission policy.
The existing setter authorization limitations require a separate review.
Their process-PID interpretation and negative-ID ESRCH are known baseline
deviations, not an intended Linux-wide equivalence with the getters. Existing
negative-ID and worker-setter tests preserve that difference explicitly; this
patch neither broadens writable targets nor silently changes setter errno.

The new guest unittest definitions call the real adapters with mapped user
SchedParam buffers. They cover queries from a running non-leader worker for
zero and its own TID, queries of a published inactive worker, unchanged
setter rejection and metadata for that worker TID, negative/missing errors,
and the baseline setter behavior for zero and the worker caller's process PID.
The inactive publication fixture aborts its own handle; running workers perform
thread exit and are joined. All prior test definitions remain present.

These are source definitions and have not been compiled or executed on this
candidate. The existing pthread C probe additionally covers pthread_getschedparam;
its presence or historical results do not validate this source. Policy/priority
queries observe metadata and do not prove real RT/runqueue policy execution.
