# procfs - Security and Reliability

## Commitment statistics

`Committed_AS` is a lock-free snapshot of the MM private commitment counter.
It is informational and can change during a multi-line meminfo read; it does
not expose object addresses or promise physical allocation. Admission never
uses the printed value as an available-memory budget.

## CPU information boundary

`/proc/cpuinfo` is read-only, including for privileged writers through the
SimpleFile closure backend. It consumes immutable, policy-filtered kcpu snapshots
and printable sanitized identity strings. It does not claim AVX/XSAVE support
from raw hardware bits or substitute the reader's CPU for a missing snapshot.
An empty/missing flag means not reported, not necessarily absent from hardware.
Each flags line describes that CPU only; consumers must account for migration.

Formatting is bounded by the present CPU count and fixed feature table, with no
CPUID, architecture-register or remote-CPU calls on the read path. It retains existing String allocation
behavior. Identity (vendor/model) is diagnostic public information, not an
attestation or authorization input. Each read regenerates text from the published snapshots; an AP completing
identification between reads can change output length. No cross-read atomic
snapshot or CPU-hotplug contract is claimed.

The same boundary applies to AArch64, RISC-V and LoongArch output. Register IDs
are formatted numerically; RISC-V firmware strings are reduced to kcpu's fixed
extension bitmap before publication, never interpolated. Missing identity IDs
remain unknown and unidentified CPUs have no reported capabilities. Vector units
without context preservation are omitted. LoongArch model text labels the PRID
explicitly rather than inventing a marketing name. These per-CPU fields are
diagnostic; they do not replace the independently generated ELF auxiliary vector
or guarantee feature availability after migration.


## IRQ affinity boundary

`/proc/irq` combines a per-superblock stable `IrqNamespace` with KIRQ-owned
identity and policy. The root source uses the common sparse `SimpleDir` adapter
and KIRQ's `next_irq_line`; it publishes only Lines with action history, has no
fixed name-count cap, and does not retain a Line handle. Numeric lookup accepts
only canonical decimal names; malformed, overflowing and leading-zero aliases
fail lookup. Root enumeration is weak: insert/remove can shift or repeat
ordinal results.

Each `LineRecord` owns stable directory/file inodes and retains the exact
`IrqLineHandle` used when it was created. The inode graph never owns the record,
avoiding a record/inode strong cycle. `is_same_line` compares underlying KIRQ
Line identity. A stale lookup does not delete a successor record; a replacement
record is installed only after its inode/observer build succeeds. Retirement
notification removes the record only for the exact recorded Line and runs after
KIRQ locks are released. Old directories/files keep their historical inode and
handle; fresh lookup of the retired Virq fails. A successor gets distinct
inodes.

The four file inodes are real, precreated `SeqFileInode` VFS inodes installed in
a fixed `DirMapping`; `SimpleDir` supplies actual d_ino/dot/dotdot. The old
`affinity_status` file is removed. Requested files start at 0644 only for
ordinary sources with static user-routing capability; fixed, PerCpu/NMI/MSI,
no-flow and controllerless sources start 0444. Effective files are 0444. Mode is
fixed at record creation: action restrictions/removal do not rewrite it, and
chmod/chown persist until the record is retired; a full-path reopen after
closing the old file and root dentry observes the same inode metadata, while a
different superblock and a successor start with fresh defaults. Open is lazy: it
checks the retained identity but does not read a mask, so a live Line whose
requested mask is still unknown can be opened O_WRONLY/O_RDWR and receive its
first command.
First read or `SEEK_END` reports `ENODATA`; no status text is synthesized.

Requested files use the shared `kcpu_id_map::text` codec with the
`LineRecord`-captured runtime domain; the domain is validated before the stable
inodes are created, so uninitialized/empty present state fails closed. CPU text
errors are mapped by procfs's shared `cpu_text_error` adapter, not by IRQ policy
code. The current present-only domain does not imply Linux possible/hotplug
coverage. The generic
backend owns fallible per-open generation storage, bounded complete copying,
one scalar callback, seek handling and readv pinning. Write offset is ignored;
128-byte overflow returns `FileTooLarge`; empty commands reach the parser and
return `EINVAL` for settable Lines. The policy precheck runs before parsing/size
checks, so unsupported/fixed/owner-restricted Lines return `EPERM` for malformed
or oversized commands too; read-side unsupported/fixed observations remain
`EOPNOTSUPP`. KIRQ's checked `set_user_affinity` is the only mutation path; a
callback error is not retried as a hardware rollback. Effective files are
read-only and use the same lazy `SeqFileInode` read validation, not eager
`SimpleFile` snapshots.

