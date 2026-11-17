# ksignal security and reliability

## Ordinary Owner Allocation Fallback

Ordinary standard and realtime SI_USER notifications may lose detailed storage.
Standard admission first coalesces; realtime sends always attempt allocation.
NOWAIT failure leaves an authoritative
pending bit and no owner. Ordinary SIGKILL skips allocation altogether. A
bit-only dequeue synthesizes zeroed SI_USER information on the stack and carries
no receipt. User-controlled SI_TIMER does not grant timer authority. Prepared
records bypass allocation and are never converted into this fallback. Ordinary
non-SI_USER realtime failure returns WouldBlock without publishing queue state,
fatal epochs or wakes. Explicit kernel_fallback is reserved for audited kernel
sources, never inferred from ABI SI_KERNEL. Ordinary From<SignalInfo> cannot
acquire that policy. Prepared receipts remain outside this fallback.

Ignored cancellation must not clear an unrelated no-info pending bit. Only
actual removal from a pending FIFO permits cancellation to clear its last bit.
Mixed queues retain the aggregate-bit semantics: no separately counted fallback
survives the final detailed owner. Normal and explicit-mask dequeue share the
same representation; dirty projections and wakes follow successful admission.
Private allocator-seam tests cover loss of details, no-retry coalescing, SIGKILL,
forged timer payloads, reserved timer ownership and cancellation isolation.
Realtime SI_USER tests also cover repeated failed admission attempts and mixed
detailed/bit-only arrival orders without an extra synthesized occurrence.
Private allocator-seam regressions also cover realtime WouldBlock, unchanged
existing FIFO state, ignored admission without allocation, unchanged fatal
snapshots/projections/wake observations, and successful retry. There is no global
allocator override, new unsafe code or syscall-level OOM injection.

## Invalidated Pending Ownership

Invalidation is monotonic and checked inside the signal admission transaction
before fatal and wake bookkeeping. Cancellation uses exact storage identity,
the captured manager and generation-checked private slot; a pending link alone
is never authority to unlink. Delayed tokens only remove receipts at or below
their invalidated generation. Safe FIFO cursor traversal avoids new unsafe
membership assumptions, at O(n) cancellation latency for that signo.
Pending-bit/projection changes are serialized; detached payloads and final
references release outside signal/timer locks. Already detached receipts remain
occupied until released and undergo the existing timer completion validation.
Tests cover shared/private queues, standard/realtime signals, late cancellation,
in-flight send rejection, detached ownership and ignored-record reuse.

## Timer Provenance Boundary

Process and private-slot admission retain the shared disposition lock until
SigLock commits the ignored/fatal decision and queue state. The lock order is
configuration ownership -> shared actions -> SigLock -> record state; no
transaction acquires an action lock in reverse order. CLONE_SIGHAND peers have
different ownership locks, so ownership alone cannot protect this boundary.
Both guards are released before arrival callbacks or rejected owner destruction.
Action replacement flushes only the caller group's queues. Valid periodic
receipts retain their occupied storage on an ignored list; one-shots do not.
Same-value Ignore still flushes. Explicit-mask consumers can acknowledge blocked
ignored records, but the normal handler path retains/discards them before
completion. ABI SI_TIMER cannot authorize retention. Cancellation and destruction
cover both lists. Detached discard batches use existing intrusive links and
release payloads after unlocking; no temporary allocation or new unsafe cursor
is introduced. Replay to dead targets discards without acknowledgement.

Requeue schedules the existing ready link under SigLock -> ready -> record,
without invoking its waker under those locks. The worker upgrades the weak
origin and validates exact timer storage before obtaining a destination; it
then rechecks disposition, pending/liveness and task identity outside the timer
mutex. Ignored shared signals cannot wake an unblocked peer when another thread
blocks the signal. A stale
request may cause a harmless pending-state recheck, never a receipt completion
or publication into a reused slot. This is not an SMP stress-test proof.

