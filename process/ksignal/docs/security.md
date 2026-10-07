# ksignal security and reliability

## Scope, assets, and boundaries

All of `src/` is covered. Assets are signal state consistency (blocked sets,
pending queues, actions), the integrity of the user handler frame, and the
`siginfo` payloads consumed by user handlers. Trusted callers are `ksyscall`
(ABI decoding and permission checks, e.g. kill permission and signo
validation) and the posix process runtime. Untrusted inputs reaching this
crate are: user-supplied `sigaction`/`siginfo`/`sigprocmask`/`sigaltstack`
payloads (validated ABI values converted before the call), user stack memory
holding signal frames, and the return address of the handler trampoline.

## External boundaries

| Boundary | Direction | Content |
|---|---|---|
| User memory (via `osvm` `write_vm`) | kernel writes | handler frame (`ucontext`, `siginfo`, restorer push) |
| User memory (direct deref) | kernel reads | `sigreturn` frame at user `sp` |
| User ABI payloads via `ksyscall` | inbound | sigaction structs, siginfo (rt_sigqueueinfo), sigset words, sigaltstack |
| Alternate signal stack | user-controlled addresses | `sp`/`size` used for frame placement |
| Signal trampoline page | kernel text mapped user-readable/exec | fixed `SIGNAL_TRAMPOLINE` vaddr |

## Unsafe inventory

All unsafe blocks are in `types.rs` plus one in `api/thread.rs`; each carries
an inline `SAFETY:` justification.

| Location | Operation | Invariant |
|---|---|---|
| `types.rs` `SignalInfo::empty` | `mem::zeroed()` for `siginfo_t` | all-zero is a valid baseline; constructors set header and exposed payload fields before observation |
| `types.rs` `header`/`header_mut`/`sifields`/`sifields_mut` | union arm access | bindgen preserves ABI offsets; readers only touch the arm selected by `si_code` or by the constructor that populated it |
| `types.rs` `ChildExitSignalInfo::new` | `_sigchld` arm write | `CLD_*` codes select the arm and every exposed field is initialized |
| `types.rs` `timer_id`/`timer_overrun`/`timer_signal_seq`/`sigval` | arm reads | guarded by `SI_TIMER` or negative `si_code` checks |
| `types.rs` `unsafe impl Send/Sync for SignalInfo` | auto-trait override | payload is opaque ABI bits, not dereferenceable aliases; mutation requires `&mut` |
| `api/thread.rs` `restore` | `&*(sp as *const SignalFrame)` | trusts that user `sp` maps the frame previously written by `handle_signal` |

The union-arm reads are the `k_sigval`/`siginfo_t` ABI boundary: the kernel
treats payload fields as raw bits and never dereferences pointer-like values
(`sival_ptr`) stored by userspace.

## Threat analysis

| ID | Threat | Severity | Trigger | Response |
|---|---|---|---|---|
| T-01 | Handler frame overwrite of unmapped/guard user stack | Medium | `SA_ONSTACK` with invalid `sigaltstack`, or stack exhaustion at dispatch | Frame write uses checked `write_vm`; failure is converted to `CoreDump` fatal exit instead of a kernel fault |
| T-02 | `sigreturn` on a corrupted/forged frame | Medium | user changes `sp` or overwrites frame memory, then calls `rt_sigreturn` | Context restore treats frame contents as data; register/pc/sp are re-entered through the normal user-return path so a bad frame cannot corrupt kernel state. `saved` is user-writable, so on x86_64 the restored `MXCSR` is masked against the trusted feature mask before `FXRSTOR` (avoids kernel `#GP`); `sigmask` is range-limited by `SignalSet`. Residual risk: user can forge its own register/FP state, which is also true on Linux |
| T-03 | Invalid `si_signo` from user `siginfo` reaching decode | Medium | `rt_sigqueueinfo` payload with signo 0 or >64 | `ksyscall` validates the signo before conversion; `SignalInfo::signo` documents the panic contract for unvalidated payloads. Residual: the crate itself trusts its callers here |
| T-04 | Stale POSIX timer signal delivered after timer rearm | Low | timer signal dequeued after `timer_settime` changed the timer | dequeue observers compare the embedded `signal_seq`; stale signals are dropped (`kprocess` wiring) |
| T-05 | Signal flood exhausting kernel memory | Medium | unbounded `rt_sigqueueinfo`/timer expirations | Standard signals coalesce to one instance; real-time queues are bounded by sender rate — matches Linux behavior; no additional kernel cap (accepted residual risk, same as Linux) |
| T-06 | Observer table races | Low | re-registering an observer while signals dequeue | Table access is `SpinNoIrq`-serialized; overwrite is documented last-wins |
| T-07 | `SignalInfo` auto-trait unsoundness | High (if violated) | storing a `sival_ptr` that becomes a dereferenceable alias | `unsafe impl` is justified by opaque-bits treatment; all reads return raw values, no dereference of payload pointers exists in this crate |

