# BPF uapi constants: single source and conformance check

Every uapi value this kernel needs (`enum bpf_cmd`, `bpf_prog_type`,
`bpf_attach_type`, `bpf_map_type`, `bpf_func_id`, the `BPF_DEVCG_*` and
`BPF_F_*` enums, and the ISA bit definitions) lives in exactly one place:

- `core/kbpf/src/uapi.rs` — generated, committed, no build-time dependency on
  a Linux tree.

Three crates consume it: `core/kbpf` (interpreter opcode bits),
`fs/filesystems/bpffs` (device-controller encoding) and `posix/bpf`
(syscall ABI). None of them re-types a uapi value.

## Why

The cgroup-v2 device controller was first transcribed by hand, and two
values were wrong: `BPF_PROG_TYPE_CGROUP_DEVICE` was written as 6 (that is
`XDP`) and the attach type as 28 (28 is the `BPF_LINK_CREATE` *command*
number; the attach type is 6). Every `runc` attach therefore failed with
`EINVAL`, and no test caught it because the test fixtures had been written
with the same wrong constants. A second class of error — the
`access_type` bit layout — was likewise invisible: the code and its tests
shared one wrong assumption.

Values that come from an enum *position* are exactly the kind that goes
wrong silently. Generating them, and pinning the result with an independent
test, is what makes the class of bug detectable.

## What is emitted

Everything, not a curated subset:

- every enum in `bpf.h` (60 of them, named and anonymous; anonymous ones get a
  deterministic name derived from their first member such as
  `bpf_anon_bpf_f_recompute_csum`, with hand-chosen names for the enums this
  kernel reads directly: `bpf_devcg_acc`, `bpf_devcg_dev`,
  `bpf_map_update_flags`),
- every integer `BPF_*` `#define` from `bpf.h` and `bpf_common.h`,
- every helper id from `___BPF_FUNC_MAPPER`.

Around 700 constants. Unused ones cost nothing at runtime and mean a new uapi
value can be consumed without touching the generator.

Declarations the generator cannot fold are **not** dropped silently: they are
listed in `uapi::NOT_CONSTANTS` and asserted empty by the conformance test.
The only remaining non-uapi value is `BPF_CGROUP_MAX_PROGS`, declared in the
generator's `EXTRA_DEFINES` with its source.

Constants are typed by value (`u32`, signed `i32` for negative enumerators,
`u64` only where a mask needs it) so consumers can use them directly in
`match` patterns without casting.

## Regenerating

```sh
python3 xtask/bpf_uapi/gen.py --linux /path/to/linux-tree
```

The generator parses `include/uapi/linux/bpf.h` and
`include/uapi/linux/bpf_common.h`, evaluates the `#define` expressions and
enum bodies, and emits `core/kbpf/src/uapi.rs` with the source-header digest
in the module doc. Re-run it when the uapi baseline moves and **review the
diff**: every changed value is an ABI decision.

`--check` fails if the committed file is stale relative to the given tree:

```sh
python3 xtask/bpf_uapi/gen.py --linux /path/to/linux-tree --check
```

Values that are not declared in the uapi headers are listed in the
generator's `EXTRA_DEFINES` with their source named — currently only
`BPF_CGROUP_MAX_PROGS`, which is a kernel-internal define in
`kernel/bpf/cgroup.c`.

## Verifying (the part that does not trust the generator)

```sh
cargo test -p kbpf --test uapi_conformance
```

`core/kbpf/tests/uapi_conformance.rs` transcribes the expected values **by
hand** from the Linux headers — it never reads the generated table to derive
an expectation. It covers:

- the full `enum bpf_cmd` order (36 entries),
- `bpf_prog_type` / `bpf_attach_type` positions, including the two values
  that were once wrong,
- map types and update flags,
- the `BPF_DEVCG_*` bits and `BPF_CGROUP_MAX_PROGS`,
- the ISA bit layout (classes, sizes, source bit, ALU operations, jump
  conditions, endianness selectors),
- opcode bytes composed from that layout (`0xb4`, `0xb7`, `0x15`, `0x45`,
  `0x56`, `0xc6`, `0xd4`/`0xdc`/`0xd7`/`0xdf`, loads/stores),
- the helper ids the interpreter dispatches on.

A failure means either the table drifted from the uapi ABI or the generator
mis-evaluated an expression. Both block the change.

The test is the check to run after touching anything BPF-related; it needs no
Linux tree and no guest VM.

## Adding a new uapi value

1. Regenerate from the baseline (usually all that is needed).
2. If the value lives outside `bpf.h`, add it to `EXTRA_DEFINES` with the
   source file named.
3. Add or extend a hand-transcribed expectation in
   `core/kbpf/tests/uapi_conformance.rs`.
4. Consume it from `kbpf::uapi`; do not declare a local copy.
