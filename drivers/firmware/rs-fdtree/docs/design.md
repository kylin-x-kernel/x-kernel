# rs_fdtree — Design

## DMA declarations and resolved windows

The allocation-free DMA module has two levels.

**Local facts.** `parse_dma_ranges` and `FdtNode::dma_ranges` parse a bus node's
own `dma-ranges` tuples; `FdtNode::dma_coherency` reads only that node's
boolean flags. They preserve missing versus identity versus multiple windows,
check tuple arithmetic and reject malformed/duplicate properties. Cell widths
default to 2 address / 1 size; explicitly unsupported widths are errors.

**DT semantics.** `FdtNode::device_dma_window` applies Linux's DMA-parent walk:
start at the structural parent, skip empty declarations while the walk
continues, stop at the first nonempty one, require exactly one window, and
translate its parent-side address upward through every ancestor `dma-ranges`
boundary. A missing declaration on the first node is tolerated but a later
absence is not (mirroring `of_dma_get_range`). The whole `[start, start + size)`
interval must be contained in each ancestor window; ambiguous overlapping
ancestor windows are rejected. `interconnects`/`interconnect-names` and
`iommus`/`iommu-map` are rejected as topology this window cannot represent.
`FdtNode::inherited_dma_coherency` walks self and ancestors, first explicit
declaration wins, and absence stays `Ok(None)` (mirroring the walk inside
`of_dma_is_coherent`, without its platform default).

These results are DT interpretation, not CPU DMA permissions: device masks,
platform coherency defaults, IOMMU routing and host policy remain with the
device integration layer and the DMA provider. The fwnode backend exposes both
results as neutral `FwDmaWindow`/coherency queries. The DT-internal
`DeviceDmaError::Missing` is converted to `Ok(None)` at that boundary, because
the neutral contract represents every absence as `Ok(None)`; malformed,
multi-window, IOMMU and untranslatable descriptions remain typed errors.

## Strict property and structural access

Strict scalar/string/array reads validate encoding and distinguish duplicate,
malformed and missing properties, without allocation or binding policy.
Node offsets are source-local structure-block offsets, not firmware identities.
Parent and offset lookup scan the validated tree without allocation. Status
interpretation and reg/ranges resource translation belong to this DT backend.
The fwnode adapter's `is_interrupt_controller` answers from the node's
`interrupt-controller` declaration, so controller drivers consume the semantic
without matching the property name themselves.

## DT address interpretation

FdtNode::is_available accepts absent/ok/okay status, rejects malformed properties,
and does not decide ancestor availability or controller readiness.
FdtNode::mmio_resources collects full CPU-physical reg intervals and reg-names
through numeric ranges. Missing ranges rejects translation; empty ranges is
identity. PCI/ISA encodings remain unsupported. The private address module
owns interval validation, including inclusive maximum-address bounds; it does
not map MMIO. GIC register roles remain in the controller driver.
Host tests run both with and without alloc; address tests require alloc.

## Phandle references and interrupt specifiers

These are generic DT interpretations, owned here rather than by a firmware
backend adapter. `LinuxFdt::node_by_phandle` resolves a phandle (also the
legacy `linux,phandle`) in firmware order; zero and `u32::MAX` name no node.
`FdtNode::phandle` reads a node's own identifier strictly.
`FdtNode::interrupt_parent` returns the effective declaration cached on the
node during traversal: `all_nodes` keeps an inheritance stack and `find_node`
carries the value down its recursion, so neither a later query nor a subtree
node re-walks ancestors. A malformed or duplicated declaration is cached as
an error and inherited by descendants until a node declares its own valid
parent. `FdtNode::reference` resolves one phandle plus caller-declared
argument cells (for example `#clock-cells`) and returns the target node and a
borrowed `Cells` view. `FdtNode::irq_spec` prefers
`interrupts-extended`, otherwise uses `interrupts` with the inherited parent
and the controller's `#interrupt-cells`; it returns the controller node and
raw cells without GIC/PLIC meaning. All of this is allocation-free and strict
about duplicate/malformed properties. Phandle-to-node resolution is a linear
structure scan, matching libfdt; the per-node inheritance cache removes
repeated ancestor walks, not that final lookup.

Once the heap exists, `NodeIndex` (`LinuxFdt::node_index`) records every node
in one traversal and answers both offset and phandle lookups with a binary
search. Nodes are stored in depth-first order, which is strictly increasing
structure-offset order; phandles are sorted separately and keep the first
declaration in firmware order, matching the linear lookup. The index stores
borrowed node pieces (`NodeParts`), so it owns only the `Vec` metadata and
still keeps the traversal-time `interrupt-parent` cache. `FdtNode`'s indexed
accessors (`interrupt_parent_with`, `reference_with`, `irq_spec_with`) use the
index for phandle resolution. The device-tree fwnode backend builds the index
lazily on first node access, because the backend and its root handle are
published before allocator setup; its `root()` path stays allocation-free.

## Purpose

