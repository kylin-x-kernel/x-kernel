# devpts — Security

## Trust model

`devpts` runs inside the kernel and trusts the KVFS core it builds on: mount
dispatch, pathname resolution, DAC permission checks, and dentry lifecycle
are owned by `kvfs`. The crate's own surface is small — mount-option parsing,
per-instance pair bookkeeping, and two inode types (`ptmx`, slave) — and is
written with `#![deny(unsafe_code)]`: no `unsafe` blocks exist in this
crate.

## Input boundaries

### Mount data

The entire mount data byte string is an untrusted input parsed in
`MountOptions::parse` (`src/lib.rs`):

- Recognized keys are `uid=`, `gid=`, `mode=`, `ptmxmode=`, `max=`,
  `newinstance`; values read base 0 (`uid`/`gid`/`max`, so `010` is octal 8
  and `0x10` hexadecimal 16) or octal masked with `S_IALLUGO`
  (`mode`/`ptmxmode`), each tolerating exactly one trailing newline.
  The VFS-level sb-flag tokens `vfs_parse_sb_flag()` consumes are accepted
  (`ro`/`rw` land on the superblock read-only flag; the rest are carried by
  the mount flags).
- Unknown keys, keys without `=`, non-UTF-8 data, and malformed values are
  rejected with `EINVAL` before any state is created. `max=` is parsed and
  range-checked exactly like Linux (accepted including `max=0` and the
  negative-zero spellings, `EINVAL` only when out of range) but cannot bound
  an instance: PTY numbers live in one global allocator.

### Path resolution

The root directory resolves exactly two kinds of names: `ptmx` and the
decimal numbers of this instance's own pairs. Resolution is owner-scoped:
`lookup_child` calls `ktty::tty::pty_slave_inode_for(&self.fs, number)`,
which answers only when the pair's recorded owner is this filesystem, so a
number recycled to another instance after a liveness check cannot leak that
instance's slave inode. Names that are not `ptmx` and do not parse as `u32`
return `ENOENT`.

## Isolation assumptions

Per-mount isolation is a data-structure property of this crate: each mount
gets its own superblock and instance record, and only pairs created through
that instance's `ptmx` are listed. Pty **allocation** itself is global
(`io/ktty`'s allocator with capacity `pty_limit()`), so containers share the
host pty budget; `max=` is rejected rather than pretending per-instance
bounds exist. Device access behind a loaded bpf program is not enforced
anywhere in the kernel today (see `core/ksyscall/src/bpf.rs`).

## Residual risks

- `ptmxmode=` defaults to `0000` like Linux; a mount that does not set it
  leaves `ptmx` inaccessible to non-root callers by design.
- Ownership falls back to the acting task's fsuid/fsgid; the credential
  model has no capability sets yet, so euid-0 proxies apply as elsewhere in
  the kernel.
