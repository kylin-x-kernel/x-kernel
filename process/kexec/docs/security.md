# kexec security and reliability

## Scope, assets, and trust model

This analysis covers all production code in `src/lib.rs`, `src/elf_image.rs`,
`src/loader.rs`, and `src/lru_cache.rs`. Test-only modules, including
`src/elf_image/tests.rs`, are excluded because they are not linked into a normal
kernel image. Assets are the target address space, executable identity,
file-derived mapping permissions/offsets, and owned argument/environment data.
The loader trusts MM and VFS ownership contracts but not executable bytes,
interpreter paths, or ELF metadata. Callers own syscall buffer copying and the
process-wide exec protocol.

VFS is responsible for path-walk integrity, execute-permission decisions, and
the lifetime of returned `Path`/`VfsFile` objects. `kexec` invokes those checks
and independently validates every file-derived ELF field before MM use. MM is
responsible for validating user address mappings and writes after `kexec` passes
checked ranges. Callers are responsible for a current credential snapshot,
exclusive access to the target `MmSpace`, and stopping or replacing peer threads.

## External input boundaries

`ExecRequest::prepare` consumes owned path/argument/environment values and a
credential snapshot. String sources use current root/pwd and Exec lookup;
resolved sources retain their `Path`. Both check `Permission::MAY_EXEC` and open
the file. An absent display path is reconstructed via VFS. File reads supply ELF
headers, PT_LOAD addresses/sizes/flags, PT_INTERP bytes, and script text.
`SCRIPT_RECURSION_MAX` limits redirect count. PT_INTERP is limited to one entry
and 4096 bytes and must be a complete NUL-terminated UTF-8 string. ELF identity,
machine, type, segment sizes, file/address ranges, alignment, page congruence,
entry point, and program-header address are validated before commit. Stack
construction checks configured capacity before commit. `AT_RANDOM` receives 16
bytes from the kernel entropy pool, while `AT_EXECFN` points to a dedicated copy
of the originally requested executable display path, survives script redirects,
and does not depend on `argv[0]`. A script interpreter line must be UTF-8 and
select a non-empty interpreter before another path lookup begins.

There is no direct user-pointer, MMIO, DMA, firmware, FFI call, or assembly here.
The signing feature delegates TA verification to `tee_task_iface`; MM methods
own address validation, page population, and safe user-address writes.

## Unsafe inventory and invariants

There is no handwritten unsafe block/function/impl in this crate.
`#[self_referencing]` on `ElfCacheEntry` delegates generated self-reference
machinery to `ouroboros`: the owned byte vector must outlive its ELF header view,
and callers use generated borrowing/building APIs rather than extracting a
longer-lived reference. No local `SAFETY` block exists to audit for that generated
implementation; dependency soundness remains external.

`BinPrm` pins the executable and owns args/env/cred. `PreparedExecImage` pins the
validated executable and interpreter cache entries with `Arc`, so cache eviction
cannot invalidate their header views. Mapping code delegates VMA construction
and installation to `filemap`/`MmSpace`. Private LRU indices assume
valid bounded storage (production capacity 32). Safe indexing can panic on a
broken cache invariant; it does not create a raw-memory access boundary.

## Thread safety

`ELF_LOADER` protects cache lookup, insertion, and preparation. Commit consumes
only request-owned `Arc` entries and does not reacquire the cache lock.
The caller exclusively borrows the target `MmSpace`; it must separately prevent
other threads from executing an obsolete image during a process exec protocol.
VFS work while holding loader/current-filesystem locks can block and must not
reenter the same lock chain.

## Threat analysis

