# procfs - Design

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
| `src/task_nodes/root.rs` | PID/TID directories, task metadata, maps, FD magic links, namespaces, process controls |
| `src/task_nodes/mounts.rs` | Mount-tree traversal and mounts/mountinfo/mountstats formatting |
| `src/task_nodes/tee.rs` | Feature-gated TA metadata through `tee_task_iface` |
| `src/basic_nodes/root.rs` | Boot command line, CPU/filesystem information, kernel settings |
| `src/mem_nodes/root.rs` | Allocator-derived memory statistics |
| `src/device_nodes/root.rs` | Device, bus, driver, and topology views through `kdevice` |
| `src/trace_nodes/root.rs` | Trace records, event metadata and enable controls through `ktracing` |
| `src/sysrq_nodes/root.rs` | Optional task-backtrace trigger |
| `src/irq_nodes/root.rs` | Empty registration stub; no interrupts file yet |

`src/root.rs` also gates lock statistics, scheduler statistics, and VM diagnostics
on Cargo features. `kvfs` owns dentries, inodes, permissions, open-file state,
buffering, and offsets. `kprocess` owns process resources and live-MM capture;
`memspace` owns VMA metadata and MM-user teardown rules. Procfs consumes these
interfaces without introducing a separate MM lifecycle.

`sys/kernel/pty/max` and `sys/kernel/pty/nr` expose decimal values followed by
a newline through read-only `SimpleFile` closures with mode `0444`. The `ktty`
provider owns both the fixed limit and reservation accounting. `nr` includes
unpublished reservations and pairs retained by surviving slave opens; it is not
a count of visible pts entries. Each read samples the provider without retaining
a PTY resource. Unlike Linux's writable `pty/max`, this interface reports a fixed
limit of 16 pairs and does not support runtime resizing.

## Calling Constraints and Execution Context

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
are rendered as `00:00`. Several status/memory fields and non-cgroup namespace
identities are placeholders. Mount reporting uses synthetic device identities
and is not a globally frozen namespace snapshot. `printk` uses X-Kernel log-level
values, not Linux console-loglevel semantics.

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
