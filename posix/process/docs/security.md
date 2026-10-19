# posix-process — Security and reliability

## Scope, trust and assets

The analysis covers all three source files and their optional TEE/TIPC paths.
Protected assets are the current thread's identity, saved user context, address
space ownership, runtime handles and parent-visible completion. `kprocess`
provides identity/publication and owner-slot synchronization, `memspace` handles
faults/mappings, `kuaccess`/`osvm` perform fault-aware user access, and `kfutex`
resolves futex keys. This crate orders those operations; it does not validate
executable formats or maintain process registries itself.

## External boundaries and validation

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

`exit_robust_list` reads the registered userspace head and linked nodes through
`read_vm`; failed reads stop the walk. Pointer low-bit tags distinguish PI
entries, which are skipped. The walk stops at the sentinel or
`ROBUST_LIST_LIMIT`, then separately handles a nonnull non-PI pending entry.
`handle_futex_death` checks signed address addition/conversion and uses
fault-aware atomic access. It changes only a word whose owner TID matches; the
pending unlocked-word case can wake a waiter without changing ownership.
`do_exit` writes zero to the userspace `clear_child_tid` pointer and wakes only
if the write and futex-key resolution succeed. These pointers are untrusted;
registration is not proof that their pages remain accessible.

## Unsafe inventory and memory invariants

User-transfer callbacks call `krcu::enter_user_quiescent` and
`krcu::exit_user_quiescent`. POSIX owns their placement relative to accounting
and trap dispatch, not the permanent watching state, reader cleanup policy or
GP service. Those belong to `krcu`; task storage remains owned by `ktask`.
These callbacks do not expose general consumer read or reclamation admission.

| Location | Operation and necessary invariant | Guard/provider |
|---|---|---|
| `src/runtime.rs`, `run_user_thread_loop` callbacks | Enter/exit user quiescence with no protected borrow spanning the window; restore watching before accounting and trap dispatch. | IRQs are masked; only audited RCU-free architecture transfer runs between transitions. Entry accounting finishes before CPU selection, and exit accounting starts after watched restoration. No scheduling may span the quiescent window. |
| `src/runtime.rs`, `run_user_thread_loop` (LoongArch only) | Calls unsafe `UserContext::emulate_unaligned`; the saved context must still describe the faulting misaligned instruction. | Only reached for the `Misaligned` exception returned by that context's `run`; matches its adjacent SAFETY explanation. |
| `src/runtime.rs`, `exit_robust_list` | Forms a raw address of the embedded list sentinel without loading the field. | The pointer is the registered robust-list head; the address is used only for termination comparison, and actual contents are copied with `read_vm`, as stated by the SAFETY comment. |

There is no direct FFI declaration or inline assembly in this crate. Architecture
and userspace access contracts remain in their providers. The task's PID handle
and installed `Thread` must agree before publication. Last-thread runtime owners
must be detached before parent-visible completion so zombie identity cannot keep
files, mounts or active mm ownership alive.
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
| T-01 | Invalid or cyclic robust list consumes kernel work or touches invalid memory. | Medium | An exiting thread supplies malformed nodes or concurrent changes. | Fault-aware reads stop on error and `ROBUST_LIST_LIMIT` bounds traversal; PI entries are skipped and their cleanup is not implemented here. |
| T-02 | Futex cleanup changes another owner's lock word. | High | A word is reused or ownership changes during exit. | Compare owner TID, then compare-exchange; retry when the observed word changes. Key resolution is delegated to `kfutex`. |
| T-03 | Faulting file mappings deliver the wrong signal. | Medium | A file-backed access is beyond the backing object's valid extent. | Map `PageFaultOutcome::BusError` to SIGBUS; MM owns the classification. |
| T-04 | Process completion exposes still-held resources. | Medium | Last-thread teardown notifies a waiter before detach. | Release mm/files/fs/ns and IPC state before `complete_process_exit`; logged cleanup failures remain a diagnostic limitation. |
| T-05 | Bootstrap input prevents system startup. | Medium | Empty argv or resolution/loading/publication failure. | Fail-stop panic with context; bootstrap recovery is intentionally not provided. |
| T-06 | A pending termination signal leaves a thread blocked indefinitely. | Medium | A sender queues a signal between the signal check and a later interrupt-flag clear. | Clear old interrupt state before checking pending signals; retain later notifications through timer polling, preemption and userspace reentry. Recheck newly raised interrupts before reentry; CPU kick behavior is provided by the task layer. |
| T-07 | `exec_de_thread` kills siblings while another executor also signals them. | Medium | Exec races a second `kill`/`tgkill`, or a sibling is in uninterruptible work. | The sweep is owned by the execing thread and joins every sibling before the address-space swap; a racing signal to an already-dying sibling is harmless. A sibling stuck in uninterruptible work delays the execing thread until it exits (Linux has the same property). |

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
are simplified, and PI robust-futex cleanup is skipped.

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

- `exec_de_thread` joins siblings before the address-space swap and undoes group-exit
  bookkeeping; changing the exec path must keep the group-kill flush ordering.
