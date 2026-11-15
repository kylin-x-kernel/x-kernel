# cgroup2fs — Security and Reliability Analysis

## Superblock-owner failure

NoMemory from SimpleFs propagates through the mount context without publishing a
root or cgroup node. The initial boot-only mount retains its fatal-boot boundary.
This owner conversion does not make cgroup node, Arc or namespace allocations
fallible. New callers must not wrap the constructor in hierarchy/reclaim locks.

## Fallible directory iterators

Iterator allocation failure propagates NoMemory and drops the name snapshot
and operation reservation. Existing Vec/String snapshot allocations remain
outside the fallible-owner guarantee. Holding a lifecycle reservation does not
authorize holding hierarchy or reclaim-required locks over child_names.

## Trust Model

The VFS copies the written content and enforces pathname/inode DAC. `cgroup2fs` still treats all commands as untrusted input;
`kcgroup` is responsible for state invariants, `kprocess` is responsible for stable task identity and migration transactions, and the adapter uses
VFS DAC, the open-file credential and the mount view to perform authorization.

## External Boundaries / Attack Surface

- A normal mount uses the current process's cgroup namespace view root; the boot mount uses the initial view.
- `mkdir`/`rmdir` names, control file contents and `cgroup.procs` PIDs come from user space.
- This crate does not touch user pointers, MMIO, DMA, FFI, interrupts or firmware data.

## unsafe Code Inventory

None. The crate uses `#![deny(unsafe_code)]`.

## Memory Safety Invariants

A file closure pins its target through `Arc<Cgroup>`. The per-mount registry pins directory and control file inodes;
the tombstone of a deleted node is not released until unmount, and re-creating a node with the same name does not reuse the old identity. Controller file visibility
is decided from the canonical state. Process enumeration must compare membership against the same
`Arc<PidHandle>` as the registry task, and must not rely on the numeric TID alone.

## Thread Safety

The node registry is protected by a sleepable mutex. All hierarchy transactions and operation guards enter the
`kcgroup` transaction lock; callers must not enter from IRQ context.

## Threat Analysis

| ID | Threat Description | Impact Level | Trigger Condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | A wrong hierarchy view is exposed by a mount | High | A normal mount falls back to the global root, or the boot mount creates a second hierarchy | A normal mount uses only the current view; the boot mount shares the initial namespace with init |
| T-02 | Control command injection or ambiguity | Medium | Non-UTF-8 input, NUL, empty operations or unknown controllers | Complete parsing; all names are validated before mutation |
| T-03 | TID reuse shows the wrong process | Medium | A numeric TID maps to a new task | `kprocess` uses `Arc::ptr_eq` on the stable identity |
| T-04 | Ambient credential inconsistent with the fd opener | High | Reading the current task in reverse at write time, or stacking an administrator shortcut | Directory mutations use the VFS credential; commands use `VfsFile::f_cred`, with no ambient administrator check |
| T-05 | Unauthorized migration from a delegated view | High | Source/destination is outside the view, or only the destination is checked | Both ends are verified visible inside the process gate, and the common ancestor's `cgroup.procs` is checked to be writable by the opener |
| T-06 | Deletion/same-name re-creation makes an old fd operate on a new node | High | Lookup rebuilds the inode or rmdir re-looks up by name | Stable registry, locked victim identity; operations on the old node return `ENODEV` |
| T-07 | A command is concatenated by offset or split into multiple mutations | Medium | Reusing seekable merge or the default write_iter chunking | `CommandFile` fully copies a single request of at most 4096 bytes and ignores the offset |
| T-08 | `pids.max` conflicts with the unlimited sentinel | Medium | Accepting a `usize` outside the PID domain | Accept only `<= 4*1024*1024` or the text `max` |
| T-09 | Privilege escalation into a cgroup via clone3 using an inherited fd | High | Authorizing with the fd opener's credentials, or checking only the destination | `cgroup_clone_target` uses the caller's **current** credentials to require `MAY_WRITE` on both the target's and the common ancestor's `cgroup.procs`, with both endpoints under the mount view root; out-of-view returns `ENOENT`, missing write permission `EACCES`, and a removed target `ENODEV` |

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Failure Cause | Local Effect | System Effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | A control file returns an error errno | Owner API failure | A single write fails | The transaction remains consistent | 3 | Propagate the typed `VfsResult` |
| F-02 | A deleted task appears in `cgroup.procs` | Enumeration races with exit | Identity lookup fails | The entry is skipped | 4 | The facade uses a snapshot and identity validation |
| F-03 | Controller file visibility changes | Activation races with lookup | One lookup returns `ENOENT` | The caller can retry | 4 | Every lookup/readdir decides from the canonical state |
| F-04 | Remove races with I/O on an old fd | The operation does not pin the lifecycle | A command acts on a tombstone | Unauthorized state modification | 2 | The operation guard blocks a concurrent remove, and `ENODEV` is returned after remove |

## Failure Management

A parse failure returns `EINVAL`, an unknown file returns `ENOENT`, a DAC failure returns `EACCES/EPERM`, and a deleted
node returns `ENODEV`. Busy, cross-hierarchy and counting errors from the owner API are propagated directly, and are not masked by a panic
or fallback.

## Privacy Analysis

The module exposes only root-namespace PIDs within a cgroup and controller statistics, and does not read address spaces or file contents.
PID namespace translation is not yet implemented, so namespace isolation must not depend on this interface.

## Known Limitations

- Non-zero `cgroup.procs` IDs are still resolved in the root PID namespace.
- Implemented controller surfaces are `pids`, `cpuset`, `cpu`, and `memory`; `io` is not implemented.
- User namespace ID mapping, capabilities and LSM are not fully wired in; delegation currently relies on the inode
  owner/mode, the mount view and VFS DAC.

## Audit Checklist

- Does a new controller wire into both the state file declarations and the controller registry?
- Does a new write path use `CommandFile`, the opener credential, a uniform mode and complete input validation?
- Does it avoid directly accessing the scheduler, the procfs registry or credential internals?
- Does task display continue to verify the stable identity?
- Do the normal mount and the boot mount each maintain the current/initial view boundary?


## Controller files

`memory.*` and `cpu.*` writes are untrusted control input. Values are parsed
as bounded integers; invalid domains are rejected before touching controller
state. Memory accounting ids are kernel-owned, never read from userspace.
`cgroup.procs` migration authorizes the source and common ancestor file
credentials before membership changes; a post-migration affinity failure is
reported without claiming the migration was rolled back.