`rs_fdtree` is a minimal, dependency-free, `no_std` parser for the Linux
flattened devicetree (FDT/DTB) format. It turns a DTB blob in memory into
borrowed, `Copy`-able views: the tree of nodes, their properties, memory
regions, and a few well-known kernel nodes (`/chosen`, Open DICE, the
interrupt controller). It is the DT backend. The transitional `of` crate owns boot integration and
legacy queries, not new DT interpretation.

## Responsibilities

- Validate the FDT container: header magic (`0xd00dfeed`), declared total
  size versus the available buffer (`LinuxFdt::new`), nonoverlapping block
  ranges, reservation termination, and the entire structure token stream.
- Iterate and find nodes: depth-first `all_nodes`, path-based `find_node`,
  alias-aware `resolve_node` (via `/aliases`, ignoring `:`-suffixed
  cell suffixes), `find_compatible`.
- Read properties: raw bytes (`property`), C-string and string-list views
  (`property_str`, `compatibles`), big-endian `u32` (`property_u32`),
  borrowed cell sequences (`Cells`), parent-node lookups (`parent_property`,
  `parent_property_u32`), and `cell_sizes` (`#address-cells` /
  `#size-cells`).
- Resolve phandle-based firmware references: `node_by_phandle`, `phandle`,
  inherited `interrupt_parent`, `reference` with binding-supplied argument
  cells, and `irq_spec` returning the controller plus raw interrupt cells.
- Build an allocation-backed `NodeIndex` for repeated offset/phandle lookups
  after allocator setup; the allocation-free accessors remain equivalent.
- Decode `reg` entries into `MemoryRegion`s through `RegIter`, honoring the
  node's cell sizes.
- Enumerate memory layout sources: `memory_regions` (`/memory`),
  `mem_reservations` (memreserve block, `0,0`-terminated),
  `reserved_memory_regions` / `reserved_memory_nodes` (`/reserved-memory`).
- Expose typed wrappers for well-known nodes: `Chosen` (`bootargs`,
  `stdout_path`, `x_kernel_log_level`), `Dice` (`regions` under
  `/reserved-memory/dice` or `google,open-dice` / `kylin,open-dice`
  compatibles), and `InterruptController` (first node carrying an
  `interrupt-controller` property). `x_kernel_log_level` reads the vendor
  property `x-kernel,log-level` and returns it unchanged; the `of` crate
  forwards it (`of::chosen_log_level`) to platform and logging callers, which
  own the level vocabulary. A vendor property rather than `bootargs` keeps
  verbosity out of the boot loader's command line.
- Report failures as `FdtError` (`BadMagic`, `BadPtr`, `BufferTooSmall`, `ParseError`).

## Non-Responsibilities

- No controller-specific bindings, IRQ mapping or device registration.
  Legacy OF interpreters not migrated here are follow-up debt, not the intended
  final ownership of DT semantics.
- No global state: the crate has no statics; a tree is whatever
  `LinuxFdt` borrows, for as long as the caller keeps the blob alive.
- No mutation, patching, or DTB generation: read-only views over
  firmware-provided bytes.
- Borrowed tree/property views do not allocate by default. The optional alloc
  feature enables resource metadata collection and node indexing, not hardware
  resource acquisition. The core parser has no dependencies; the optional
  fwnode adapter adds `fwnode` and `lazyinit` for its lazy index.
- No alignment guarantees: unaligned big-endian field reads are supported
  on purpose (`FdtData`), because boot loaders do not always align the blob.

## Scope

```text
drivers/firmware/rs-fdtree/
├── src/
│   ├── lib.rs            # LinuxFdt, MemReserveIter, re-exports
│   ├── header.rs         # FdtHeader layout, magic and block ranges
│   ├── parsing.rs        # FdtData cursor, big-endian ints, CStr
│   ├── node.rs           # FdtNode, NodeProperty, RegIter, region iterators
│   ├── property.rs       # Strict property encoding reads, borrowed Cells
│   ├── reference.rs      # Phandle lookup, references, interrupt specifiers
│   ├── index.rs          # Optional alloc-backed node/phandle index
│   ├── fwnode_backend/   # fwnode contract adapters
│   │   ├── mod.rs        # Backend module boundary and re-exports
│   │   └── dtb.rs        # DtBackend: device-tree translation
│   ├── error.rs          # FdtError, Result alias
│   └── kernel_nodes/     # Chosen, Dice, InterruptController wrappers
└── Cargo.toml
```

## Architecture

```text
&[u8] DTB blob (or *const u8 via unsafe from_ptr)
      |
  FdtHeader + validate_structure (container and token stream validated)
      |
      +-- structs block --> all_nodes / find_node / reserved_memory_*
      +-- strings block --> property name resolution (string_at_offset)
      +-- memrsv block  --> MemReserveIter (address/size pairs)
      |
  FdtNode<'b, 'a>  -- properties, compatibles, reg --> MemoryRegion
```

All types carry the borrow of the blob: `LinuxFdt<'a>`,
`FdtNode<'b, 'a>` (node lifetime vs blob lifetime), and property values as
`&'a [u8]` / `&'a str`. Nodes and values remain valid without keeping the
`LinuxFdt` value itself.