Ordinary SignalInfo conversion never grants a TimerSignalReceipt, even when
userspace supplies SI_TIMER and matching ID/sequence fields. Kernel code is
trusted to prepare timer records only for actual timer emissions. The unreserved
SignalNotification::timer constructor is crate-private and available only with
cfg(unittest) for compatibility fixtures. Production uses reserved preparation.
The non-Clone receipt is consumed once after SigLock is released, before
generic observers. Both handler and explicit-mask consumers use this path.
Its weak TimerSignalOrigin is captured at creation and upgraded only at unlocked
completion. Expiration rejects delivery, with no receiver/PID fallback. The
origin method may acquire timer/signal locks; it never runs in queue Drop.
No strong runtime cycle is added, although a weak reference retains its Arc
control block until release. The last weak release also occurs outside signal
and configuration locks on production send/dequeue/retirement paths.
Ordinary coalescing is unchanged; ignored/flushed notifications never acknowledge
timers. Ordinary realtime owner errors now propagate through senders.
The timer manager issues non-wrapping u64 generations and rejects exhaustion.
Receipts retain the full value independently of the truncated ABI field.
POSIX TimerSignalTarget binds destination manager identity and optional slot,
not a numeric PID/TID. Upgrade precedes locking; private liveness and slot
generation are checked with enqueue, and empty shared targets reject sends.
Wake hints require a second identity check against a retained task. Ordinary
signal routes and legacy itimers are unchanged. Slot generations also use a checked
u64 source and never wrap: exhaustion rejects registration before mutation,
returns prepared storage to the unlocked caller, and does not prevent retirement.
Shrinking the slot table never resets the generation source. SlotId is scoped
to its manager, not a standalone cross-manager capability. Ordinary KArc node
allocations use NOWAIT. Standard and realtime SI_USER failure preserve the signal
bit; other ordinary realtime failure returns WouldBlock unless kernel fallback
is explicit. Prepared records reserve
storage fallibly before locking; submission, dequeue and ready scheduling do
not allocate. A busy reservation cannot overwrite a queued payload.

Standard notification dequeue now transfers the KArc node in DetachedSignal rather
than freeing it inside SigLock. Handler and explicit-mask consumers release
that storage and complete provenance only after their transaction returns.
Thread retirement transfers the entire slot owner out of the transaction before
destroying its private queue. Queue state and registration are already detached
when final storage release occurs. Destructors schedule availability through
the alarm wake hook, never invoke origin completion/availability directly.
Tests verify retained ownership, independent successor registration and pending
state, and no acknowledgement when a detached notification is discarded.
Prepared tests cover mixed FIFO, busy exclusion through completion, drain,
ordinary coalescing, independent standard timers, rejected ownership and expired
origins. The standard bypass requires a live reserved lease, not SI_TIMER data.
No public allocator failure hook is introduced; allocator failure injection is
private to kalloc-types.

Pending and ready links are separate AtomicLinks, projected by a private
concrete adapter with KArcPointerOps. The unique lease prevents double pending
membership; the global ready lock prevents double ready membership. Payloads
contain no lease while queued, avoiding a strong self-cycle. Pending queue
destruction reconstructs leases only after callers detach the entire queue.
Ready processing retains a strong node owner while invoking the weak origin
outside all locks. Exact same_storage checks prevent timer-ID reuse from
turning stale availability into successor retry. Storage clones do not clone
leases. Final strong/weak release happens outside these locks.

Send transactions now return the original notification on coalescing or stale
slot rejection instead of destroying it inside SigLock. Nonretained ignored sends
carry ownership out of the transaction. Production callers release returned
ownership only after releasing the outer handler-ownership lock as well; missing
configuration drops that guard before the original notification by local
declaration order. Public bool/TID wake hints remain unchanged and do not encode
whether a record was inserted. Tests verify returned provenance survives retry,
stale sends do not publish into a reused slot, and ignored/released sends do not
acknowledge timers. Ignored-but-blocked sends queue; unblocked periodic records
remain retained until replay, invalidation or queue destruction.

## Scope, assets, and boundaries

Thread-slot owners use fallible `KBox` allocation before acquiring `SigLock`.
Invalid policies or NoMemory return before slot publication; the temporary
projection reference is released. Default construction requires sleepable,
reclaim-compatible task context; explicit NOWAIT does not promise NMI safety,
allocator reentrancy, reserves, or bounded latency. Existing Arc allocations
and slot-table growth remain infallible. Tests reject an invalid policy,
verify absent registration and successful retry, and check process-reference
cleanup. They do not force global allocator exhaustion.

All of `src/` is covered. Assets are signal state consistency (blocked sets,
pending queues, actions), the integrity of the user handler frame, and the
`siginfo` payloads consumed by user handlers. Trusted callers are `ksyscall`
(ABI decoding, permission checks and syscall-specific zero-signal policy)
and the posix process runtime. Untrusted inputs reaching this
crate are: user-supplied `sigaction`/`siginfo`/`sigprocmask`/`sigaltstack`
payloads (validated ABI values converted before the call), user stack memory
holding signal frames, and the return address of the handler trampoline.

## External boundaries

