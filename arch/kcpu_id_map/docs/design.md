# kcpu_id_map — Design

## Scope

`kcpu_id_map` owns logical/raw CPU identity, the `KCpuMask` capacity domain, and
the runtime CPU numbering facts used by CPU-facing text ABIs. It is reachable
from early boot, so the parser is allocation-free and has no VFS or IRQ policy
dependency.

## Runtime CPU facts

- `NR_CPUS` is the compile-time mask capacity, not a userspace CPU range.
- `present` is the set of logical IDs that currently have a logical-to-raw
  mapping. This implementation has no independent possible/firmware-disabled
  set; **possible is currently present**, which is a documented limitation, not
  a complete Linux possible model.
- `online` is present minus explicit hot-unplug.
- `ready` is published by `kiface_cpu_api::CpuInterruptReadinessIf`; it is not a
  parser/domain fact. A present but not-ready CPU is still a valid requested
  target until KIRQ applies its readiness intersection.
- `nr_cpu_ids()` is the highest present logical ID plus one, minimum 1 before
  initialization. It is the bitmap width/high-chunk upper bound. `nr_cpus()`
  remains the present-CPU count and must not be used as the text width.

## CPU text codec

`src/text.rs` exposes:

```text
CpuTextDomain::{new, runtime, nr_cpu_ids, present}
CpuTextError
parse_bitmap / parse_list
write_bitmap / write_list
bitmap_capacity / list_capacity
```

`CpuTextDomain` has private fields. Its constructor accepts only the present
mask and derives width from the highest present bit, so width and membership
cannot disagree. Empty present is `EmptyDomain`; `runtime()` never synthesizes
CPU 0.

Parser and formatter are allocation-free. Formatting writes canonical text with
no trailing newline; consumers add their own line ending and reserve output
space with the capacity helpers.

## Linux grammar and error order

The parser follows Linux v6.12 `lib/bitmap-str.c` for the accepted grammar:

- bitmap: comma-separated hexadecimal chunks, at most 32 bits each, high-to-low;
  empty chunks/commas are skipped; short input leaves missing high chunks zero;
  a set bit in `[nr_cpu_ids, words*32)` is `Overflow`.
- list: decimal, ranges, `N`, case-insensitive `all`, and numeric
  `a-b:off/group`; a single number cannot take `:off/group`.
- region separators are `isspace()` (including vertical tab/form feed) or comma;
  whitespace is not allowed inside a range or its pattern.
- first newline/NUL terminates input.
- syntax/width errors are resolved before present membership. For example, a
  bitmap containing both an absent bit and an out-of-width bit reports
  `Overflow`; a list with a non-present valid region followed by invalid syntax
  reports `InvalidSyntax`.
- `all`/`N` expand over `nr_cpu_ids` and are checked against present afterward;
  they are never silently intersected with present.

## Consumers

procfs task/status and IRQ affinity files both use this module. procfs owns the
domain sampling, output buffers, line endings, `SeqTextSink` errors, and
Linux-errno mapping. IRQ's `SeqCommandOptions.max_command_bytes` is an independent
128-byte resource budget, not the canonical output capacity. IRQ captures one
domain per stable LineRecord; task/status captures one per status record.

## Tests

`ut-tests/text.rs` contains fixed expected vectors for bitmap/list syntax,
sparse-present rejection, error precedence, formatter width, capacity, and the
Linux boundary cases. Tests requiring a wider `NR_CPUS` return
`unittest::TestResult::Ignored`; ignored count must be reported separately from
passed assertions. The current single-word configuration does not execute
width-33/64 vectors.