## Parsing Model

Construction performs one allocation-free validation pass before publishing a
`LinuxFdt`. The accepted format uses the version-17 header (or a newer version
compatible with version 17). Checked block arithmetic establishes bounds,
relative alignment, nonoverlap, and a complete memory-reservation terminator.
The returned reservation view ends immediately after that terminator.

The structure validator checks balanced node tokens, exactly one empty root,
NUL-terminated UTF-8 names, properties preceding children, complete property
headers, name offsets, payload lengths, and alignment padding bounds. It requires
`FDT_END = 9` after the root closes, following the
[Devicetree flattened format](https://devicetree-specification.readthedocs.io/en/stable/flattened-format.html).
Nesting is limited to 63 nodes including the root, matching the existing
iterator's fixed parent storage and bounding recursive path lookup. Padding
bytes are ignored for compatibility with the checked-in crosvm fixture; their
ranges must still be present. Property names may share suffixes in the strings
block.

Traversal subsequently trusts these immutable structural invariants. Its
narrow lint exceptions document constructor-proven complete tokens, UTF-8
names, and delimiters. Property payloads remain uninterpreted bytes: device
semantics, cell formats, and reserved physical-address overlap are separate
checks owned by callers.

## Execution Context

- Borrowed parsing, structural queries and status work before allocator setup.
  Resource collection (mmio_resources) requires the optional alloc feature and
  an initialized allocator; it performs no hardware access.
- RISC-V calls the constructor at physical addresses before enabling paging.
  Token validation consumes NOPs separately and checks the final `FDT_END`
  after the root closes, leaving only the three node/property tokens in the
  dispatch. This avoids the absolute virtual-address jump table generated for
  the former five-token dispatch. Changes to this control flow need optimized
  normal and unit-test binary inspection and early-boot checks; source-level
  branch spelling alone does not constrain compiler lowering.
- `LinuxFdt::new` is the safe entry for an already-sliced blob;
  `from_ptr` is the unsafe boot-loader-pointer entry (null check, header
  read, then the header-declared total size).

## Concurrency Model

- No shared state; every value is `Copy` or a shared borrow, so concurrent
  readers of one DTB need no synchronization.

## Error Model

Construction returns `BadMagic` for a wrong magic, `BufferTooSmall` for a
truncated header or a buffer shorter than the declared total size, and `BadPtr`
for a null pointer in `from_ptr`. `ParseError` reports malformed container or
structure data, unsupported header compatibility, or excessive nesting.
Malformed structure therefore never becomes an absent-node result.

On a successfully constructed tree, accessors return `None` for absent nodes or
properties and for property values that cannot be decoded in the requested
semantic format. Iterators traverse already validated structural data.

Phandle-based accessors distinguish an out-of-range index (`Ok(None)`, which
allows enumeration) from an unresolvable reference
(`ReferenceError::UnresolvedPhandle`) and from a required property that is
missing, duplicated or malformed (`ReferenceError::Property`). A plain
`interrupts` list without any inherited `interrupt-parent` is
`ReferenceError::MissingInterruptParent`, not silence.

## Firmware Trust Boundary

Both constructors validate container ranges and the complete structure before
returning a tree. Callers must keep the borrowed bytes alive and immutable;
raw-pointer callers must additionally establish readability of the complete
header-declared range before calling `from_ptr`. Structural validation does not
establish that firmware describes the actual hardware correctly or that each
property's value obeys its device binding.

## Unsafe Code

One unsafe entry point: `LinuxFdt::from_ptr(ptr)`. It forms two
`slice::from_raw_parts` views — header-sized, then `totalsize`-sized after
re-reading the header from the first view. The `# Safety` contract: `ptr`
must point to a readable DTB whose header and total-size range remain
accessible and immutable for the returned lifetime. All other code is safe and bounds-
checked.

## Design Decisions

- Zero dependencies and no allocation in the core: raw parsing remains usable
  before the kernel heap exists. Allocation-backed helpers are feature-gated.
- Borrowed, `Copy`-heavy views instead of an owned tree: boot-time code
  passes nodes and properties around freely without cloning, and the
  two-lifetime `FdtNode<'b, 'a>` lets iterators borrow the tree while
  values keep pointing into the blob.
- Version-17 headers only: a header with `version < 17` or
  `last_comp_version > 17` is rejected with `ParseError`. The structure
  block bounds check needs `size_dt_struct`, which older header versions do
  not define, so such blobs cannot be validated before use.
- Unaligned reads over alignment preconditions: boot firmware does not
  guarantee 4-byte alignment for the DTB, so `FdtData` reads big-endian
  fields byte-wise.
- Well-known node wrappers (`Chosen`, `Dice`, `InterruptController`) live
  in `kernel_nodes/` to keep node policy discoverable without pushing
  kernel conventions into the generic parser.
- `#![allow(rustdoc::bare_urls)]` is set crate-wide for spec-reference URLs
  in documentation.
