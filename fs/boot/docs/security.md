# fs_boot — Security and Reliability Analysis

## 9P owner failure

Failure to prepare the transport KBox drops the selected local class handle
before client negotiation, remote attachment or local root publication.
Device enumeration and transport internals retain existing allocation behavior.
The mount callback returns NoMemory rather than choosing fatal-boot policy;
the existing caller decides whether its particular mount failure is fatal.


## Required-filesystem allocation failure

Failure to allocate bootstrap-root or initial-cgroup private state is diagnosed
at the existing fatal-boot boundary. No namespace attachment is attempted with
an absent superblock. Other mount callers receive NoMemory through VfsResult.
This does not provide boot-time OOM recovery or make nested filesystem allocations
fallible; it separates explicit boot policy from the reusable constructor.

## Trust Model

The root block device name comes from the kernel build configuration; device enumeration and disk contents come from drivers and external media.
Validation of the concrete disk format is the responsibility of the `get_tree/fill_super` of the candidate `FileSystemType` in the registry.
Built-in filesystem type descriptors and the boot mount policy are trusted kernel build artifacts.

## External Boundaries / Attack Surface

- `KFEAT_ROOT_BLOCK` may not match any registered device.
- A block device may be missing, removed, return I/O errors, or contain a corrupted filesystem.
- The 9P mount tag and responses come from an external transport.

This crate does not handle user pointers; its calls occur before user processes start.

## Unsafe Code Inventory

`fs/boot/src` contains no `unsafe` blocks.

## Memory Safety Invariants

- The initial root path may be obtained only after the bootstrap mount namespace has been successfully initialized.
- The root device handle remains valid until `FsContext::get_tree` and the real root graft complete.
- The root/pwd of the init `FsStruct` are updated in pairs in both switches, the bootstrap one and the real-root one.
- All built-in filesystem types must be registered exactly once through the unified init section
  before the user-visible procfs is installed and the mount syscalls are opened.
- The registry must hold the unique static descriptor reference of each implementation; boot uses these canonical objects directly,
  and POSIX and procfs look up/iterate the same objects through the registry, without copying types or creating separate providers.
- `nosuid`, `nodev`, `noexec`, and `relatime` belong to the concrete `Mount`; they must not be written into shared
  superblock state. `/dev` must not be set `nodev`.
- A root candidate must first be fully probed as RW, then fully retried with `SB_RDONLY` and the read-only mount flag; boot must not
  stop directly because of the first-round `EACCES` on a read-only block device.
- A read-only real root must already contain the mountpoint directories required by the boot policy; Linux likewise does not synthesize missing directories on
  read-only media. A missing directory makes the root image invalid and stops boot.
- The mount helper accepts only existing directories; nested paths that the boot policy declares to create must be established level by level first, and it must not merely
  create the final component or delete a non-directory object of the same name.
- The devtmpfs/sysfs mounted by boot must use the same shared superblock as the corresponding type factory,
  otherwise later mounts would expose a detached directory tree missing the boot nodes.
- The devtmpfs/sysfs constructors must retain the active reference of the internal root mount; unmounting a visible mount must not
  tear down a shared tree that the kernel is still updating.
- `anon_inodefs` and `pipefs` must complete hidden-mount publication before ordinary tasks can create anonymous fds/pipes;
  runtime paths must not perform first-time initialization implicitly.
- When cgroup2 is enabled, the boot mount must reuse `CgroupNamespace::initial()` with the initial `NsProxy`; the ordinary
  cgroup2 mount path that depends on the current user process must not be called before PID 1 is created, and a second hierarchy must not be created.

## Thread Safety

Initial mount setup is executed serially by the boot CPU. Mounted filesystems hold the backing device handle directly;
boot does not create a separate removal subscription or device lifecycle state.

## Threat Analysis

