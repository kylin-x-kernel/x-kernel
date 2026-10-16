# kexec design

## Purpose and background

`kexec` loads user executables and prepares their address-space layout. It is an
MM client: `filemap` creates private file VMA/runtime pairs, `memspace::MmSpace`
installs mappings and coordinates page tables, and underlying MM owners manage
page cache and anonymous/COW pages. Syscall ABI copying, process publication,
credential commit, and post-exec policy belong to callers.

## Responsibilities

- Resolve and pin executable, script-interpreter, and ELF-interpreter files
  through VFS while enforcing the supplied credential's execute permission.
- Parse and validate ELF metadata, normalize load segments, choose non-overlapping
  image addresses, and construct the initial auxiliary vector.
- Install into an exclusively borrowed, user-empty address space the executable mappings,
  signal trampoline, initial stack, and heap.
- Maintain a bounded cache of parsed executable headers and keep prepared cache
  entries alive until commit finishes.

## Non-responsibilities

- `core/ksyscall` owns syscall ABI decoding and user-pointer copying;
  `posix/process` and other callers own thread coordination, process publication,
  credential transitions, and the policy for a failed destructive commit.
- VFS owns path-walk rules, file permissions, executable-write exclusion, and
  file-version coherence. This crate does not detect changes to cached files.
- `memspace`, `filemap`, and the underlying MM crates own page tables, page-cache
  pages, anonymous memory, COW, and VMA teardown.
- This crate does not randomize load addresses or provide a general-purpose ELF
  parser for kernel and boot images.

## Source scope and architecture

`src/lib.rs` re-exports `ExecSource`, `ExecRequest`, `BinPrm`, `PreparedExecImage`,
`prepare_user_app_request`, `load_prepared_user_app`, and `clear_elf_cache`.
`src/elf_image.rs` validates ELF
metadata, normalizes load segments, and plans executable/interpreter addresses.
`src/loader.rs` contains VFS loading, request preparation, the global `ElfLoader`,
and `ElfCacheEntry`.
`src/lru_cache.rs` is a private capacity-bounded cache using vector storage and
indexed MRU/LRU links; its production capacity is 32.

The public interaction directions are:

- `posix/process` prepares an owned image with `prepare_user_app_request`, then
  calls `load_prepared_user_app` on init's unpublished, user-empty `MmSpace`.
  `core/ksyscall` uses the same split APIs and owns its commitment boundary:
  it activates a fresh user-empty MM between preparation and installation, leaving
  independent VM-sharing peers on the old MM. The loader does not perform activation.
- `ExecRequest::prepare` resolves and pins a `BinPrm` without changing an address
  space. Full preparation accepts either a pathname or an already resolved
  request after this inspection step.
- `devfs` memory-tracking support calls `clear_elf_cache`; the operation removes
  cache visibility while in-flight prepared requests retain their `Arc` entries.
- `kexec` calls VFS for resolution and reads, `filemap` for private executable
  VMAs, `memspace` for mappings and stack writes, and `ksignal` for the signal
  trampoline. These dependencies do not call back into `kexec` during commit.
- `ElfCacheEntry::load_file` calls
  `kernel_elf_parser::ElfHeadersBuilder::{new, ph_range, build}` to create owned
  cached header metadata. `PreparedElfImage::new` then passes that metadata to
  the private `elf_image::ValidatedElfImage` validator before any MM operation.

## Feature configuration

The `tee` feature enables the optional `tee_task_iface` dependency and the
corresponding `kprocess/tee` integration. `tee_ta_sign` enables
`tee_task_iface/tee_ta_sign`; when active, `kexec` asks that interface to verify
new cache entries before accepting them. Neither feature changes ELF placement
or MM ownership.

```text
ExecRequest -> prepare -> BinPrm (owned args/env/cred + pinned VfsFile/Path)
    -> ELF cache / script redirects / PT_INTERP resolution
    -> validate ELF and construct ExecLayoutPlan
    -> PreparedExecImage (pinned entries + layout + owned stack + entry/SP)
    -> load_prepared_user_app(user-empty MM)
    -> signal trampoline -> main/interpreter PT_LOAD
    -> anonymous stack -> stack population/write -> anonymous heap
    -> (entry point, stack pointer)
```

