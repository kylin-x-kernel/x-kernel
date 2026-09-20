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
- Replace an exclusively borrowed address space with the executable mappings,
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

`src/lib.rs` re-exports `ExecSource`, `ExecRequest`, `BinPrm`, `ExecFailure`,
`load_user_app_request`, and `clear_elf_cache`. `src/elf_image.rs` validates ELF
metadata, normalizes load segments, and plans executable/interpreter addresses.
`src/loader.rs` contains VFS loading, request preparation, the global `ElfLoader`,
and `ElfCacheEntry`.
`src/lru_cache.rs` is a private capacity-bounded cache using vector storage and
indexed MRU/LRU links; its production capacity is 32.

The public interaction directions are:

- `core/ksyscall` and `posix/process` construct `ExecRequest` and call
  `load_user_app_request`; `kexec` consumes the request and mutates their
  exclusively supplied `MmSpace`.
- `ExecRequest::prepare` resolves and pins a `BinPrm` without changing an address
  space. `load_user_app_request` accepts either an unprepared request or the same
  request after this inspection step.
- `devfs` memory-tracking support calls `clear_elf_cache`; the operation removes
  cache visibility while in-flight prepared requests retain their `Arc` entries.
- `kexec` calls VFS for resolution and reads, `filemap` for private executable
  VMAs, `memspace` for mappings and stack writes, and `ksignal` for the signal
  trampoline. These dependencies do not call back into `kexec` during commit.
- `ElfCacheEntry::load_file` calls
  `kernel_elf_parser::ELFHeadersBuilder::{new, ph_range, build}` to create owned
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
    -> PreparedExecImage (Arc-pinned cache entries + normalized segments)
    -> MmSpace::clear -> signal trampoline -> main/interpreter PT_LOAD
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

`ExecLayoutPlan` is complete before `MmSpace::clear`. `ET_EXEC` retains absolute
addresses. An `ET_DYN` main image receives an alignment-correct bias at the user
base. For an `ET_DYN` interpreter, `USER_INTERP_BASE` is a search hint: the
planner reserves the main image's complete load span, searches for an aligned
gap below `USER_HEAP_BASE`, and falls back below the hint when necessary. The
selected bias drives both segment addresses and `AT_BASE`.

Commit clears the address space, maps the signal trampoline, and maps the
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

The outer flow builds argc/argv/env/aux data with `app_stack_region` and checks
its configured stack capacity before commit. It then maps an anonymous stack,
populates/writes its pages, and maps an anonymous heap. The returned tuple is
entry address and user SP.

## Commit boundary and error handling

ELF parsing, segment validation, interpreter resolution, address planning,
auxiliary-vector scalar computation, random-byte generation, and initial-stack
construction happen before destructive commit. `MmSpace::clear` is the point
after which the old image is gone. Mapping, population, and writes can still
fail after that point. `load_user_app_request` reports pre-clear failures as
`ExecFailure::BeforeCommit` and post-clear failures as
`ExecFailure::AfterCommit`. The syscall layer returns the former as errno and
terminates the process with `SIGSEGV` for the latter. The init-process caller
panics on either class because it has no prior user image to resume.

The loader does not roll back a partial commit. Callers outside the existing
syscall and init paths must preserve the same phase-aware policy.

## Concurrency and cache lifecycle

The global sleepable `ELF_LOADER` mutex serializes cache lookup, insertion, and
request preparation, including associated VFS work. `PreparedExecImage` holds
`Arc<ElfCacheEntry>` references for the executable and interpreter, so commit no
longer reacquires the cache lock and eviction or `clear_elf_cache` cannot
invalidate an in-flight request. The address space is independently protected by
the caller's exclusive borrow.

Cache eviction/flush releases entries after prepared requests drop their `Arc`
references. VMAs retain mapped files through MM runtime ownership, so clearing
the header cache does not unmap an executable. `clear_elf_cache` also clears the
TA header cache when the signing feature is active. No file-change invalidation
is integrated.

The request lifecycle is `ExecRequest -> BinPrm -> PreparedExecImage -> commit`.
Before the clear boundary, dropping any state releases its file and cache-entry
references without changing the target address space. After clear, mapped files
are retained by MM-owned VMA runtimes; dropping `PreparedExecImage` only releases
header metadata. `MmSpace::clear` or process teardown releases the mapped VMAs,
stack, and heap through their owning MM types.

## Decisions and limitations

Common file-private VMA construction keeps ELF mappings aligned with mmap/COW
ownership. Stack and heap use normal anonymous MM APIs. Metadata and stack bytes
are prepared before destructive commit; fallible MM operations remain after it
and are phase-tagged. Known constraints include stale cache contents, fixed
rather than randomized load hints, missing `AT_HWCAP`/`AT_PLATFORM`, and no
complete exec credential/namespace transaction within this crate.
