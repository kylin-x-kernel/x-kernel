# Topic Brief

## Topic

User VM core:
`VMA + mmap/munmap/mprotect + page fault + private COW`

## Scope

- `MmSpace` as user address-space owner
- `VmArea` / VMA metadata model
- `mmap`, `munmap`, `mprotect` design boundary
- page-fault dispatch contract
- anonymous private fault path
- private file mapping write-fault to COW transition
- page-table interaction boundary needed for the above

## Explicit Non-Goals

- reclaim
- swap
- memcg
- NUMA
- THP
- `mremap`
- `brk`
- `msync`
- `mlock`
- full page-cache writeback design

## Phase Target

Phase 1 design freeze for user-visible VM core semantics.

## Expected Deliverables

- Linux semantic baseline for the topic
- X-Kernel adaptation draft
- bidirectional review findings
- arbiter decision table
- frozen design
- implementation task split

## Input Assumptions

- Linux MM baseline comes from `docs/ai/skills/linux-mm-design-knowledge/`
- X-Kernel target shape is informed by `mm/docs/linux-aligned-final-architecture.md`
- current memory crates remain valid design inputs, not fixed final structure

## Blocking Open Questions

- what is the minimum official phase-1 object model for file-private COW
- whether phase-1 needs a formal object/view layer, or only a boundary placeholder