| ID | Threat and asset | Severity | Trigger | Effect | Response and residual risk |
|---|---|---|---|---|---|
| T-01 | Malformed executable disrupts loading | Medium | Invalid identity, architecture, header, load segment, or interpreter text | A wrong mapping, kernel panic, or denial of exec could follow if metadata reached MM unchecked. | `ValidatedElfImage::new` in `src/elf_image.rs` validates image metadata. In `src/loader.rs`, `ElfCacheEntry::load_file`, `read_exact_at`, `ElfLoader::interp_path`, and `script_interpreter_args` reject invalid table ranges, alignments, short reads, and interpreter text with recoverable errors; scripts stop after four redirects. |
| T-02 | Excessive file-derived allocation/mapping | Medium | ELF advertises huge ranges or lengths | Kernel memory pressure or an oversized user mapping could deny service. | `ValidatedElfImage::new` bounds ELF64 tables and segments with checked arithmetic; `ElfLoader::interp_path` caps PT_INTERP at 4096 bytes; `ExecLayoutPlan::new` requires complete image spans below the heap. Allocator failure remains governed by kernel policy. |
| T-03 | Resume of a destroyed image | High | Mapping or stack work fails after `MmSpace::clear` | Resuming the old instruction pointer could execute in a partial or unmapped address space. | `load_user_app_request` returns `ExecFailure::AfterCommit`; `sys_execve` drops the address-space lock and terminates the process with `SIGSEGV`. Other callers must apply the same policy. There is no rollback. |
| T-04 | Changed file uses stale metadata | Medium | Executable contents change after caching | Header-derived mappings can disagree with the current file contents. | `ElfLoader` keys entries by path identity and pins files, while `clear_elf_cache` provides explicit invalidation. No content-version check exists; dependence on VFS write exclusion or explicit flushing is an accepted residual risk. |
| T-05 | Prepared entry is evicted before commit | Low | Concurrent loads or explicit cache flush during the prepare/commit gap | A dangling parsed-header view could otherwise cause invalid reads or wrong mappings. | `PreparedElfImage` retains an `Arc<ElfCacheEntry>`; `LruCache::flush` removes reuse visibility but cannot invalidate the request. |
| T-06 | Wrong executable authority | High | Consumer substitutes display text for a resolved object or supplies stale credentials | The kernel could execute a different file or bypass the intended authorization decision. | `ExecRequest::prepare` retains the resolved `Path` and checks `Permission::MAY_EXEC` with the supplied snapshot. Caller ownership of snapshot freshness and the broader exec authorization remains explicit. |
| T-07 | TA signature rejection | High | Verification under `tee_ta_sign` fails or is bypassed | An untrusted TA image could execute. | `ElfLoader::ensure_cached` requests `tee_task_iface` verification and maps rejection to `PermissionDenied` before cache insertion. Cache coherence remains a separate assumption. |

## Failure modes and effects (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity (1-4) | Handling |
|---|---|---|---|---|---|---|
| F-01 | Preparation fails | VFS resolution/open/permission or invalid image | `KResult` error before clear | Old image retained | 3 | Caller reports normal preparation failure. |
| F-02 | Script recursion exhausted | More than four redirects | `FilesystemLoop` | Exec rejected | 3 | Stop before commit. |
| F-03 | Post-clear failure | Mapping, population, or write failure | Partially rebuilt address space | Old image cannot resume | 1 | Return `ExecFailure::AfterCommit`; the syscall path terminates the process rather than returning errno. |
| F-04 | ELF validation fails | Wrong architecture, malformed range, invalid alignment, short read, or no load gap | `InvalidExecutable` or `NoMemory` before clear | Old image retained | 3 | Reject the prepared request without entering commit. |
| F-05 | Stale cache | File changed without invalidation | Old parsed headers | Wrong load semantics | 2 | Explicit flush only; residual coherence limitation. |

## Failure management and privacy

Preparation failures and destructive-commit failures retain their underlying
`KError` in `ExecFailure`. Invalid executable, filesystem loop, address-layout
exhaustion, argument-list size, MM/VFS, and optional signature errors remain
distinct. The syscall path exposes only pre-commit errors as errno and turns a
post-commit error into process termination. Allocator failure has no local
recovery.
Debug logs can expose executable/interpreter paths and mapping ranges. Arguments,
environment, and executable bytes are held in owned buffers; final release does
not explicitly erase them. The crate does not intentionally log their contents.

## Known limitations and audit checklist

- Preserve current-fs initialization and a consistent credential snapshot.
- Keep file-private mappings on the approved MM APIs and align both offsets.
- Never imply an arbitrary load error leaves the previous address space intact.
- Preserve phase-aware handling when adding a new `load_user_app_request` caller.
- Add architecture-derived `AT_HWCAP` and `AT_PLATFORM` before claiming full
  dynamic-runtime auxiliary-vector compatibility.
- Reassess file-change invalidation when VFS gains an executable-write exclusion
  or stable inode change sequence.
- Keep all file-derived arithmetic and reads on checked, recoverable paths.
- Keep generated self-reference ownership and dependency assumptions visible.
