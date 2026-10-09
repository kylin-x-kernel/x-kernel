# procfs - Security and Reliability

## Trust Model

Procfs accepts filesystem operations through `kvfs` and publishes kernel-owned
information. It trusts `kvfs` for path resolution, credentials, generic access
checks, buffer validity, and open-file serialization; it trusts `kprocess` and
`memspace` for task/resource lifetimes, coherent MM capture, and VMA invariants.
Output is diagnostic data and is not an authorization decision or proof that
separately read process fields describe the same image.

## External Boundaries and Attack Surface

Source paths below are relative to `fs/filesystems/procfs/` unless stated otherwise.

`sys/kernel/pty/max` and `sys/kernel/pty/nr` are read-only observations of `ktty`
state. Their `0444` mode is not the sole write barrier: the `SimpleFile` closure
backend rejects writes even when a privileged opener bypasses mode checks.
Reading does not reserve a slot or keep a pair alive; the reported reservation
count can change immediately after observation and grants no admission guarantee.

- Filesystem names, PID/TID/FD components, offsets, and write bytes are external
  inputs. Numeric lookup components are parsed, task-directory membership is
  checked, and unsupported names fail lookup (`src/task_nodes/root.rs`).
- Writable controls include `comm`, `oom_score_adj`, trace event enables,
  `sys/kernel/printk`, and optional `sysrq-trigger`. `comm` truncates to 15 bytes
  and validates C-string/UTF-8 conversion; OOM adjustment accepts only parsed
  values in `[-1000, 1000]`. Tracing delegates validation to `ktracing`; SysRq
  accepts the first non-whitespace byte only when it is `t`.
- FD magic links expose resource-backed paths. `ProcFdLink::follow` rejects
  magic-link-disallowing flags or lack of final-component following before
  obtaining a descriptor snapshot. Descriptor reuse can change a later result.
- Task names, command lines, executable/VMA paths, mount data, trace records,
  and optional TA metadata can carry sensitive or externally influenced text.
- Procfs operates on Rust slices and metadata, not raw user pointers or mapped
  user payload. It has no direct MMIO/PIO, DMA-buffer, FFI, assembly, or interrupt
  handler boundary. Boot command lines, device topology, and hardware statistics
  arrive through provider APIs; RISC-V counter access is delegated to `riscv`.

## Unsafe Inventory and Memory Safety

There are no local `unsafe` blocks/functions or manual unsafe `Send`/`Sync`
implementations in `src/`. Safety still depends on provider contracts:

- `MapsIter` retains a `LiveAddressSpace` active MM user, not merely a structural
  reference. `ProcessRuntimeState::address_space` in repository-relative
  `process/kprocess/src/process_runtime/runtime_state.rs` clones the MM user and
  matching heap handle inside the same binding critical section.
- VMA access occurs under that capability's MM mutex. Entries own copied values
  and paths, so formatting does not retain references into an unlocked VMA list.
- `LiveAddressSpace::heap_top` in
  `process/kprocess/src/process/runtime_access.rs` performs an Acquire atomic
  read without locking the current process binding. Last-user capability drop
  may lock and clear the MM; callers must release it in sleepable context,
  outside the MM mutex and spinlocks.
- Weak task references are upgraded before access. A successful task upgrade
  does not imply that process runtime resources remain attached; accessors can
  still fail and their errors must be propagated.

## Thread Safety

`fs/kvfs/src/seq_file.rs` (repository-relative) requires `SeqIterator: Send` and
serializes each open iterator with a mutex. Procfs provides no extra unsafe
thread-safety implementation. MM capture releases the binding lock before the
MM mutex is acquired. The heap's Acquire/Release operations preserve live
same-image updates but do not create a transaction over all procfs fields.
Independent opens, weak-reference callbacks, and mount/FD lookups may observe
different moments.

## Threat Analysis

