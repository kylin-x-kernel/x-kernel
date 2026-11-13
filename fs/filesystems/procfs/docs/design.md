# procfs - Design

## CPU information

`src/basic_nodes/cpuinfo.rs` formats `/proc/cpuinfo`; `root.rs` only registers
the read-only SimpleFile. Each present logical CPU retains its original ID and
one blank-line-separated section. The legacy `cpu cores` field still means total
present logical CPUs, not physical cores per package; `architecture` is retained.
These compatibility fields and present-CPU enumeration are not a claim of full
Linux topology/hotplug compatibility (Linux normally enumerates online CPUs).

On x86_64, procfs reads `kcpu::cpu_info` snapshots, adding `vendor_id`, `cpu family`,
`model`, `stepping`, `model name`, and `flags`. A present CPU that has not completed
identification has unknown identity and an empty flags line, never another CPU's
snapshot. No CPUID instruction runs during file reads. Linux v6.12
[cpufeatures.h](https://github.com/torvalds/linux/blob/v6.12/arch/x86/include/asm/cpufeatures.h)
provides the names, including `pni` for SSE3. The table is a subset: absent names
do not prove hardware absence. CPU/OS support filtering belongs to kcpu, not
procfs. AVX/XSAVE-related flags require kcpu's enabled state contract as well
as the corresponding hardware bits; FXSAVE fallback CPUs omit them.

SimpleFile retains its generated-file partial-read behavior: each read regenerates
the text and slices it at the supplied offset. CPU snapshots are immutable after
bring-up, but opening during AP identification can observe changing output.

All architectures consume the same `kcpu::cpu_info(LogicalCpuId)` entry point:

- AArch64 prints Linux `Features`, `CPU implementer`, `CPU architecture`,
  `CPU variant`, `CPU part` and `CPU revision` fields from its register snapshot.
- RISC-V prints the selected ISA subset as a Linux `isa` string plus optional
  firmware `mvendorid`, `marchid` and `mimpid`. Base letters remain contiguous;
  multi-letter extensions have underscore separators. Unsupported vector state
  is omitted. Firmware identification may precede AP startup; absent snapshots
  have an empty ISA line and unknown IDs.
- LoongArch prints `CPU Family`, `Model Name`, `CPU Revision`, `ISA` and
  `Features`. CPUCFG has no marketing name, so `Model Name` identifies the raw
  PRID explicitly. Unknown implementers are not relabeled as Loongson.

Linux v6.12's per-architecture cpuinfo implementations define these field names.
Snapshots and usable-feature filtering belong to kcpu; procfs only selects
presentation names. Feature tables are conservative subsets, not complete Linux
cpuinfo implementations. In particular, arm64 Linux normally reports the common
HWCAP intersection, while these fields describe each CPU's snapshot. ELF
AT_HWCAP/AT_HWCAP2 remains independent and is not changed by this procfs adapter.

Regression tests in `ut-tests/cpuinfo.rs` check section identity, naming and
unsupported flags; `basic_nodes_root.rs` exercises short VFS reads and write
rejection. kcpu tests inject CPUID values to cover positive and negative feature
cases without depending on the test host's CPU model. Cross-architecture tests
check field presence, unknown identities, feature spelling uniqueness and
state-policy filtering; common kcpu tests cover sparse snapshot slots.


## Fallible superblock preparation

Procfs constructors return VfsResult and use SimpleFs's KERNEL allocation for
mount-private ownership before building root nodes. They require sleepable task
context with IRQs/preemption enabled, outside non-sleepable RCU and without locks
needed by reclaim, I/O or filesystem callbacks.

`/proc/tracing/capture_stats` is a read-only per-open text snapshot of ktracing's
retained, overwritten, oversized and uninitialized record counters. Reopening
refreshes it without initializing tracing. Formatting takes place outside the
capture spinlock; the trace snapshot ABI and header remain unchanged.

## Fallible directory iterators

Task, namespace, TEE and tracing directory child_names implementations return
KBox iterators with default KERNEL policy and propagate allocation failure.
Thread/process/tracing helpers return owned snapshots; fd-table read guards
end at the snapshot statement. Static-name and empty iterators retain no locks.
All allocations obey SimpleDirOps' sleepable-task and reclaim-safe-lock contract.

Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.

`/proc/devices` combines hardware topology records with snapshots of named
KVFS cdev registrations. Virtual tty name/major values come from successful
registrations, not hardcoded entries. Failed or removed cdevs are absent from
subsequent snapshots; the existing no-devices fallback is retained when both
sources are empty. Existing topology rows retain core DeviceRecord IDs; named
cdevs use a separate `Registered character devices (major:minor)` section with
explicit major:minor values. These identifier namespaces are not mixed in one
column. This is a snapshot view, not a held device lifetime.

## Purpose and Background

`procfs` is a `no_std` virtual filesystem exposing process information, kernel
statistics, and selected control files through `kvfs`. It formats subsystem-owned
state; it does not own process lifecycle, address-space replacement, mapping
policy, or hardware activation. `src/lib.rs` registers the device-less `proc`
filesystem and constructs a `SimpleFs` superblock with magic `0x9fa0`.

## Scope and Architecture

Paths in this table are relative to `fs/filesystems/procfs/`.

| Source | Responsibility |
| --- | --- |
| `src/lib.rs`, `src/root.rs` | Registration, superblock construction, composition of static and dynamic entries |
| `src/name.rs` | Strict canonical-decimal parsing of numeric proc entry names (Linux `name_to_int()`) |
| `src/task_nodes/root.rs` | PID/TID directories, task metadata, maps, FD magic links, fdinfo rendering (including inotify watches), namespaces, process controls |
| `src/task_nodes/mounts.rs` | Mount-tree traversal and mounts/mountinfo/mountstats formatting |
| `src/task_nodes/tee.rs` | Feature-gated TA metadata through `tee_task_iface` |
| `src/basic_nodes/root.rs` | Boot command line, root entry registration, filesystem information, kernel settings and the `/proc/sys/fs/inotify` live limits |
| `src/basic_nodes/cpuinfo.rs` | Per-CPU text formatting and Linux feature-name table over kcpu snapshots |
| `src/mem_nodes/root.rs` | Allocator-derived memory statistics |
| `src/device_nodes/root.rs` | Device, bus, driver, and topology views through `kdevice` |
| `src/trace_nodes/root.rs` | Trace records, event metadata and enable controls through `ktracing` |
| `src/sysrq_nodes/root.rs` | Optional task-backtrace trigger |
| `src/irq_nodes/root.rs` | Empty registration stub; no interrupts file yet |

Numeric entry names (`<pid>`, `<tid>`, `fd`, and `fdinfo`) are parsed by
`src/name.rs`, a port of Linux's `name_to_int()`. Only canonical decimal text is
accepted: signs, whitespace, `0x`, multi-digit leading zeroes, and values above
the `(u32::MAX - 9) / 10` guard do not resolve, so a path `ls` would not list
cannot be opened through a lax spelling.

`src/root.rs` also gates lock statistics, scheduler statistics, and VM diagnostics
on Cargo features. `kvfs` owns dentries, inodes, permissions, open-file state,
buffering, and offsets. `kprocess` owns process resources and live-MM capture;
`memspace` owns VMA metadata and MM-user teardown rules. `kfd_objects` owns
inotify instances and the per-euid instance/watch ledger behind the
`/proc/sys/fs/inotify/` limits, and supplies the watch snapshots rendered in
fdinfo. Procfs consumes these interfaces without introducing a separate MM or
file-object lifecycle.

Static directory mappings retain the inodes of their declared entries,
including the inotify sysctl nodes with custom permission operations. Repeated
lookups reuse those objects even when the dentry cache is disabled, preserving
file watches and content-event routing. Dynamic task and descriptor entries
continue to be materialized by their lookup callbacks.

`sys/kernel/pty/max` and `sys/kernel/pty/nr` expose decimal values followed by
a newline through read-only `SimpleFile` closures with mode `0444`. The `ktty`
provider owns both the fixed limit and reservation accounting. `nr` includes
unpublished reservations and pairs retained by surviving slave opens; it is not
a count of visible pts entries. Each read samples the provider without retaining
a PTY resource. Unlike Linux's writable `pty/max`, this interface reports a fixed
limit of 16 pairs and does not support runtime resizing.

### fdinfo and inotify controls

`/proc/<pid>/fdinfo/<fd>` and `/proc/<pid>/task/<tid>/fdinfo/<fd>` render the
generic `pos`, `flags`, `mnt_id` and `ino` lines in Linux's order. The read
closure revalidates the descriptor before formatting it and reports a
descriptor that disappeared after lookup as `ENOENT`, matching Linux's
read-time `files_lookup_fd_locked()`. It then runs
`kprocess::ptrace::check_read_fs_creds_access` for a user caller and fails with
`EACCES` when the `PTRACE_MODE_READ_FSCREDS`-style check denies access. An
inotify file appends one `inotify wd:... ino:... sdev:... mask:...
ignored_mask:...` line per live watch, using the instance's watch snapshot.

A pidfd appends Linux's `Pid:` line for glibc/systemd pidfd-to-pid resolution.
The value is the `pid_nr_ns` view: the process's number in the reading task's
active PID namespace, and `0` when it has no projection there. This procfs does
not model a per-mount PID namespace, so the reader's namespace stands in for
the procfs instance's; a kernel-task reader has none and falls back to the
internal number instead of panicking. A pidfd reports `-1` only once its process has been
reaped: Linux computes the number while the pid still has a task attached
(`pid_has_task`), which includes an exited child that has not been waited for,
so a waitable zombie keeps naming its pid and only the reaped state reads as
unidentifiable.

`/proc/sys/kernel/{hostname,domainname}` are `0644` nodes backed by the
caller's UTS namespace, so `uname(2)` and the sysctl always agree. Reads report
the namespace bytes verbatim plus one newline — the UTS owner accepts non-UTF-8
names and `uname(2)` returns them unchanged, so no decoding happens here.
Writes follow Linux `proc_dostring` under the default strict mode: they start
at the file position, stop at the first NUL or newline, truncate the stored
value there, and accept a write that begins past the current value as a
successful no-op. Both directions require a caller with a UTS namespace, so a
kernel task gets `ENOENT`, and writing requires `CAP_SYS_ADMIN` like
sethostname(2).

`/proc/sys/fs/inotify/{max_queued_events,max_user_instances,max_user_watches}`
are `0644` nodes with a per-inode permission hook that authorizes an open for
write on the opener's euid, matching Linux's `proc_sys_permission` rather than
generic DAC's fsuid. Reads sample the `kfd_objects` ledger. Writes pass the
writing task's current credential to the provider, which parses Linux's base-0
number format (`0..=i32::MAX`), rechecks `euid == 0` on every write, and mutates
the live limit only after validation succeeds.

## Calling Constraints and Execution Context

Task `status` renders UID/GID columns from one immutable objective credential
snapshot of the selected thread (`Thread::real_cred`), in real, effective,
saved-set, and filesystem order. The process leader and per-thread paths share
this iterator; neither substitutes the reader's credentials. Cloning the
credential `Arc` releases its read lock before formatting. The existing
`SeqFile` buffer generates one status record on first read, then serves its
remaining bytes across short reads. A zero-offset read regenerates the record;
opening alone does not capture credentials. No procfs credential cache is added,
and unrelated fields are not a globally atomic task snapshot. Current `kcred` IDs belong
to the initial user namespace; reader-namespace ID translation is not implemented.

`status` emits a Linux-compatible subset in Linux field order. `SigPnd`,
`ShdPnd`, and `SigBlk` come from one `ThreadSignalManager::masks()` snapshot, so
they describe one `SigLock` critical section; `SigIgn`/`SigCgt` need the separate
handler lock and may reflect a different instant. `VmSize` is the sum of VMA
lengths, not the fixed address-space window span; it is omitted when the task
has no live MM, while the positional `stat` field remains numeric `0`.
`Threads` uses the unreleased membership count, so an exited-but-unreaped group
leader still counts. `Cpus_allowed` and `Cpus_allowed_list` use the runtime CPU
count for the top-chunk width and list bounds; `Mems_allowed` and
`Mems_allowed_list` are the documented single-node constants.

`Name` escapes only line feed and backslash (`\n` and `\\`) so one task name
cannot inject an extra status line; other bytes pass through. Raw `stat.comm`
does not escape, matching Linux's `proc_task_name(..., false)` and allowing
parsers to split on the final `)`.

Regression fixtures publish inactive targets with distinct credentials, restore
reader credentials with a scope guard, and abort their owned publications on
drop. Tests cover root/non-root columns, all four ID roles, sibling isolation,
reader changes, both VFS status paths with short reads, and retained/reclaimed
target lifetime. A current-thread credential change between partial reads checks
record consistency, independent opens, and refresh after rewind. The fixture
restores credentials and does not activate additional workload threads.

Normal lookup, open, read, write, and release paths require task context with
allocation and subsystem initialization available. They can take sleeping locks
and are not interrupt-context or pre-scheduler early-boot interfaces. Filesystem
registration runs through the init hook after the registry is available.
`self` and cgroup-relative rendering require a current process thread; explicit
PID views use the selected task. CPU and device reporting depends on initialized
provider state; RISC-V `instret` uses the architecture counter accessor.

Separate opens have separate sequential iterators. Reads of one open sequential
file serialize through the `kvfs` private-state mutex. Recursive entry while
holding that mutex or the captured MM mutex is unsupported. Releasing a maps
iterator must be sleepable and outside spinlocks and the MM mutex because the
last MM-user release may tear down mappings.

## Task-directory Link Counts

`task_nodes::root::ProcessTaskDir` retains a weak reference to its selected
Process. Each getattr returns two base links plus the process-owned unreleased
member count (`kprocess::procfs::thread_group_member_count`). An exited leader
remains a member until wait/autoreap releases the identity, although TID lookup
and scheduler queries already exclude it. Thus an opened single-thread child's
directory reports 3 during its zombie-before-wait window and 2 after reap, even
if a strong Process observer remains. Inherited fds never switch to the caller.
Failed Weak upgrade also returns 2. No directory-layer exit-state adjustment,
extra counter or ownership of the target is introduced.

An exited leader remains a member until wait/autoreap releases the identity, or
until a non-leader `exec` takeover successfully moves the group identity onto
the survivor. That transition drops the retained leader slot in kprocess, so
the count stays one without any procfs compensation or count-only leader alias.
A failed takeover leaves the original leader slot authoritative. See kprocess
design.md for the identity-transition boundary.

The query scans stored slots under membership/slot spinlocks, with no allocation.
Publication can interleave; this is not an atomic group snapshot. Each getattr
incurs O(stored slots) work; no performance improvement is claimed. The distinct
scheduler facade intentionally counts published tasks and is unsuitable here.

The callback runs after the node metadata-copy lock is released. Dynamic nlink
changes only the returned getattr copy; cached metadata and directory enumeration
retain their existing inode/type semantics. The live fixture explicitly checks
its own TID is published (unittest_support uses start_user_task). A separate
single-owner identity exercises Weak failure. kprocess exit/reap definitions
check unreleased-member lifetime, including leader-before-sibling exit. kvfs
tests compare same-source metadata fields and document the existing block_size
normalization difference between getattr and metadata(). Definitions are not
execution evidence and await this head's CI/local regression.

The userspace ABI validation requirements remain synchronized counts 3 / 6 / 3,
inherited parent fd 6 versus child self 3, plus a pre-opened single-thread child's
fd reporting 3 after exit but before wait and 2 after wait. No sleep-based zombie
race is acceptable: use an explicit wait-without-consume observation or other
deterministic readiness signal before checking, then reap and check again.

## Maps Capture and Iteration

MM and heap capture is implemented by `MapsIter` in `src/task_nodes/root.rs`
and the following repository-relative provider paths:

- `process/kprocess/src/process_runtime/runtime_state.rs`:
  `ProcessRuntimeState::address_space` captures an active `MmUserHandle` and the
  corresponding heap `Arc<AtomicUsize>` under one process-binding lock.
- `process/kprocess/src/process/runtime_access.rs`: `Process::address_space`
  returns `LiveAddressSpace`; its `heap_top()` performs an Acquire load without
  reacquiring the binding lock.
- `fs/kvfs/src/seq_file.rs`: `SeqFileInode` creates the iterator at open time and
  manages subsequent buffered reads and rewinds.

1. Opening maps upgrades the weak task reference and calls `MapsIter::new`.
   Capture fails with `NoSuchProcess` if the task is reclaimed or its live MM
   is no longer available. The iterator stores only the capability, copied
   entries, and a cursor; it has no separately acquired heap handle.
2. `rewind` clears the entries and cursor, locks the captured MM, and copies its
   VMAs into `MapsEntry` values. Heap classification calls
   `self.aspace.heap_top()` while that MM mutex is held. File metadata is copied
   from VMA backends; stack and signal-trampoline labels use layout constants.
3. `next` clones a copied entry and `show` formats it after the MM lock is gone.
   Zero-offset reads and discontinuous offsets cause the seq-file adapter to
   rebuild the view; `start` also rewinds. Sequential continuation consumes the
   collected entries rather than refreshing every line.

## Concurrency Model

The binding lock establishes the identity of the MM/heap pair, then is released
before VMA iteration. The MM mutex protects the VMA walk; heap reads use Acquire
loads paired with the provider's Release stores. Procfs does not look up the
process's current heap while holding the captured MM mutex.

This is a retained identity with live contents, not an immutable image snapshot.
Same-image heap and mapping changes can appear on later collections. An atomic
heap load is not a transaction over all process metadata or even a promise of
one heap value across every copied row. A future binding replacement cannot
retarget an already captured capability to another MM or heap handle.

Other nodes have different observation boundaries: most task callbacks upgrade
weak references when invoked; mount iteration retains a root `Path` and walks
children incrementally; FD links resolve the numbered descriptor anew using a
resource snapshot. Dynamic task directories disable dentry caching. These
mechanisms do not make all procfs reads transactional.

## Decisions and Limitations

Keeping the active MM user and matching heap inside one capability prevents
cross-image heap labeling without adding another aggregate ownership object.
The existing per-process fork/`CLONE_VM` heap policy is unchanged. This contract
preserves a retained view independently of the process binding. Procfs implements
neither exec nor an MM-replacement operation. It does not claim Linux-exact
open-file or post-exit lifetime semantics.

Maps retains an O(VMA-count) copied list and clones path strings while holding
the MM mutex. Addresses use a minimum width of 12 hex digits and device numbers
are rendered as `00:00`. Status is a subset rather than Linux's full field set.
MM has no resident-page accounting, so `VmRSS` is omitted and `stat.rss` is a
positional `0` placeholder, not a measurement. `Mems_allowed*` are fixed
single-node values, not a rendering of Linux's `MAX_NUMNODES`-wide mask. Status
samples each subsystem in sequence and does not promise a global atomic
snapshot. Non-cgroup namespace identities remain placeholders. Mount reporting
uses synthetic device identities and is not a globally frozen namespace
snapshot. `printk` keeps its historical
single-value contract: it reads the console default level as a number in
X-Kernel's order (`0=off … 5=trace`) and a privileged write of a level name or
number changes that level, leaving the module filter untouched — a target with a
surviving override therefore keeps printing, and full silence comes from writing
`off`/`0` to `sys/kernel/log_filter`, which drops every rule. The filter spec
lives in `sys/kernel/log_filter` (format described in security.md); the value domain
is X-Kernel-private and *not* Linux's inverted `console_loglevel`.

`/proc/<pid>/ns/<type>` inode numbers come from per-subsystem ID allocators
(the `kcred` namespace allocator for mnt/net/ipc/uts/cgroup, the
`kidentity` PID-namespace counter for pid, and the fixed initial user
namespace for user). These counters are independent, so a collision across
subsystems is possible; Linux avoids it with one global nsfs inode space.
A shared namespace-inode registry is a prerequisite for nsfs-style magic
symlinks and bind-mount identity.

## Drop and Resource Release

Task dentries and ordinary task-file closures generally hold weak task/process
references, so retaining them does not itself keep a task alive. An opened maps
iterator instead owns an active MM-user capability and its paired heap handle:
process exit cannot perform last-user mapping teardown while that capability
survives. This does not prevent normal mapping changes in the retained MM.

Dropping the open-file state drops the iterator, copied entries, and capability.
The provider applies existing last-user cleanup rules without holding the
process-binding lock. Mount iterators release their retained `Path` and mount
references through ordinary ownership cleanup. Procfs adds no custom teardown
state machine.

## Quota and build introspection

`/proc/sys/fs/quota/allocated_dquots` derives its value from resident records in
the registered superblocks' canonical quota caches, without a second counter.
`/proc/config.gz` exposes the resolved build configuration generated by xconfig
as a deterministic gzip stream. Mount reports append each filesystem's
show_options output and expose the source block-device identity where present.