## FMEA

| ID | Failure mode | Cause | Local effect | System effect | Sev | Response |
|---|---|---|---|---|---|---|
| F-01 | Frame write fails | unmapped user stack | dispatch returns `CoreDump` | process exits | 3 | documented fatal path |
| F-02 | Dead thread entry kept in `children` | thread exit without prune | stale tid selected | wake/interrupt to dead tid is filtered by callers; entries pruned on next scan | 4 | opportunistic `retain` pruning |
| F-03 | Pending bit set without queue entry | bookkeeping divergence under concurrent put/dequeue of same signal | dequeue returns `None` while bit set (fast-path flag stays true) | extra slow-path iterations, no lost signal | 4 | bit and queue mutated under the same `SpinNoIrq` per manager |
| F-04 | `has_pending` false negative | ordering bug between store and queue | signal delayed until next set | latency only | 4 | Release store after successful put; Acquire loads on check |

## Thread safety

Signal-arrival poll notifications and temporary-source drops run after releasing
all signal locks. Registration also runs outside those locks, including weak
upgrade and its potentially final strong-reference drop.
Registered callbacks must be nonblocking and valid in the sender's context.
Notifications are shared by CLONE_SIGHAND peers, but consumers check their own thread's
pending set and mask; no signal payload is exposed through the wake operation.
Registration may fail and callers must propagate the error instead of sleeping
without a subscription. Dropped guards cancel waits; a concurrent late wake is
allowed by kpoll and never grants readiness by itself.
Weak source capture cannot retain handler ownership or prevent source death.
An expired captured source never rebinds after exec. Inactive manager capture
returns `NoSuchProcess`, mapped to `InvalidState` by thread registration; normal
expiry after capture is a successful registration with no waiter installed.

Managers use per-field `SpinNoIrq` cells; `SignalActions` is `Arc`-shared
and locked per access. Its shared reference is internal to signal managers;
external callers receive action values, not an independently retainable lock.
Fork copies the whole action snapshot under its lock and releases the lock
before allocating the independent child state. An outer process ownership lock
serializes table selection with fork and exec. The lock order is ownership,
represented by `ProcessSignalManager.handlers`, then `SignalHandlers.actions`,
children, blocked; no path takes another process's ownership lock.
Only shared fork adds a table owner, so a sole-owner count under the ownership
lock permits in-place reset. Exec allocation and old-table destruction happen
outside signal locks. Pending publication holds the ownership lock after
classification locks are released, so release and acceptance are serialized.
No waker runs under the ownership lock. `SignalSet` is a plain `Copy` bitmap
shared by value.
`SignalInfo` `Send`/`Sync` rely on the opaque-bits invariant above. The
global observer table is lock-serialized. `find_target_thread` mutates the
children list (prune) during sends from any thread.

## Failure handling

All user-memory writes in dispatch return an OS action (`CoreDump`) rather
than an error to keep the user-return path uniform. Queue operations are
infallible. `signo()` and the observer registration functions document their
panic contracts; no other intentional panics exist in this crate.

## Privacy analysis

`SignalInfo` carries child exit status, PIDs, UIDs and CPU times — process
metadata, not user file content. No other user data is processed or logged.

## Known limitations

- Notification scope does not imply readable data: pending queues remain private
  to a process/thread, and unselected or repeated signals may request rechecks.
- `SIGSTKFLT` default action follows Linux (terminate) though it is unused.
- `SignalOSAction::Stop`/`Continue` are reported, but job-control stop state
  is approximated by the posix runtime (documented there: stop currently
  exits with code 1).
- FPU/vector state is not carried in the ABI `fpregs` (x86_64 zeroes
  `fpregs_mem`); restoration goes through `UserRestorableContext`, which on
  x86_64 holds an `FXSAVE` snapshot. Because that snapshot is read back from
  the user-writable frame, `restore` clears unsupported `MXCSR` bits against a
  cached trusted feature mask before `FXRSTOR`.
- `restore` trusts a mapped frame at `sp`; a corrupted frame produces a user
  fault after return rather than a kernel-side checked error.

## Audit checklist

- New `si_code` producers must populate the union arm they advertise before
  the `SignalInfo` can be observed.
- Every new user-memory access in the dispatch path must use checked `osvm`
  access; never raw deref on the write side.
- Keep `SIGKILL`/`SIGSTOP` unblockable in `set_blocked`.
- Dequeue observers must stay cheap and non-sleeping.
- Fast-path atomics must be updated with the queue-lock discipline described
  in design.md (store after successful enqueue under lock).