| ID | Threat | Severity | Trigger | Existing controls and residual risk |
| --- | --- | --- | --- | --- |
| T-01 | Cross-image maps/heap mismatch | Medium | Binding changes after a reader captures its MM | Single-lock MM/heap capture and capability-local heap reads retain the pair. Contents remain live. |
| T-02 | Stale task or torn-down MM access | High | Exit races with lookup, open, or read | Weak upgrades, fallible resource access, active MM-user retention, and locked VMA copies guard access. Retained views intentionally prolong mapping lifetime. |
| T-03 | Information disclosure or unwanted controls | High | An insufficiently restricted caller reaches process/diagnostic nodes or writes controls | VFS permissions apply; `printk` explicitly checks `file.cred().is_privileged()`. Other listed callbacks have no equivalent local privilege check, and task reads have no local ptrace-style policy. Deployment access policy and further credential checks require separate audit. |
| T-04 | Resource pressure and lock contention | Medium | Many retained maps opens, large VMA lists, or repeated rewinds | Ownership cleanup releases buffers and MM users, but procfs imposes no local quota on retained views or scan work. |
| T-05 | Ambiguous diagnostic text | Low | Names or paths contain separators/newlines | Mount sources are mangled; maps paths and several other strings are emitted verbatim. Consumers must not treat all output as escaped records. |

## Failure Modes and Effects (FMEA)

| ID | Failure | Cause | Local effect | System effect | Severity (1-4) | Handling |
| --- | --- | --- | --- | --- | --- | --- |
| F-01 | Target unavailable | Task reclaimed or runtime detached | Lookup/open/read fails, or directory enumeration is empty | Observation unavailable | 4 | Return `NotFound`/`NoSuchProcess` as appropriate; do not retarget retained maps |
| F-02 | Invalid command | Bad encoding, range, or unsupported command | Write rejected | State remains unchanged on those validation failures | 4 | Return `InvalidInput`; unauthorized printk returns `OperationNotPermitted` |
| F-03 | Allocation pressure | Large generated output or many retained views | Allocation/latency pressure | Kernel allocator failure policy applies | 2 | No procfs allocation recovery or admission limit |
| F-04 | Invalid release context | Last MM-user drop under a conflicting lock or in non-sleepable context | Teardown blocks or violates context requirements | Potential hang/failure | 2 | Respect capability drop contract; release outside binding/MM locks |

## Failure Handling

Ordinary lookup, resource-access, and parsing errors use `VfsResult`. Missing FD
targets are translated from `BadFileDescriptor` to `NotFound`. Mountpoint
rendering falls back to `<mount:name>` if absolute-path rendering fails; missing
TEE runtime data may render empty output. Seq-file formatting errors become
`Io`. Registration uses `expect` for the register-once invariant; formatting
unwraps rely on generated ASCII permission bytes or infallible String formatting.
There is no module-level retry, rollback, or panic recovery protocol.

## Privacy and Known Limitations

Maps exposes addresses and paths; task nodes expose command lines, names, and
executable/FD information. Tracing, topology, and optional TA information expand
the disclosure surface. These files must be included in the system's visibility
policy; absence of raw user-memory reads does not make their output non-sensitive.

The retained maps capability preserves MM/heap identity, not a frozen image or
a transaction spanning procfs reads. Same-image changes remain observable on
recollection. Retention can outlive task exit and delay last-user MM cleanup;
Linux-exact lifetime compatibility is not promised. Exec replacement,
hardware-root switching and shared-brk policy are outside procfs. Placeholder
statistics and namespace identities are described in `design.md`.

## Audit Checklist

- Keep MM-user and heap-handle capture inside one binding critical section;
  never independently query the current process heap from `MapsIter`.
- Keep `heap_top()` safe under the captured MM mutex and preserve Acquire reads.
- Check retained old/new image pairing, same-image heap visibility, exit/drop
  lifetime, existing fork policy, and actual maps heap labels in regression tests.
- Check release context and avoid lock recursion or final MM-user drop under
  binding/MM locks.
- Review credential enforcement and magic-link flags when adding entries or
  controls; preserve lookup-time versus operation-time error distinctions.
- Treat text escaping, retained-view resource costs, and feature-gated outputs
  as explicit review concerns; do not broaden snapshot or compatibility claims.
