# posix-process — Security and reliability

## Physical OOM faults

A user page fault that returns `OutOfMemory` terminates the allocating process
through kernel `SIGKILL`, independently of user signal handlers. There is no
retry loop, address-error misclassification, global victim scan or reclaim.
MM cleanup releases commitments and private pages, but unrelated infallible
kernel allocations can still fail; this policy is not an end-to-end NOFAIL
recovery guarantee. Default overcommit admission and `MAP_NORESERVE` semantics
are owned by `memspace`, not the signal layer.

## Synchronous signal-frame faults

The fault API accepts only kernel-generated fault information in the current
user task. It queues to that thread, or terminates the group when delivery is
blocked/ignored, so malformed sigreturn state cannot be resumed merely because
a sibling accepted a process-directed notification.

## x86_64 FP ownership

Fork/clone snapshots are kernel-owned and captured while the creator is current.
The child restores its own inherited copy before user entry; no user-writable
signal-frame extension is reused for this transfer. Ordinary context switches
preserve all enabled xstate, and exec installs initial FP state so the new image
does not observe the previous program's vector registers.


Fatal trap signals explicitly permit bit-only fallback on owner exhaustion;
this policy is not selected by an untrusted SI_KERNEL code. The call retains
its existing kernel-fatal-delivery role and direct-exit fallback. No allocator
retry, sleeping reclaim or NOFAIL guarantee is introduced.

## Scope, trust and assets

The analysis covers all three source files and their optional TEE/TIPC paths.
Protected assets are the current thread's identity, saved user context, address
space ownership, runtime handles and parent-visible completion. `kprocess`
provides identity/publication and owner-slot synchronization, `memspace` handles
faults/mappings, `kuaccess`/`osvm` perform fault-aware user access, and `kfutex`
resolves futex keys and performs PI exit handoff. This crate orders those operations; it does not validate
executable formats or maintain process registries itself.

## External boundaries and validation

The saved-context entry is prepared with KERNEL before ordinary clone creates
child relationships. NoMemory returns to the syscall caller without adding a
child or publishing a task. Dropping an unused prepared entry releases captures
without running the user loop. Its capture destructors must be valid in the
dropping context. The task builder only transfers this owner; it cannot fail on
a second callback allocation after clone commitment. PID 1 retains an explicit
fatal boot failure policy, not a NOFAIL promise. Other task allocations remain
infallible, so this is not end-to-end OOM recovery.

The thread owner is a `KBox` whose policy is chosen during kprocess preparation:
KERNEL for sleepable initial construction, NOWAIT within the process cgroup
clone transaction. Passing it to the trap-loop task builder adds no allocation
or lifetime extension. Boot handles initial preparation failure explicitly;
normal clone failures propagate without activating the child. Final runtime
destruction continues through the scheduler's existing ownership protocol.

`spawn_init_process(args, envs, ...)` passes path/argv/envp to `ExecRequest` and
`prepare_user_app_request`; filesystem resolution and executable validation belong
to those providers. `load_prepared_user_app` receives only an unpublished empty
MM and owned preparation result. Publication follows successful installation;
missing argv or bootstrap failures panic.
`run_user_thread_loop` receives architecture trap results and the saved user
context. It uses structured MM fault outcomes and signal actions, not raw
user-supplied exception interpretations.
Internal syscall restart codes are resolved according to handler installation,
not merely signal consumption. Default continue actions cannot suppress this
resolution, and signal return must not restart an already restored context.
CPU-timer work is requested by a periodic IRQ and consumed on user return.
An armed timer alone does not force a group scan after each syscall; a task
that never returns to userspace can still defer its own timer delivery.

`exit_robust_list` reads the registered userspace head and linked nodes through
`UserConstPtr::read_vm`; `RobustListHead` and `RobustList` implement `UserRead`
because their pointer and integer fields admit every bit pattern. Failed reads
stop the walk. Pointer low-bit tags distinguish PI
entries. Both kinds are marked, but only ordinary entries use plain wake.
The walk stops at the sentinel or `ROBUST_LIST_LIMIT`, then separately handles
a nonnull pending entry.
`handle_futex_death` checks signed address addition/conversion and uses
fault-aware atomic access. It changes only a word whose owner TID matches; the
pending unlocked-word case can wake a waiter without changing ownership.
`do_exit` writes zero to the userspace `clear_child_tid` pointer and wakes only
if the write and futex-key resolution succeed. These pointers are untrusted;
registration is not proof that their pages remain accessible.

## Unsafe inventory and memory invariants