| Boundary | Direction | Content |
|---|---|---|
| User memory (via `UserPtr::write_vm`/`write_vm_slice`) | kernel writes | handler frame (`siginfo`, `ucontext`, saved state; x86_64 also the FP/XSAVE region and restorer word) |
| User memory (direct deref) | kernel reads | `sigreturn` frame at user `sp` |
| User ABI payloads via `ksyscall` | inbound | sigaction structs, siginfo (rt_sigqueueinfo), sigset words, sigaltstack |
| Raw signal numbers via `Signo::from_raw` | inbound | checked numeric carrier conversion and signal validity; syscall-specific zero handling remains in `ksyscall` |
| Alternate signal stack | user-controlled addresses | `sp`/`size` used for frame placement |
| Signal trampoline page | kernel text mapped user-readable/exec | fixed `SIGNAL_TRAMPOLINE` vaddr |

## Unsafe inventory

The non-consuming fatal-pending query takes only the existing IRQ-safe signal
lock. Scheduler wakeup must remain outside that lock to avoid inversion with
the waiter's post-Blocked recheck. Signal consumption and disposition handling
remain owned by normal delivery, not by the wait predicate.

Signal-frame and `siginfo` unsafe code is in `types.rs`, `arch/x86_64.rs`,
and `api/thread.rs`; the intrusive-list adapter in `notification.rs` is covered
under "Timer Provenance Boundary". Each unsafe block or impl carries
an inline `SAFETY:` justification.

| Location | Operation | Invariant |
|---|---|---|
| `types.rs` `From<SignalInfo> for k_siginfo` | unsafe wrapping of a raw union record | `SignalInfo` seals its raw field; all construction starts from a zeroed record or a complete `k_siginfo` user copy |
| `types.rs` `header`/`header_mut`/`sifields`/`sifields_mut` | union arm access | bindgen preserves ABI offsets; readers only touch the arm selected by `si_code` or by the constructor that populated it |
| `types.rs` `ChildExitSignalInfo::new` | `_sigchld` arm write | `CLD_*` codes select the arm and every exposed field is initialized |
| `types.rs` `timer_id`/`timer_overrun`/`timer_signal_seq`/`sigval` | arm reads | guarded by `SI_TIMER` or negative `si_code` checks |
| `types.rs` `unsafe impl Send/Sync for SignalInfo` | auto-trait override | payload is opaque ABI bits, not dereferenceable aliases; mutation requires `&mut` |
| `arch/x86_64.rs` `UserRead`/`UserWrite for UContext` | inbound and outbound ucontext byte contract | repr(C) integer-only fields; the `stack_t` hole is the explicit zeroed `SignalStackAbi::padding`; offsets and the 304-byte size are asserted. Restored registers and xstate are validated separately |
| `api/thread.rs` `UserWrite for SignalFrame` (non-x86) | outbound frame byte contract | repr(C) frame; nested contexts model every alignment hole as an explicit zeroed field; `siginfo` is the complete 128-byte `k_siginfo` record as bytes; sizes and offsets are asserted per architecture |
| `api/thread.rs` `restore` (non-x86) | direct frame dereference at `sp` | trusts that user `sp` maps the frame previously written by `handle_signal` |

`SignalInfo::empty` safely initializes the full 128-byte `_si_pad` union arm
with zeros; constructors then set the header and exposed payload fields.

The union-arm reads are the `k_sigval`/`siginfo_t` ABI boundary: the kernel
treats payload fields as raw bits and never dereferences pointer-like values
(`sival_ptr`) stored by userspace.
`set_timer_fields` constructs the outbound timer `sigval` through its
full-width pointer arm, so an integer-width member cannot leave bytes unset.

## Threat analysis