VFS DAC remains the open gate; root/capability cannot bypass KIRQ policy. No
KIRQ-to-VFS dependency, controller dependency or second IRQ registry is
introduced. Old open files retain a historical Line handle and cannot follow a
reused Virq; fresh namespace lookup revalidates current KIRQ identity. The
retirement test exercises the production MSI free/notification path, including
index removal before the observer checks the namespace.

## Superblock-owner failure

Private-owner allocation failure returns NoMemory through get_tree before any
procfs root is published. Existing Arc, inode and snapshot allocations remain
outside this fallible boundary. Constructor callers must propagate the Result
rather than turning ordinary mount-time memory pressure into a panic.

Tracing loss counters in `tracing/capture_stats` expose dropped records without
logging from IRQ/scheduler callbacks. Reading does not reset counters or mutate
trace configuration. Each open is coherent on its own, not with an independently
opened `tracing/trace` snapshot; consumers must not infer losslessness from the
trace header alone.

## Fallible directory iterators

Iterator storage failures propagate as NoMemory through directory iteration;
empty ZST iterators do not allocate storage. Vec/String snapshots and lazy PID
or TID formatting retain their existing infallible allocation behavior. No
snapshot guard survives into the KBox allocation; future callers must preserve
the contract even though the current allocator does not perform reclaim.

Named character devices are read from KVFS's registration snapshot; procfs
does not claim a fixed tty exists when registration failed. A snapshot can
become stale during concurrent removal, as can hardware topology output; the
next rewind refreshes it, and this text view grants no device access.


## Trust Model

Procfs accepts filesystem operations through `kvfs` and publishes kernel-owned
information. It trusts `kvfs` for path resolution, credentials, generic access
checks, buffer validity, and open-file serialization; it trusts `kprocess` and
`memspace` for task/resource lifetimes, coherent MM capture, and VMA invariants.
Output is diagnostic data and is not an authorization decision or proof that
separately read process fields describe the same image.

## External Boundaries and Attack Surface

Source paths below are relative to `fs/filesystems/procfs/` unless stated otherwise.

`sys/kernel/pty/max` and `sys/kernel/pty/nr` are read-only observations of `ktty`
state. Their `0444` mode is not the sole write barrier: the `SimpleFile` closure
backend rejects writes even when a privileged opener bypasses mode checks.
Reading does not reserve a slot or keep a pair alive; the reported reservation
count can change immediately after observation and grants no admission guarantee.

- Filesystem names, PID/TID/FD components, offsets, and write bytes are external
  inputs. Numeric lookup components are parsed, task-directory membership is
  checked, and unsupported names fail lookup (`src/task_nodes/root.rs`).
