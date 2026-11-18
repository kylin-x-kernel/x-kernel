# kcpu_id_map — Security and Reliability

## CPU text input boundary

CPU bitmap/list bytes are external input. The shared parser is allocation-free,
uses fixed `KCpuMask` storage, and never calls VFS, IRQ policy, hotplug, or
hardware code. It bounds every set bit by the supplied domain and the mask
capacity before masking, avoiding out-of-bounds `CpuMask::set`.

The parser rejects malformed syntax, integer overflow, bitmap chunk overflow and
list out-of-range values with the module-local `CpuTextError`; errno mapping is
owned by procfs. Present membership is validated after complete syntax/width
parsing so an earlier non-present bit cannot hide a later overflow or syntax
error.

`CpuTextDomain` is immutable after construction. Its constructor derives width
from present and rejects an empty set; callers cannot forge a mismatched width.
The current possible==present limitation means this is not a complete Linux
possible-CPU authorization model; callers must not infer hotplug/possible
support from `nr_cpu_ids`.

## Output and consumers

Formatter functions do not allocate and do not append newlines. Consumers must
reserve a checked output capacity, add their own line ending, and preserve sink
allocation/limit errors rather than collapsing them into `Output`. Domain
sampling is fail-closed: an uninitialized or empty present set is an error, never
an implicit CPU 0.

## Verification

Syntax and capacity vectors are independent expected values, not roundtrips.
Capacity-gated vectors report `Ignored` when the configured `NR_CPUS` cannot
represent them; ignored tests are not evidence of execution. Multi-word coverage
requires a real `NR_CPUS>=33` configuration.