| ID | Threat | Severity | Trigger | Response |
|---|---|---|---|---|
| T-01 | Handler frame overwrite of unmapped/guard user stack | Medium | `SA_ONSTACK` with invalid `sigaltstack`, or stack exhaustion at dispatch | Frame write uses checked `write_vm`; failure is converted to `CoreDump` fatal exit instead of a kernel fault |
| T-02 | `sigreturn` on a forged frame | High | user alters pointers, registers or FP header | x86_64 copies and validates the complete supported state before commit; invalid frames cause SIGSEGV. MXCSR is sanitized, privileged RFLAGS are preserved, and unblockable signals are removed from the mask. Other architectures retain their existing restore path |
| T-03 | Invalid signal number from user input reaching decode | Medium | invalid or truncating raw numeric argument, or unvalidated imported `siginfo` | `ksyscall` uses `Signo::from_raw` before producing signal payloads; primitive negative or out-of-width values cannot wrap into a valid signal. Zero probes stay separate. `SignalInfo::signo` still documents and relies on its existing validated-payload caller contract |
| T-04 | Stale POSIX timer signal delivered after timer rearm | Low | timer signal dequeued after `timer_settime` changed the timer | typed timer receipts carry sequence authority to the unlocked kprocess validator; ordinary ABI payloads cannot invoke it |
| T-05 | Signal flood exhausting kernel memory | Medium | unbounded `rt_sigqueueinfo`/timer expirations | Ordinary standard sends coalesce while any record is pending; each prepared timer can hold one record even for a standard signo. Queued timer storage is reserved at creation. Ordinary realtime growth remains unbounded; Linux resource-limit parity is not claimed |
| T-06 | Observer table races | Low | re-registering an observer while signals dequeue | Table access is `SpinNoIrq`-serialized; overwrite is documented last-wins |
| T-07 | `SignalInfo` auto-trait unsoundness | High (if violated) | storing a `sival_ptr` that becomes a dereferenceable alias | `unsafe impl` is justified by opaque-bits treatment; all reads return raw values, no dereference of payload pointers exists in this crate |
| T-08 | Signal-frame output exposes kernel padding bytes | High (fixed) | a frame struct with implicit alignment holes, or a compiler-ordered layout, is copied to the user stack as raw bytes | Every outbound frame region is a `UserWrite` carrier copied through `UserPtr`. Non-x86 `SignalFrame` is repr(C) in Linux `rt_sigframe` order with explicit zeroed padding in the nested contexts and frame tail; x86_64 copies `k_siginfo`, a hole-free `UContext`, a zero-initialized FP byte buffer and the restorer word separately. CG-005 checks the compiled layouts and CG-006 checks the copy path |

Compile-time size and offset assertions pin the 64-bit frame layouts. The
holes listed below were implicit before the fix and are now explicit zeroed
fields; the non-x86 frame also had compiler-chosen (`repr(Rust)`) field order.

| Architecture | Explicit zeroed padding | Outbound layout |
|---|---|---|
| aarch64 | `ucontext` 168–176 (before 16-aligned `uc_mcontext`); `sigcontext` 280–288 (before `__reserved`); frame tail 4696–4704 | `siginfo` 0–128, `ucontext` 128–4688, saved `tpidr` 4688–4696, pad 4696–4704 (4704 bytes) |
| riscv64 | `ucontext` 168–176 | `siginfo` 0–128, `ucontext` 128–1088 (1088 bytes); `uc_mcontext` holds PC, x1..x31, then the 528-byte FP union |
| loongarch64 | `ucontext` 168–176; `sigcontext` 268–272 (after `sc_flags`) | `siginfo` 0–128, `ucontext` 128–576 (576 bytes) |
| x86_64 | none needed: `UContext` has no implicit holes (`stack_t` padding was already explicit) | From handler SP: `pretcode` 0–8, `ucontext` 8–312, `siginfo` 312–440, FP/XSAVE region at 440 (64-byte aligned), matching Linux `rt_sigframe`. The regions are contiguous; at least 128 bytes below the original SP are skipped, not written |

The signal-frame order follows Linux `rt_sigframe`: info precedes ucontext on
arm64, RISC-V and LoongArch. `k_sigaltstack` matches Linux `stack_t` offsets
on all four targets.

## FMEA

| ID | Failure mode | Cause | Local effect | System effect | Sev | Response |
|---|---|---|---|---|---|---|
| F-01 | Frame write fails | unmapped user stack | dispatch returns `CoreDump` | process exits | 3 | documented fatal path |
| F-02 | Retired slot reused while a handle is retained | thread exit without releasing the slot, then reuse | stale handle addresses a successor thread's state | signals routed to the wrong thread | 3 | generation-checked handles; `exit()` only clears the target flag, `Drop` releases the slot |
| F-03 | Pending bit set without queue entry | bookkeeping divergence under concurrent put/dequeue of same signal | dequeue returns `None` while bit set (fast-path flag stays true) | extra slow-path iterations, no lost signal | 4 | bit and queue mutated under the same `SpinNoIrq` per manager |
| F-04 | Availability projection under-reports | a caller publishes "nothing pending" from an observation taken under another lock | deliverable signal stays queued until an unrelated event re-arms the check | thread hangs waiting for a handler that never runs | 2 | projection written only by `SigLock::republish`, inside the transaction that changed state, derived from that state, and only for slots the transaction marked dirty; no caller-side clears exist |

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