- Writable controls include `comm`, `oom_score_adj`, trace event enables,
  `sys/kernel/printk`, `sys/kernel/log_filter`, the three
  `sys/fs/inotify/{max_queued_events,max_user_instances,max_user_watches}`
  limits, optional `sysrq-trigger`, and the per-task `uid_map`, `gid_map`
  (`0666`) and `setgroups` (`0644`) files. The map files accept at most
  `ID_MAP_MAX_EXTENTS` decimal `inside outside count` lines and are write-once;
  `IdMapFile::write` authorizes through `kcred::new_idmap_permitted`. The
  `setgroups` file accepts `allow`/`deny` and is gated at open-for-write by
  `CAP_SYS_ADMIN` in the target user namespace; `deny` is irreversible. `log_filter` accepts one filter spec
  (`[<level>] [<target>=<level>]...`, at most `MAX_FILTER_RULES` rules with
  `MAX_FILTER_TARGET`-byte targets); a write is privilege-gated and either
  replaces the whole spec or fails with `InvalidInput` and leaves the previous
  filter in place. The inotify limits are `0644`; their inode permission hook
  requires `euid == 0` to open for write, and their write closure passes the
  writing task's current credential (not the opener snapshot), requires
  `euid == 0`, accepts Linux's base-0 integer syntax in `0..=i32::MAX`, and
  leaves the previous value untouched on any parse or privilege failure. `comm`
  truncates to 15 bytes and validates C-string/UTF-8 conversion; OOM adjustment
  accepts only parsed values in `[-1000, 1000]`. Tracing delegates validation
  to `ktracing`; SysRq accepts the first non-whitespace byte only when it is
  `t`. `sys/kernel/hostname` and `sys/kernel/domainname` are also writable:
  both are `0644` and their write path requires `kprocess::current_cred()
  .has_root_euid()` (Linux `test_perm()`'s GLOBAL_ROOT_UID check, not a
  capability) and fails with `OperationNotPermitted` otherwise. An accepted write replaces the
  caller's UTS namespace value following Linux `proc_dostring` strict
  semantics — it starts at the file position, stops at the first NUL or
  newline, truncates the stored value there, and silently accepts a write that
  starts past the current value. Reads report the namespace bytes verbatim plus
  one trailing newline; both the read and write paths return `NotFound` for a
  kernel-task caller because it has no UTS namespace of its own.
- FD magic links expose resource-backed paths. `ProcFdLink::follow` rejects
  magic-link-disallowing flags or lack of final-component following before
  obtaining a descriptor snapshot. Descriptor reuse can change a later result.
- Task names, command lines, executable/VMA paths, mount data, trace records,
  and optional TA metadata can carry sensitive or externally influenced text.
  `status` escapes line feed and backslash in `Name` so a task cannot forge an
  extra status line; `stat.comm` is raw, as in Linux, and parsers must split on
  the final `)` rather than treating the line as delimiter-free.
- Procfs operates on Rust slices and metadata, not raw user pointers or mapped
  user payload. It has no direct MMIO/PIO, DMA-buffer, FFI, assembly, or interrupt
  handler boundary. Boot command lines, device topology, and hardware statistics
  arrive through provider APIs; RISC-V counter access is delegated to `riscv`.

## Unsafe Inventory and Memory Safety

There are no local `unsafe` blocks/functions or manual unsafe `Send`/`Sync`
implementations in `src/` or `ut-tests/`. Safety still depends on provider contracts:

- `MapsIter` retains a `LiveAddressSpace` active MM user, not merely a structural
  reference. `ProcessRuntimeState::address_space` in repository-relative
  `process/kprocess/src/process_runtime/runtime_state.rs` clones the MM user and
  matching heap handle inside the same binding critical section.
- VMA access occurs under that capability's MM mutex. Entries own copied values
  and paths, so formatting does not retain references into an unlocked VMA list.
- `LiveAddressSpace::heap_top` in
  `process/kprocess/src/process/runtime_access.rs` performs an Acquire atomic
  read without locking the current process binding. Last-user capability drop
  may lock and clear the MM; callers must release it in sleepable context,
  outside the MM mutex and spinlocks.
- Weak task references are upgraded before access. A successful task upgrade
  does not imply that process runtime resources remain attached; accessors can
  still fail and their errors must be propagated.

## Task-directory Metadata Boundary

Task-directory nlink comes from kernel-owned membership of its retained target,
never from a caller-supplied count or an unrelated current process. Successful
weak upgrade retains that `Process` during the query; failed upgrade reports the
two base directory links. This does not add user-memory access, unsafe code, a
global task scan, or a new lock.

The query uses the existing membership-table and member-slot locks after the
simple node's metadata lock has been released. Per-slot publication can
interleave with the count scan. The returned value cannot authorize access,
prevent clone/exit, or guarantee that a later task operation sees the same set.
Existing visibility, credential, and maps-lifetime rules remain independent.

## Thread Safety

