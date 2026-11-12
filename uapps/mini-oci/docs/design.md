# mini-oci design

## Purpose, scope, and background

`mini-oci` is a minimal guest integration launcher for a subset of OCI-like JSON,
not a complete OCI runtime. The independent Cargo workspace builds `mini-oci`
from `src/main.rs` and `oci-test-init` from `src/test_init.rs`. `serde` and
`serde_json` decode the configuration; `libc` and Unix `CommandExt` provide
process and mount operations. Kernel namespace, credential, cgroup, filesystem,
and exec enforcement remain external responsibilities.

## Interfaces and architecture

The command is `mini-oci run ID BUNDLE`. It reads `BUNDLE/config.json`, requires
`/sys/fs/cgroup/cgroup.controllers`, and creates `/sys/fs/cgroup/oci-ID`.
`Spec` contains a root path (a true `root.readonly` is rejected as unsupported), process, optional hostname, and mounts. Process
configuration supplies arguments, environment strings, cwd, user IDs/groups,
and optional fields that trigger rejection for capabilities or seccomp.
`no_new_privileges` uses that literal field name in the current deserializer;
this is not a claim of full OCI field-name compatibility.

```text
parent: parse -> create cgroup -> pipe2 -> clone -> migrate child -> ready byte
                                                       |                 |
child:                         close write end -> read ready <----------+
       -> hostname/policy -> mounts -> chroot -> cwd -> groups/IDs -> exec
parent: waitpid -> bounded cgroup removal -> exit with child status
```

## Execution context and algorithms

Use a single-threaded, privileged process in a disposable guest with the required
namespace and cgroup support. The fork-like raw `clone` has no memory-sharing
flag and passes a null child stack. Flags create mount, UTS, and IPC namespaces;
there is no new PID, network, user, or cgroup namespace. Do not add background
threads without reassessing post-clone Rust/libc operations.

The child waits for a byte before setup, so the parent moves it into the cgroup
before exec. It optionally sets the hostname and no-new-privileges, rejects
configured capabilities/seccomp, canonicalizes the root path, and makes mount
propagation recursively private. Supported mounts are bind, proc, and tmpfs;
bind `ro` uses a second remount call. Other options are not implemented.
Destinations must start with `/`; that test does not reject parent components
or symlink traversal.

After `chroot`, cwd is set, nonempty supplementary groups are applied, and all
real/effective/saved GID and UID values are set. An empty supplementary list
leaves inherited groups unchanged. The child clears the environment, splits
entries at the first `=`, and calls `Command::exec`. Success replaces the child;
an error is printed and the child exits with 127.

## Concurrency and lifecycle

Parent and child have separate address spaces. The close-on-exec pipe is their
only explicit synchronization. Each closes its unused endpoint; the remaining
raw endpoints rely on exec/process exit for closure. Parent `waitpid` blocks.
Successful teardown retries cgroup removal up to 100 times, sleeping 10 ms after
`EBUSY`. It returns the child exit status or `128 + signal` after removal.

There is no RAII rollback of the cgroup, child, or raw pipe descriptors on early
failure. Mounts disappear according to kernel namespace ownership, not an
explicit unmount sequence. The synchronization and teardown ordering is meant
to make guest smoke tests observable; it is not a persistent container lifecycle
manager.

## Usage example and test payload

In an isolated guest, prepare a bundle with an executable at
`rootfs/bin/oci-test-init` and this `config.json`:

```json
{
  "root": {"path": "rootfs"},
  "process": {
    "args": ["/bin/oci-test-init"],
    "env": ["OCI_SMOKE=1"],
    "cwd": "/"
  },
  "mounts": [{"destination": "/proc", "type": "proc", "source": "proc"}]
}
```

With the guest permitting that procfs mount, `mini-oci run smoke /path/to/bundle`
should execute `oci-test-init`. The payload checks the `0::/oci-smoke` membership
prefix, cwd `/`, and `OCI_SMOKE=1`, then prints `OCI_SMOKE_PASS pid=...`.
This example requires guest integration setup; documentation compilation does
not execute it.

## Known limitations

Only `run` is supported. Configuration parsing accepts unknown fields, does not
validate container IDs or confine host paths, and implements only the described
mount and process subset. It has no seccomp/capability implementation, namespace
completeness, rollback manager, rootless mode, or resource-limit configuration.