| ID | Threat Description | Impact Level | Trigger Condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | Drift between root type selection and the mountable type table | High | Kconfig, runtime, or boot maintains a second source of provider/name/capability | Kconfig only links implementations; each implementation registers its own canonical descriptor through the unified init section; boot only iterates the `REQUIRES_DEV` types of the same registry |
| T-02 | A corrupted disk triggers unsafe parsing | High | A filesystem does not validate external media | Mount errors must be propagated; disk validation is implemented by the concrete `fill_super` |
| T-03 | A mounted backing device is removed | Medium | Hot removal of the root or 9P device | The mount/session holds the resident object; later I/O propagates device errors; boot does not duplicate device lifecycle state |
| T-04 | Drift between the filesystem type list and user mount dispatch | Medium | Boot, procfs, and the syscall each maintain their own type names or construction entry points | Boot uses the registered canonical descriptor directly, procfs/POSIX obtain it from the same registry, and creation and attach all go through `MntNamespace::mount_new` |
| T-05 | Boot mount flags are wrongly applied to the shared superblock or disable `/dev` | High | Per-mount flags are filled into statfs backend state, or `nodev` is set on `/dev` | Boot sets `MountFlags` through namespace attach; the superblock constructor accepts only filesystem-wide flags |
| T-06 | A read-only root device fails to boot | High | Root probing uses only RW flags and panics directly after the first-round `EACCES` | Following Linux `mount_root_generic()`, perform two probing rounds, RW and RO, over the full filesystem candidate set; the RO round sets both the superblock and mount read-only bits; the root image must pre-provide the necessary mountpoints |
| T-07 | The nested 9P mountpoint lacks a parent directory | Medium | The mount helper runs `mkdir` only on the final `/mnt/hostshare` | Before mounting, `mount_host_share()` creates the policy-owned path level by level through `ensure_directory_path()`; the generic helper requires the target to exist already |
| T-08 | Anonymous fds/pipes run before the hidden pseudo fs is initialized | High | Boot misses the initialization or defers it to first use at runtime | After publishing the real root, `prepare_namespace()` explicitly initializes the two filesystems, and uninitialized access exposes the contract error immediately |
| T-09 | The boot cgroup2 mount and PID 1 use different hierarchies | High | Boot and `NsProxy` each create a root, or boot implicitly reads a nonexistent current process | Both paths share `CgroupNamespace::initial()`; boot mounts through an explicit initial superblock constructor |

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Failure Cause | Local Effect | System Effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | No block device | Driver did not probe or misconfiguration | Cannot select the root backing | boot panic | 1 | Explicit `No block device found` |
| F-02 | Root mount fails | Source lookup, I/O, format, or feature error | No real root superblock | boot panic | 1 | Log the `FsContext/get_tree` error and stop boot |
| F-03 | Preferred device name does not exist | Configuration inconsistent with hardware | Cannot select the root | boot panic | 2 | Output the configured name and candidate devices; do not fall back to other media |
| F-04 | Filesystem type name collision or duplicate registration | Boot wiring error | Registration returns `ResourceBusy` | boot stops before the namespace is opened to user space | 2 | Each config-enabled canonical type is registered exactly once |

## Fault Management

Failure of the root or critical virtual filesystems stops boot, to avoid running user space on an incomplete namespace.
Optional 9P mount errors preserve the transport context and stop boot.

## Known Limitations

Device removal cannot yet force a mounted block filesystem into read-only mode or complete an automatic unmount; later I/O
on already-held objects returns the underlying error. When no root name is configured, the compatible selection ordered by `dev_t` is still retained. Filesystem types
are currently a static built-in set; runtime module registration/unregistration is not supported.

## Audit Checklist

- Does `fs_boot` still avoid referencing concrete block filesystem crates or `KFEAT_FS_*` branches?
- Does a feature-controlled pseudo filesystem carry only explicit initial namespace policy, and does the boot
  path not depend on the current process?
- Is a new mount path namespace policy, or should it go into the generic mount/type layer?
- Does a new filesystem register a canonical type, without adding a name table in the procfs or POSIX layer?
- Are the flags of a new boot mount written into `Mount`; does `/dev` remain an accessible device node?
- Does a real root mount failure terminate before user space starts, and retain an explicit error?
- Is the backing device lifecycle held by the mount/session handle, rather than duplicating state in boot?