Status credential columns describe the retained target thread, using one
objective `Cred` snapshot for both UID and GID. No reader-credential fallback is
used when a target disappears: existing weak-upgrade failure remains `ESRCH`.
An intentionally retained task can still expose its last credential snapshot.
The status iterator keeps only a weak task reference. A record already generated
by `SeqFile` is retained until consumed; short reads cannot mix old UID columns
with new GID columns. Rewind regenerates the record, and a reclaimed target then
returns `ESRCH` through `SeqIterator::try_show`. Opening does not pin the task or
freeze credentials for the lifetime of the descriptor.
The snapshot is diagnostic information, not authorization for a later action;
existing VFS permissions and the absence of local ptrace-style filtering remain
unchanged. Only initial-user-namespace numeric IDs are currently supported.

`fs/kvfs/src/seq_file.rs` (repository-relative) requires `SeqIterator: Send` and
serializes each open iterator with a mutex. Procfs provides no extra unsafe
thread-safety implementation. MM capture releases the binding lock before the
MM mutex is acquired. The heap's Acquire/Release operations preserve live
same-image updates but do not create a transaction over all procfs fields.
Independent opens, weak-reference callbacks, and mount/FD lookups may observe
different moments.

## Threat Analysis

| ID | Threat | Severity | Trigger | Existing controls and residual risk |
| --- | --- | --- | --- | --- |
| T-01 | Cross-image maps/heap mismatch | Medium | Binding changes after a reader captures its MM | Single-lock MM/heap capture and capability-local heap reads retain the pair. Contents remain live. |
| T-02 | Stale task or torn-down MM access | High | Exit races with lookup, open, or read | Weak upgrades, fallible resource access, active MM-user retention, and locked VMA copies guard access. Retained views intentionally prolong mapping lifetime. |
| T-03 | Information disclosure or unwanted controls | High | An insufficiently restricted caller reaches process/diagnostic nodes or writes controls | VFS permissions apply; `printk` and `log_filter` both require `file.cred().euid() == 0` on write. Task reads are partly covered: `/proc/<pid>/fdinfo/<fd>` gates every read on `kprocess::ptrace::check_read_fs_creds_access` (`PTRACE_MODE_READ_FSCREDS`: same-thread-group pass, otherwise the caller's fsuid/fsgid against the target's real/effective/saved ids and the target's permitted capabilities against the caller's effective set, with an effective `CAP_SYS_PTRACE` override) and fails with `EACCES`; the remaining task callbacks still have no local privilege check and require separate audit. The fdinfo gate runs in the read closure because access depends on the target task and descriptor identified only after path parsing. |
| T-04 | Resource pressure and lock contention | Medium | Many retained maps opens, large VMA lists, or repeated rewinds | Ownership cleanup releases buffers and MM users, but procfs imposes no local quota on retained views or scan work. |
| T-05 | Ambiguous diagnostic text | Low | Names or paths contain separators/newlines | Mount sources are mangled; maps paths and several other strings are emitted verbatim. Consumers must not treat all output as escaped records. |
| T-06 | Zombie `exe` link resolves after exit | Low | `/proc/<pid>/exe` read while the target is a zombie | Read/follow/readlink require the target thread to be live and its process not exited, so a zombie's link reports `ENOENT` like Linux; `getattr` still succeeds with size 0. |

## Failure Modes and Effects (FMEA)

| ID | Failure | Cause | Local effect | System effect | Severity (1-4) | Handling |
| --- | --- | --- | --- | --- | --- | --- |
| F-01 | Target unavailable | Task reclaimed or runtime detached | Lookup/open/read fails, or directory enumeration is empty | Observation unavailable | 4 | Return `NotFound`/`NoSuchProcess` as appropriate; do not retarget retained maps |
| F-02 | Invalid command | Bad encoding, range, or unsupported command | Write rejected | State remains unchanged on those validation failures | 4 | Return `InvalidInput`; unauthorized printk returns `OperationNotPermitted` |
| F-03 | Allocation pressure | Large generated output or many retained views | Allocation/latency pressure | Kernel allocator failure policy applies | 2 | No procfs allocation recovery or admission limit |
| F-04 | Invalid release context | Last MM-user drop under a conflicting lock or in non-sleepable context | Teardown blocks or violates context requirements | Potential hang/failure | 2 | Respect capability drop contract; release outside binding/MM locks |