## Execution context

Loading needs task context, allocator, VFS, MM, and scheduler/lock services.
String paths, scripts, and dynamic interpreters consult the current process
filesystem context; its root/pwd must be initialized. Resolved requests retain a
`kvfs::Path` and optionally a display path, but still check execute permission
against the supplied credential. Operations can block on filesystem reads and
sleepable locks and are unsuitable for interrupt context. Initial-process
loading may run from a kernel bootstrap task: `kprocess::current_fs_context`
then uses the initialized global `INIT_FS`, so no user Thread is required. A
user-task caller instead needs its process filesystem context still attached.
The caller supplies exclusive `&mut MmSpace` and owns execution handoff.

## Request preparation and image algorithms

`ExecRequest` owns arguments, environment, source, and a credential snapshot.
`prepare` resolves a string via `LookupIntent::Exec`, or reuses the resolved
`Path`, checks `MAY_EXEC`, obtains a display path when absent, and pins an opened
`VfsFile`. It does not receive or modify the target address space. Display paths
are used for script argument reconstruction, not as a replacement authority for
an already resolved object.

`ElfCacheEntry::load_file` reads up to 4096 bytes and then reads the complete,
checked program-header range when necessary. Short reads, file-bound violations,
and range conversion failures reject the image. `ouroboros`
encapsulates a self-referential header view over owned data. Under `tee_ta_sign`,
TA verification is requested before accepting an entry; failure maps to
`PermissionDenied`. Entries are keyed by `Path::ptr_eq`, not file content version.

A non-ELF head beginning with `#!` is parsed from at most the first 256 bytes.
The line must be UTF-8 and name a non-empty interpreter. The interpreter and
optional argument prefix are followed by the script display path and original
argument tail. At most four redirects are permitted.
For PT_INTERP, the loader permits one entry of at most 4096 bytes, requires a
complete read and exactly one NUL-terminated UTF-8 path, resolves it using the
same credentials, and validates the interpreter before replacing the address
space.

`ValidatedElfImage` accepts 64-bit little-endian `ET_EXEC` and `ET_DYN` images
for the build target (`EM_AARCH64`, `EM_X86_64`, `EM_RISCV`, or
`EM_LOONGARCH`). It checks the program-header table, requires a load segment,
checks file/memory sizes and ranges, rejects arithmetic overflow, validates
`p_align` and virtual/file offset congruence, and requires the entry point to be
inside an executable segment whose final page mapping is executable. Raw
PT_LOAD memory ranges may touch but must not overlap. Page overlap caused only
by alignment between consecutive segments is assigned to the later segment,
matching the final fixed mapping ownership.

`ExecLayoutPlan` is complete before address-space replacement and activation.
`ET_EXEC` retains absolute addresses. An `ET_DYN` main image receives an
alignment-correct bias at the user base. For an `ET_DYN` interpreter,
`USER_INTERP_BASE` is a search hint: the
planner reserves the main image's complete load span, searches for an aligned
gap below `USER_HEAP_BASE`, and falls back below the hint when necessary. The
selected bias drives both segment addresses and `AT_BASE`.

Installation checks that the target has no user VMAs, maps the signal trampoline, and maps the
prevalidated PT_LOAD segments for the main executable and optional dynamic
interpreter. Segment virtual/file starts are aligned down to 4 KiB; their page
offset relationship was already validated during preparation.
`new_file_private_vma` receives file offset and `p_offset + p_filesz` as the
file-data boundary, preserving prefix, zero-tail, and private-mapping semantics
through MM rather than implementing a second loader-specific memory owner.
Auxiliary entries describe the main image and interpreter base and include the
credential snapshot's `AT_UID`, `AT_EUID`, `AT_GID`, `AT_EGID`, and explicit
`AT_SECURE`. `app_stack_region` adds `AT_RANDOM` backed by the kernel entropy
pool and `AT_EXECFN` backed by the resolved request's display path, including
when `argv` is empty. Script redirects preserve the original script display
path for `AT_EXECFN` while using the interpreter path for loading. `AT_HWCAP`
and `AT_PLATFORM` are not yet supplied, so dynamic runtimes that require either
entry remain outside the supported ABI. The interpreter entry is selected when
present.

