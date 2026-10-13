# of — Security And Reliability

## Scope

This analysis covers the entire crate — `src/lib.rs` (global DTB state,
semantic accessors, PCI/interrupt/memory decoding) plus
`ut-tests/of_unittest.rs`, which is test-only and reachable exclusively
through the audited public entry points. The four unsafe entry points
are the only direct external-input boundaries; the DTB blob itself is
validated by `rs_fdtree` (see its own security document).


## Trust Model

The device tree is external boot-provided data: structurally validated
once by `rs_fdtree` at initialization, then interpreted here under a
"never trust cell contents" rule. The crate treats every property read as
potentially truncated, oversized, or lying, and degrades to `None` /
`Unknown` values instead of panicking. Kernel-internal callers (platform
init, drivers) are trusted to check those `Option`s.

## External Boundaries

- The boot DTB pointer handed to `init_device_tree_ptr` is the root trust
  boundary: the blob must be valid and must remain accessible for the
  kernel lifetime (see Unsafe Code). Structural validation (magic,
  block layout) is delegated to `rs_fdtree`.
- Property byte streams (interrupt cells, `ranges`, `interrupt-map`,
  phandle targets) are untrusted data decoded with per-step bounds
  checks.
- The decoded outputs — IRQ numbers, ECAM bases, memory regions,
  power-off register addresses — drive kernel configuration downstream;
  a wrong-but-well-formed DTB is indistinguishable from truth here.

PCI interrupt-map child specifiers are validated as three address cells and
one interrupt cell before key-size arithmetic and 4-cell buffer decoding.
Unsupported child layouts return `None`, preventing firmware-controlled cell
counts from indexing beyond those buffers. Oversized parent specifiers are
rejected before filling the 16-cell buffer and report the requested size and
capacity through `log::warn!`.
This change does not add diagnostic coverage for other parser failures.

## Unsafe Code

Four unsafe entry points, each with a `# Safety` section and a matching
`SAFETY:` comment at its single unsafe block:

- `init_device_tree_ptr(ptr)` — fabricates `LinuxFdt<'static>` from the
  boot pointer. Caller precondition: `ptr` targets a readable DTB that
  stays accessible for the whole program lifetime.
- `dtb_total_size_from_ptr(ptr)` — same precondition, lifetime limited to
  the call.
- `read_memory_regions_from_ptr(ptr)` /
  `read_reserved_memory_regions_from_ptr(ptr)` — same precondition; no
  global state is touched.

The `'static` lifetime is sound only because the boot DTB lives in memory
the kernel never reclaims or rewrites.

## Protected Resources

- `PMU_IRQ_CACHE` (`AtomicUsize`): profiling interrupt number cache. A
  `0` value means "unresolved"; the PMU IRQ is never 0 in practice, which
  is what makes the cache protocol safe.
- `SysconControl { paddr, value }`: describes a register the power-off
  path will write. The address is computed from the DTB (`regmap` base +
  `offset`); the write itself belongs to the power-off layer.

## Invariants

- Every multi-byte property read validates `value.len() >= N * 4` before
  forming cells; `interrupt-map` walking bounds-checks each step against
  the map length and returns `None` on truncation.
- Phandle resolution only reads nodes found in the validated tree.
- `ranges` parsing clamps to the declared `total_cells` and rejects
  chunks shorter than the cell count.
- No accessor panics on malformed content: undecodable values surface as
  `None`, `InterruptTrigger::Unknown`, or
  `InterruptControllerKind::Unknown`.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | DTB blob freed or overwritten after init | High — UAF on every tree access | Boot code reusing the DTB buffer | `init_device_tree_ptr` `# Safety` contract puts ownership on the caller; kernel boot never reclaims the region. Residual risk: a boot-path regression would be kernel-wide. |
| T-02 | Truncated property stream | Low — missing device configuration | Malformed or hostile DTB | Per-read length checks return `None`; nothing indexes past the buffer. |
| T-03 | Lying IRQ number or memory region | Medium — misdirected interrupts, wrong memory adoption | Compromised or buggy firmware | Not detectable at this layer; validation is structural only. Residual risk accepted and owned by platform policy. |
| T-04 | PMU IRQ cache poisoning | Low — wrong profiling interrupt | Concurrent `pmu_irq_or` calls racing the cache | Acquire/release atomic protocol; resolution is idempotent, so a race costs at most a redundant tree walk, never a torn value. |
| T-05 | syscon-poweroff register pointing at a critical register | Medium — unintended register write at power-off | Malicious DTB node | The node must reference a regmap phandle within the validated tree; the write target is firmware-declared by design. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | DTB missing or unparseable at init | `FirmwareInitError` returned | Platform falls back to static config or aborts | 3 | `Result` at the init boundary; no panic. |
| F-02 | Accessors called before init | `None` from `fdt()` | Callers treat as "no device tree" | 3 | `LazyInit` + `Option` accessors. |
| F-03 | Undecodable interrupt/range cells | `None` / `Unknown` variant | Device stays without IRQ or window | 3 | Degrade, never guess. |

## Known Limitations

- A DTB that is well-formed but lies (valid checksums, wrong addresses)
  is fully trusted by this layer.
- `bus-range` and cell-count defaults (`#address-cells` = 1/2) follow the
  DT spec but can misread a spec-violating tree; the result is a failed
  decode (`None`), not memory unsafety.

## Audit Checklist

- New property decoders bounds-check every indexed read.
- New unsafe entry points keep the "blob lives forever" contract and a
  `# Safety` section.
- No panic paths added on firmware-controlled data.
