---
name: linux-mm-design-knowledge
description: Use when an AI agent needs a Linux source-grounded semantic baseline for X-Kernel memory-management design work, especially for mm struct, VMA, mmap, page faults, anonymous memory, file-backed mmap, COW, brk, madvise, msync, and mlock decisions.
---

# Linux MM Design Knowledge

Use this shared skill when the task requires:

- grounding an X-Kernel MM design in Linux user-visible semantics;
- checking whether a proposed memory-model change matches Linux responsibilities;
- understanding which Linux data structures, invariants, and paths actually
  carry the behavior;
- distinguishing Linux-required semantics from implementation complexity that
  X-Kernel may defer or simplify in early phases.

This skill is a knowledge base, not a coding workflow.

## Source Tree Confirmation

Before using this skill in a design workflow, confirm the Linux source tree and
version.

Default source tree:

- `~/code/linux-stable`

Expected default version:

- Linux `v7.0`

Required behavior:

- If `~/code/linux-stable` exists, read its `Makefile` or git metadata and
  record the observed version in the workflow output.
- If the default path does not exist, ask the user for the Linux source tree
  path before producing a Linux baseline.
- If the observed version differs from the expected default or the requested
  design depends on version-sensitive behavior, ask the user whether to use the
  observed tree or a different Linux source tree.
- For disputed or critical semantics, re-check the local Linux source instead
  of relying only on the summarized topic documents.

## Entry Point

Start with:

- [README.md](README.md)

Then read the topic documents that match the current design problem:

- `00-linux-mm-map.md`
- `01-address-space-mm-struct.md`
- `02-vma-design.md`
- `03-mmap-munmap-mprotect.md`
- `04-page-table-design.md`
- `05-page-fault-path.md`
- `06-anonymous-memory.md`
- `07-file-backed-mmap.md`
- `08-cow-design.md`
- `09-brk-stack-heap.md`
- `10-madvise-msync-mlock.md`
- `99-open-questions.md`

## How To Apply It

For each mechanism, separate:

- Linux-required user-visible semantics
- Linux internal implementation choices
- X-Kernel phase-1 simplifications that do not violate the ABI contract

Do not treat this skill as permission to copy Linux structure layouts
mechanically. Use it to extract the semantic contract and the real invariants.

## Typical Consumers

This shared skill is expected to be consumed by:

- `docs/ai/skills/xkernel-mm-design-workflow/`
- memory-subsystem design reviews
- Linux compatibility checks for MM refactors