User-transfer callbacks call `krcu::enter_user_quiescent` and
`krcu::exit_user_quiescent`. POSIX owns their placement relative to trap
dispatch; the callbacks publish no accounting state and read no clock. The
watching state, reader cleanup policy and GP service belong to `krcu`. Task storage remains owned by `ktask`. These callbacks do not expose general consumer read or reclamation
admission.

| Location | Operation and necessary invariant | Guard/provider |
|---|---|---|
| `src/runtime.rs`, `run_user_thread_loop` callbacks | Enter/exit user quiescence with no protected borrow spanning the window; restore watching before trap dispatch. | IRQs are masked; only audited RCU-free architecture transfer runs between transitions. The callbacks only enter/exit user quiescence: they publish no accounting state and read no clock. Both defer preemption checks. No scheduling may span the quiescent window. |
| `src/runtime.rs`, `run_user_thread_loop` (LoongArch only) | Calls unsafe `UserContext::emulate_unaligned`; the saved context must still describe the faulting misaligned instruction. | Only reached for the `Misaligned` exception returned by that context's `run`; matches its adjacent SAFETY explanation. |
| `src/runtime.rs`, `exit_robust_list` | Forms a raw address of the embedded list sentinel without loading the field. | The pointer is the registered robust-list head; the address is used only for termination comparison, and actual contents are copied with `read_vm`, as stated by the SAFETY comment. |

There is no direct FFI declaration or inline assembly in this crate. Architecture
and userspace access contracts remain in their providers. The task's PID handle
and installed `Thread` must agree before publication. Last-thread mm, files and TEE
runtime owners must be detached before parent-visible completion. Namespace
release (including the mount namespace) is deliberately deferred to reap, so an
unreaped zombie keeps its `/proc/<pid>/ns/*` identity usable.
After robust-list and `clear_child_tid` access, `finish_thread_exit` atomically
reserves the sole last-thread cleanup role and retires the TID. Only a successful
final-member removal returns true; a repeat cannot run process cleanup again.
The named `exit_tee` stage then runs after the exit mark and CPU accounting,
outside the membership lock and before mm, TEE runtime and fs detach. Namespace
detach is not part of `do_exit`; `wait_reap::release_zombie_namespaces` releases
it after the parent-visible completion and reap.
It may sleep while exchanging `ReleaseOwner`, using the current user runtime
and filesystem context without global TID lookup, user memory or process file
descriptors. Transport failure is logged, local TEE handles are cleared, and
teardown continues within the shared one-second deadline. Parent-visible exit
remains after this stage and all resource detach. TEE configuration gates only
this side effect, not the core retirement and reservation state machine.
An exiting Thread retains only the structural MM pin and coherent scheduling
metadata captured before unpublication. This protects its saved/installed root,
not user mappings. `check_signals` therefore returns immediately for an exiting
thread: no pending handler can write a user frame and no fatal action can repeat
teardown after its active MM owner has been detached. The trap loop's exit check
continues to prohibit user reentry.

## Thread safety

Current-thread helpers require a user runtime; they are not kernel-task APIs.
The loop exclusively updates its supplied user context. Shared process state is
protected by `kprocess`, and atomic futex words may change concurrently in
userspace. No local lock makes the whole robust list immutable: each read is a
snapshot and traversal is bounded. Resource destruction and parent notification
must retain the ordering described in the design document.

## Threats

| ID | Threat / asset | Severity | Trigger | Existing response and residual risk |
|---|---|---|---|---|
| T-01 | Invalid or cyclic robust list consumes kernel work or touches invalid memory. | Medium | An exiting thread supplies malformed nodes or concurrent changes. | Fault-aware reads stop on error and `ROBUST_LIST_LIMIT` bounds traversal; PI entries use PI exit handoff rather than plain wake. |
| T-02 | Futex cleanup changes another owner's lock word. | High | A word is reused or ownership changes during exit. | Compare owner TID, then compare-exchange; retry when the observed word changes. Key resolution is delegated to `kfutex`. |
| T-03 | Faulting file mappings deliver the wrong signal. | Medium | A file-backed access is beyond the backing object's valid extent. | Map `PageFaultOutcome::BusError` to SIGBUS; MM owns the classification. |
| T-04 | Process completion exposes still-held resources. | Medium | Last-thread teardown notifies a waiter before detach. | Release mm/files/fs/ns and IPC state before `complete_process_exit`; logged cleanup failures remain a diagnostic limitation. |
| T-05 | Bootstrap input prevents system startup. | Medium | Empty argv or resolution/loading/publication failure. | Fail-stop panic with context; bootstrap recovery is intentionally not provided. |
| T-06 | A pending termination signal leaves a thread blocked indefinitely. | Medium | A sender queues a signal between the signal check and a later interrupt-flag clear. | Clear old interrupt state before checking pending signals; retain later notifications through timer polling, preemption and userspace reentry. Recheck newly raised interrupts before reentry; CPU kick behavior is provided by the task layer. |
| T-07 | `exec_de_thread` kills siblings while another executor also signals them. | Medium | Exec races a second `kill`/`tgkill`, or a sibling is in uninterruptible work. | The sweep is owned by the execing thread and joins every sibling before the address-space swap; a racing signal to an already-dying sibling is harmless. A sibling stuck in uninterruptible work delays the execing thread until it exits (Linux has the same property). |
| T-08 | A failed or stalled ReleaseOwner leaves stale manager sessions. | Medium | The manager is unreachable or exceeds the exit cleanup deadline. | The client uses one absolute 1-second deadline, clears local handles and continues exit without retry; manager-side sessions may remain and their numeric-PID risk is documented in `tee_kernel/docs/security.md`. |