`SigLock` guards the whole deliverability predicate in one `SpinNoIrq`:
the process-directed queue and each thread's private queue, blocked set and
saved sigmask. The lock-free fast path reads only a per-thread projection, which
may over-report but never under-report and is written solely by the lock's own
commit step; query paths use a read-only capability that recomputes nothing.
`ThreadSignalManager::masks()` returns only the private/shared pending and
blocked bitmaps from one such critical section; it exposes no `SignalInfo`
payload, queue entry or retainable lock, and its private fields prevent callers
from constructing a snapshot that never existed. Dispositions are read under
`handlers` and can reflect a different instant. `SignalActions` is
`Arc`-shared and locked per access. Its shared
reference is internal to signal managers; external callers receive action
values, not an independently retainable lock.
Fork copies the whole action snapshot under its lock and releases the lock
before allocating the independent child state. An outer process ownership lock
serializes table selection with fork and exec. The lock order is ownership,
represented by `ProcessSignalManager.handlers`, then `SignalHandlers.actions`
and `SigLock`; no path takes another process's ownership lock and no path takes
`SigLock` before `handlers`.
Only shared fork adds a table owner, so a sole-owner count under the ownership
lock permits in-place reset. Exec allocation and old-table destruction happen
outside signal locks. Pending publication holds the ownership lock after
classification locks are released, so release and acceptance are serialized.
No waker runs under the ownership lock or under `SigLock`. `SignalSet` is a plain `Copy` bitmap
shared by value.
`SignalInfo` `Send`/`Sync` rely on the opaque-bits invariant above. The
global observer table is lock-serialized. Slot registration, retirement and
target selection all run inside `SigLock`, so a handle cannot be observed as
reusable while a send is selecting targets.

## Failure handling

All user-memory writes in dispatch return an OS action (`CoreDump`) rather
than an error to keep the user-return path uniform. Queue operations are
infallible. `signo()` and the observer registration functions document their
panic contracts; no other intentional panics exist in this crate.

## Privacy analysis

`SignalInfo` carries child exit status, PIDs, UIDs and CPU times — process
metadata, not user file content. `SignalMasks` exposes only pending/blocked
bitmaps; it neither reveals queued payloads nor permits mutation. No other user
data is processed or logged.

## Known limitations

- TimerSignalSendOutcome returns rejected ownership to the unlocked bridge.
  TargetGone, Ignored and Coalesced are admission outcomes, not successful
  completion or proof of future delivery. A missing wake hint can also mean
  successful blocked admission. No receipt completion runs on rejection or
  destruction. Reserved-record return supports deferred new-generation demand;
  Retained denotes ignored periodic storage, not delivery. Replay does not
  release occupancy or acknowledge; discarded one-shots never replay.
- Notification scope does not imply readable data: pending queues remain private
  to a process/thread, and unselected or repeated signals may request rechecks.
- `SIGSTKFLT` default action follows Linux (terminate) though it is unused.
- `SignalOsAction::Stop`/`Continue` are reported, but job-control stop state
  is approximated by the posix runtime (documented there: stop currently
  exits with code 1).
- x86_64 exposes editable Linux `fpregs`, including YMM state only when enabled.
  `ksyscall` copies the frame and checks user registers; `kcpu` bounds and
  validates the copied xstate payload before XRSTOR. Invalid frames raise
  SIGSEGV. MXCSR retains the existing trusted-mask sanitization policy.
- On other architectures `restore` still trusts a mapped frame at `sp`; a corrupted frame produces a user
  fault after return rather than a kernel-side checked error. The restored
  mask is user-controlled, so it goes through the same blocked-mask write entry
  as `rt_sigprocmask`, which drops `SIGKILL`/`SIGSTOP` (Linux `do_sigreturn` ->
  `set_current_blocked`). A frame that names them cannot make the process
  unkillable.

## Audit checklist

- New `si_code` producers must populate the union arm they advertise before
  the `SignalInfo` can be observed.
- Every new user-memory write in the dispatch path must go through `UserPtr`
  with a `UserWrite` carrier whose layout has no implicit padding and is
  pinned by const assertions; never raw deref or raw-pointer `write_vm`.
- `SigTxn::set_blocked`/`add_blocked` are the only blocked-mask writers and
  always drop `SIGKILL`/`SIGSTOP`; new writers must go through them instead of
  re-deriving the rule.
- Dequeue observers must stay cheap and non-sleeping.
- The availability projection is written only by `SigLock::republish`, inside
  the transaction that changed state. Never publish an availability answer from
  a caller, and never clear it after releasing the lock.
