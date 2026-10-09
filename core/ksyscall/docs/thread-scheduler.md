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

The sched_setscheduler and sched_setparam adapters likewise resolve published
TIDs, including workers, so pthread_setschedparam can update its thread.
Positive getpid() still denotes the leader TID, never a representative worker.
They retain their existing negative-ID ESRCH behavior; getters return EINVAL.
Policy/parameter writes require the caller effective UID to match the target
real/effective UID or a privileged caller (euid 0, the current CAP_SYS_NICE
approximation). Otherwise they return EPERM and leave metadata unchanged.
This owner gate does not implement RT resource limits or real RT/runqueue execution.

Guest unit tests call the actual adapters with mapped SchedParam buffers.
They cover a running worker addressed by zero and its TID, a published inactive
worker queried and updated by TID, absent/unpublished identifiers, and a worker
caller explicitly targeting the leader TID. Worker changes preserve the leader's
metadata, and leader parameter writes preserve the worker's metadata.
Cross-owner writes through both actual setters are denied without changing
policy or priority. Inactive publication is rolled back by its owned handle;
running workers exit and join.
The main-branch tests which required process-only setter lookup are updated to
this PR's intended thread-target contract instead of retaining contradictory
expectations. Validation results are recorded separately for the actual head.
