# devpts — Design

## Purpose

`devpts` provides X-Kernel's devpts pseudo-terminal filesystem. Every mount
is its own instance: it exposes the Unix98 master multiplexer `ptmx` and
publishes the slave nodes of the pty pairs allocated **through that
instance's `ptmx`** inside the same filesystem, so the
`open("/dev/ptmx")` → `open("/dev/pts/<n>")` round trip completes within the
mount — the shape runc's default OCI spec expects when it mounts `devpts`
with `newinstance` at `<rootfs>/dev/pts`.

## Responsibilities

- Parse mount options with Linux `fs/devpts/inode.c` semantics: `uid=` /
  `gid=` (pty node ownership, falling back to the acting task's
  fsuid/fsgid), `mode=` (slaves, default 0600), `ptmxmode=` (the ptmx node,
  default 0000), and `newinstance` (inherent — every mount is a fresh
  instance).
- Maintain per-mount instance state: record the pty pairs created through
  the instance's `ptmx` (weak references); the root directory enumerates and
  resolves only pairs that are alive and published.
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
  owned by the filesystem of the `ptmx` node that was opened, so
  `open("/dev/ptmx")` → `open("/dev/pts/<n>")` closes only when that
  `/dev/ptmx` resolves into this mount — the OCI default has `runc` symlink
  `/dev/ptmx` to `pts/ptmx`, which does. Opening the devfs `/dev/ptmx` node
  instead allocates a devfs-owned pty that appears in the host `/dev/pts`, not
  in a container's instance. Linux has no such rule: `ptmx_open()` →
  `devpts_acquire()`/`devpts_ptmx_path()` resolve the target instance from the
  ptmx file's *path* (the sibling `pts` mount), so any `/dev/ptmx` node reaches
  the same devpts instance. Implementing that needs path-based resolution,
  which this instance-keyed model does not do yet.
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
