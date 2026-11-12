# devpts — Design

Instance creation propagates the fallible SimpleFs private-owner allocation
through get_tree_nodev. KERNEL allocation requires sleepable mount context with
no reclaim-dependent locks held; it precedes root construction and publication.
Per-mount PTY owner identity and mount options are unchanged. Nested Arc and
container allocations retain their existing infallible behavior.

## Purpose

`devpts` provides X-Kernel's devpts pseudo-terminal filesystem. Every mount
is its own instance: it exposes the Unix98 master multiplexer `ptmx` and
publishes the slave nodes of the pty pairs allocated **through that
instance's `ptmx`** inside the same filesystem, so the
`open("/dev/ptmx")` → `open("/dev/pts/<n>")` round trip completes within the
mount — the shape runc's default OCI spec expects when it mounts `devpts`
with `newinstance` at `<rootfs>/dev/pts`. The conventional `/dev/ptmx` symlink
to `pts/ptmx` selects this instance through the mounted path.

## Responsibilities

- Parse mount options with Linux `fs/devpts/inode.c` semantics: `uid=` /
  `gid=` (pty node ownership, falling back to the acting task's
  fsuid/fsgid), `mode=` (slaves, default 0600), `ptmxmode=` (the ptmx node,
  default 0000), and `newinstance` (inherent — every mount is a fresh
  instance).
- Scope the per-mount view: pty pairs created through the instance's `ptmx`
  are recorded in `io/ktty` with this mount's filesystem as owner; the root
  directory enumerates and resolves only pairs owned by this mount that are
  alive and published.
- Provide the `ptmx` node: its `open` allocates through the shared ktty PTY
  driver and publishes the slave inode inside this filesystem.

## Non-responsibilities

`devpts` does not allocate pty numbers, implement terminal behavior, or own
the master/slave character-device drivers — number allocation and terminal
semantics belong to `io/ktty` (the global PTY allocator, capacity
`ktty::pty_limit()`); the host-side `/dev/ptmx` node and `/dev/pts` view
belong to `devfs`. Generic pathname, permission, and mount semantics belong
to KVFS.

## Concurrency model

Each mount's `MountOptions` lives in an `Arc<ksync::Mutex<MountOptions>>`
(created by `builder()`, published via `fs.set_private`, shared between
`DevptsPtmx` and remount's `devpts_reconfigure()`): a ptmx open reads the
current options (slave owner, modes) under the mutex, while a reconfigure
replaces them wholesale and immediately updates the live `ptmx` mode. The
mutex makes a reconfigure atomic with respect to subsequent slave creation.

## Known limits

- Pty numbers come from the global allocator, so `max=` is parsed and
  range-checked exactly like Linux (accepted including `max=0`, `EINVAL` only
  when out of range) but cannot bound an instance's allocation.
- **The `ptmx` that allocates a pty decides which view shows it.** A pair is
  owned by the filesystem of the `ptmx` node that was opened. devfs publishes
  `/dev/ptmx` as a symlink to `pts/ptmx`, so when devpts is mounted at
  `/dev/pts`, the path resolves into this instance and
  `open("/dev/ptmx")` → `open("/dev/pts/<n>")` completes within it. Before
  such a mount, the same symlink reaches devtmpfs's fallback `pts/ptmx`, whose
  pairs appear in devtmpfs's `/dev/pts`. Linux resolves the target from the
  ptmx file's *path* (`ptmx_open()` → `devpts_acquire()`/
  `devpts_ptmx_path()`); the symlink gives that conventional layout the same
  choice, while a character node placed at `/dev/ptmx` would still bind to the
  filesystem that created it.
- Mounts without `newinstance` still create a fresh instance: the host
  `/dev/pts` is devfs-owned, and this filesystem has no notion of a shared
  initial devpts instance.
- A slave inode is created and published when the instance's `ptmx` is
  opened; instances never see each other's ptys.

## Scope

```text
fs/filesystems/devpts/src/
├── lib.rs             # filesystem type registration, mount-option parsing,
│                      # instance construction
└── root.rs            # ptmx node, root directory lookup, slave nodes (the
                       # pair records themselves live in io/ktty, filtered by
                       # this mount's filesystem as their owner)
```

## Testing

`fs/filesystems/devpts/ut-tests/devpts.rs` covers the filesystem type name
and mount-option parsing (defaults, the runc-typical combination, `uid=` /
`mode=` masking, and rejection of unknown or unenforceable options).