## Failure Handling

Ordinary lookup, resource-access, and parsing errors use `VfsResult`. Missing FD
targets are translated from `BadFileDescriptor` to `NotFound`. Mountpoint
rendering falls back to `<mount:name>` if absolute-path rendering fails; missing
TEE runtime data may render empty output. Seq-file formatting errors become
`Io`. Registration uses `expect` for the register-once invariant; formatting
unwraps rely on generated ASCII permission bytes or infallible String formatting.
There is no module-level retry, rollback, or panic recovery protocol.

## Privacy and Known Limitations

Maps exposes addresses and paths; task nodes expose command lines, names, and
executable/FD information. Tracing, topology, and optional TA information expand
the disclosure surface. These files must be included in the system's visibility
policy; absence of raw user-memory reads does not make their output non-sensitive.

The retained maps capability preserves MM/heap identity, not a frozen image or
a transaction spanning procfs reads. Same-image changes remain observable on
recollection. Retention can outlive task exit and delay last-user MM cleanup;
Linux-exact lifetime compatibility is not promised. Exec replacement,
hardware-root switching and shared-brk policy are outside procfs. MM has no
resident-page accounting, so `status` omits `VmRSS` and `stat.rss` is a fixed
positional placeholder rather than a disclosure of resident pages. Placeholder
statistics, single-node `Mems_allowed*`, and namespace identities are described
in `design.md`.

## Audit Checklist

- Keep MM-user and heap-handle capture inside one binding critical section;
  never independently query the current process heap from `MapsIter`.
- Keep `heap_top()` safe under the captured MM mutex and preserve Acquire reads.
- Check retained old/new image pairing, same-image heap visibility, exit/drop
  lifetime, existing fork policy, and actual maps heap labels in regression tests.
- Check release context and avoid lock recursion or final MM-user drop under
  binding/MM locks.
- Review credential enforcement and magic-link flags when adding entries or
  controls; preserve lookup-time versus operation-time error distinctions.
- Treat text escaping, retained-view resource costs, and feature-gated outputs
  as explicit review concerns; do not broaden snapshot or compatibility claims.
- Check live task-directory link counts through stat/fstat/fstatat, inherited
  descriptor target identity, and the two base links after target disappearance.
- Keep member-count queries outside the simple node metadata lock and preserve
  per-slot observation limits; do not claim an atomic group snapshot.

### Unreleased leader lifetime

Task-directory nlink queries the target Process's member lifetime, including an
exited leader before wait/autoreap. It does not infer a count from zombie flags,
force a minimum of one, or expose an exited task to lookup/scheduling. Reap
releases retained membership independently of remaining Arc observers, and a
Weak-only directory cannot extend that identity's ownership. Publication can
interleave with the slot scan; this remains an advisory metadata snapshot.

A successful non-leader `exec` takeover drops the retained leader slot only
after kprocess has moved canonical publication and signal routing onto the
survivor; the procfs count observes that final membership and never creates an
accounting-only leader alias. A failed takeover leaves the leader slot intact.
The advisory count still cannot authorize a TID or prove that a concurrent
exec transition has completed; it remains a metadata snapshot.

## Quota introspection

Quota cache counts are read-only observations under each superblock's existing
quota mutex. No backing-file I/O or native filesystem lock is taken while
counting. Mount-option snapshots use the filesystem operation table; build
configuration bytes are produced from the resolved build inputs, not runtime
user data. All three introspection paths expose state rather than accepting
policy changes.

## IPv4 forwarding control

Every write checks the current caller's `CAP_NET_ADMIN`, including on inherited
descriptors. The inode retains the namespace captured at lookup, so an open
descriptor does not retarget after `setns`. Namespace-dependent dentries are not
cached across lookups. A missing stack returns an error rather than falling back
to the global stack.

`/proc/net/ip_tables_names` lists the built-in IPv4 tables exposed by knet,
with mode 0440 and no write operation. Table enumeration is global, like the
current legacy table adapter; independent namespace rule stores are unsupported.