Preparation builds argc/argv/env/aux data with `app_stack_region` and checks
its configured stack capacity. The ready value owns these encoded bytes rather
than retaining a second copy of the original argument strings. Installation maps
an anonymous stack, populates/writes its pages, and maps an anonymous heap. The returned tuple is
entry address and user SP.

## Commit boundary and error handling

ELF parsing, segment validation, interpreter resolution, address planning,
auxiliary-vector scalar computation, random-byte generation, and initial-stack
construction happen without access to a target MM. The installer consumes the
ready value and never clears or switches address spaces. It rejects a nonempty
target with `InvalidInput` before mutation; later failures can leave partial
mappings that the caller must discard or handle as a fatal post-commit failure.

In the exec syscall, successful process-MM binding exchange is the commitment
boundary: the caller cannot resume the old user image. Both loader APIs return
ordinary `KError` values. Preparation and replacement precheck errors return as
errno; after exchange, fallible installation and process-update work follows the
caller's fatal `SIGSEGV` policy. Installation targets a fresh activated MM, not
the old MM retained by independent users. The init-process caller
uses the split APIs directly and panics on preparation or installation failure
because it has no prior user image to resume.

The loader does not roll back partial installation. Callers own commitment and
failure policy; installation itself does not require a current user process.

## Concurrency and cache lifecycle

The global sleepable `ELF_LOADER` mutex serializes cache lookup, insertion, and
associated VFS work. The guard is released once executable/interpreter entries
are pinned, before final validation, layout and stack construction.
`PreparedExecImage` holds
`Arc<ElfCacheEntry>` references for the executable and interpreter, so commit no
longer reacquires the cache lock and eviction or `clear_elf_cache` cannot
invalidate an in-flight request. The address space is independently protected by
the caller's exclusive borrow.

Cache eviction/flush releases entries after prepared requests drop their `Arc`
references. VMAs retain mapped files through MM runtime ownership, so clearing
the header cache does not unmap an executable. `clear_elf_cache` also clears the
TA header cache when the signing feature is active. No file-change invalidation
is integrated.

The request lifecycle is `ExecRequest -> BinPrm -> PreparedExecImage -> installation`.
Before installation, dropping prepared state releases its file/cache references
and stack bytes without changing any target. After installation, mapped files
are retained by MM-owned VMA runtimes; dropping `PreparedExecImage` releases its
header references and staging stack bytes. MM teardown, including last-active-user
clear, releases mapped VMAs, stack and heap through their owning MM types. Exec
replacement does not clear an old MM while independent active users retain it.

## Decisions and limitations

Common file-private VMA construction keeps ELF mappings aligned with mmap/COW
ownership. Stack and heap use normal anonymous MM APIs. Pure BSS load segments
(`p_filesz = 0`) may have offsets beyond EOF because they declare no file bytes.
When their aligned page prefix starts beyond EOF, they use anonymous zero-filled
mappings. In-file page prefixes retain private file mappings: a shared page may
contain bytes from the preceding segment, including the program-header table.
The BSS portion is zero-filled; nonempty declared file ranges must remain inside
the executable.
Metadata and stack bytes
are prepared without a target MM; fallible installation remains separate. The
exec syscall replaces only its own process binding, preserving independent
CLONE_VM peers' old image, and distinguishes errors at the exchange boundary.
Known constraints include stale cache contents, fixed
rather than randomized load hints, missing `AT_HWCAP`/`AT_PLATFORM`, and no
complete exec credential/namespace transaction within this crate.

## Regression Support

Unittest-only ELF fixtures and request-local partial-install probes support the
exec lifecycle tests in `core/ksyscall/src/task/tests/exec_lifecycle.rs`.
They check that installation errors propagate through the caller's commitment
policy. The fixture payload contains metadata, not executable user instructions.
Probe mechanics and observation contracts are documented alongside the test
interfaces in `src/loader.rs`; production loading has no injection selector.
The partial-install observer is an owned closure capturing request-local peer
state. This lets the syscall fixture wait for explicit acknowledgement without
adding a scheduler dependency or a global handshake to the loader.

File-private BSS page prefixes are capped at the actual executable size,
including when the BSS offset lies beyond EOF within the same page. The
remaining bytes are zero-filled rather than copied from the file cache.