## Failure-mode analysis

| ID | Failure mode | Cause | Local effect | System effect | Severity (1–4) | Controls |
|---|---|---|---|---|---|---|
| F-01 | Robust-list read/update fails | Malformed or concurrently changed user list | Remaining nodes may not be recovered | User waiters may remain blocked | 3 | Stop invalid traversal; process exit still progresses. |
| F-02 | Resource detach returns an error | Provider cleanup error | Cleanup may be incomplete | Resource lifetime or observer semantics can degrade | 2 | Log per-owner errors and continue the remaining teardown. |
| F-03 | Init construction fails | Missing bootstrap prerequisites or invalid image | PID 1 never runs | Boot stops | 1 | Explicit assertions/expect messages identify the failed stage. |
| F-04 | Group-exit signal delivery races thread exit | Concurrent sibling termination | A sibling may disappear | Best-effort group termination continues | 3 | Iterate currently published siblings and tolerate delivery errors. |
| F-05 | `exec_de_thread` join never returns | A sibling ignores the thread-directed SIGKILL | Exec stalls | The process cannot replace its image | 2 | `SIGKILL` is uncatchable and delivery is best-effort, mirroring Linux `de_thread`; no timeout is imposed. |

## Privacy and limitations

Arguments/environment and robust-list contents are userspace data. Bootstrap
passes them to loading/runtime metadata; fault logs include executable path,
PID and instruction/stack/fault addresses, which require trusted log access.
There is no local redaction or core-file generation. Stop and CoreDump actions
are simplified; robust-futex cleanup remains best effort on memory faults.

## Audit checklist

- Keep identity/runtime construction before publication and activation.
- Preserve robust-list limits, fault-aware accesses and owner-TID atomic checks.
- Preserve mm/files/fs/ns detach before process completion.
- Check optional TEE/TIPC teardown in its configured build.
- Keep the LoongArch context precondition tied to the actual trap result.
- Verify fault signal mapping and the post-timer interrupt recheck when changing the loop.
  That recheck must run once, before the no-handler restart rewrite, and every scan in the
  return must share one handler flag with the restart-block clear. A handler ends the
  drain so userspace runs it. Do not loop on the interrupt flag; cap non-handler
  delivery so a continuous sender cannot pin the return path.
- Do not treat `SignalOsAction::Continue` as a delivered handler when applying
  syscall restart. Clear an `ERESTART_RESTARTBLOCK` slot in the same return
  that publishes `EINTR`, including when the later scan is the one that installs
  the handler.

- `exec_de_thread` joins siblings before the address-space swap and exempts sweep targets from group-exit
  bookkeeping; changing the exec path must keep the group-kill flush ordering.
  Once the group is un-resumable it commits `SIGCHLD` as the parent contract
  before returning, so later fatal failures cannot restore the old signal.

## PM freezer boundary

Returning to user mode is a cooperative checkpoint with enabled IRQs and no
syscall guard retained. A pending freeze waits on the task subsystem's persistent
release predicate; ordinary wakes/signals cannot bypass it. This hook does not
discover tasks, stop creation or make a blocked syscall safe. PM must enumerate
all non-exempt tasks and enforce those boundaries before power transitions.

An admission wait must run after the architecture user-return boundary restored
sleepable task context. The live MM capability retains lifetime while provider
waits run outside MM serialization, and subsequent access uses fresh VMA checks.
Permanent fault outcomes keep their existing signal mapping.

PI cleanup publishes the Thread exit flag before scanning futex buckets and
before TID retirement. That flag does not close CPU accounting: switch-in
still opens an interval until the exit sample is published. Contenders recheck
the flag under the same bucket lock, so exit cannot miss a newly adopted state. The sweep does not access another
process's user mapping; the acquiring waiter repairs OWNER_DIED through its own
address. Robust-list faults remain best effort and do not skip the PI sweep.
