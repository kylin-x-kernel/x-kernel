# AArch64 EL0 cache-permission regression

This standalone C probe checks Linux-visible user instruction behavior. It does
not inspect kernel logs or require a desktop, network, or application package.
It is not registered in the external Starry Test Harness: registration into the
`ci-test-iter` workspace is staged as a follow-up of the PR that carries this
probe, and until then this regression runs manually only.

## Build and run

From the repository root, use an existing AArch64 musl toolchain:

```sh
mkdir -p target/experiments/aarch64-el0-cache
aarch64-linux-musl-gcc -std=c11 -O2 -Wall -Wextra -Werror -static -no-pie \
  docs/experiments/aarch64-el0-cache/cache-permission-probe.c \
  -o target/experiments/aarch64-el0-cache/cache-permission-probe
```

The output is a static AArch64 executable. Copy it to an AArch64 Linux or
Linux-compatible guest using the test environment's normal injection method.
Run it **inside that environment**, not directly on an x86 host:

```sh
./cache-permission-probe all
./cache-permission-probe all --all-cpus
```

`--all` is an alias for `all`. Individual modes are `ctr`, `dc`, `ic`, `zva`,
`jit`, and `udf`. No root privileges are required by the probe itself. The runtime
must allow anonymous mappings, `mprotect`, `fork`, and observing child status.
The mapping length comes from `sysconf(_SC_PAGESIZE)`.

## Expected behavior

| Mode | Operation | Passing observation |
| --- | --- | --- |
| `ctr` | Read `CTR_EL0` from EL0 | Child exits normally with status 0; no particular CTR value is required. |
| `dc` | `DC CVAU` on the child's live private mapping, followed by barriers | Child exits normally with status 0. |
| `ic` | `IC IVAU` on the child's live private mapping, followed by barriers | Child exits normally with status 0. |
| `zva` | Read `DCZID_EL0`; when `DZP` is set exit with the skip code (77). Otherwise fill the mapping with a nonzero byte, run `DC ZVA`, and verify the covered line reads back zero | Child exits normally with status 0, or is reported as a skip when DC ZVA is unsupported. |
| `jit` | Write two instructions, invoke the toolchain's `__builtin___clear_cache`, change RW to RX, and call the generated code; repeat on the same page | Calls return 42 and then 73, and the child exits with status 0. |
| `udf` | Execute the truly undefined instruction word `0x00000000` | `waitpid` reports termination by SIGILL. Returning normally or merely exiting with code 90 (`CHILD_ERROR`) fails. |

Each mode forks its own child and resets/unblocks SIGILL, SIGSEGV, and SIGBUS.
UDF is a negative control: permitting cache instructions must not suppress
actual undefined-instruction faults. Core dumps are disabled for this process
and its children. Tests assert observable outcomes, not whether a kernel
implements an instruction directly or emulates a trap.

Output is newline-delimited JSON. The parent emits one `result` per mode and
a final `summary` with separate `passed`, `failed`, and `skipped` counts.
Child setup failures, wrong return values, unexpected signals, and timeouts
are failures. A default `all` run expects six passing results and zero skips;
when the target reports `DCZID_EL0.DZP=1` the `zva` mode counts as a skip.
The process exits 0 for complete success, 1 for a test/infrastructure failure,
2 for invalid invocation or parent setup failure, or 77 when optional coverage
(CPU affinity or DC ZVA support) was skipped without another failure. Preserve
stdout, stderr, and the actual command status; a subsequent shell command's
status is not the result.

`--all-cpus` first performs the ordinary unbound six modes, then repeats them
on each CPU in the calling task's allowed affinity mask. Each child requests
a singleton affinity mask and checks `sched_getcpu()` before its operation.
This covers available CPUs, not CPUs excluded by a container, cpuset, or
offline state. Unsupported/denied affinity calls and CPU availability changes
are explicit skips and cause exit 77, never a pass. The fixed `cpu_set_t`
capacity also bounds enumeration; kernels requiring a larger mask can return
EINVAL, which is reported as skipped coverage. An empty mask or inconsistent
reported CPU is a failure. No unbounded enumeration or affinity retry occurs.

Every child has a 10-second monotonic observation deadline. Timeout cleanup
checks that the child is still waitable before sending SIGKILL, then allows
at most two more seconds to reap it. ECHILD never causes signaling of the
numerical PID, because it may have been reused. EINTR during ordinary waiting
does not itself fail a test. Cleanup failures are reported, not hidden.
These are liveness bounds, not performance requirements: a hung kernel cannot
run a guest timer, so the host harness must also impose an overall timeout.
For `--all-cpus`, budget up to 12 seconds per mode per allowed CPU, plus the
six unbound modes and boot overhead.

## Scope and references

### Before/after validation

Tested against upstream `519c186065ad6ac0eb01e69004f5bff28edd5579` and the
UCT/UCI initialization change, using the same probe binary and fresh copies
of the same minimal BusyBox/musl ext4 image. Both kernels used the official
`kplat-aarch64/qemu_defconfig`, release mode, and QEMU 8.2.2 TCG with
`virt,gic-version=3`, `cortex-a76`, four CPUs and 1 GiB of RAM.
The probe was built with AArch64 musl GCC 11.2.1 and the flags above.

| `all --all-cpus` result | Unmodified upstream | With UCT/UCI initialized |
| --- | --- | --- |
| `ctr`, `dc`, `ic`, `jit` | All 20 terminate with SIGILL | All 20 pass |
| Undefined-instruction control | All 5 correctly terminate with SIGILL | All 5 correctly terminate with SIGILL |
| Summary / exit status | 5 pass, 20 fail, 0 skip / 1 | 25 pass, 0 fail, 0 skip / 0 |

The five runs of each mode consist of one unbound run and singleton-affinity
runs on CPUs 0 through 3. Each bound child confirmed its requested CPU using
`sched_getcpu()`. This is a standalone guest regression, not a Starry suite
result.

**DZE/`zva` validation on this tree:** the run above predates `zva` — it
covered the five UCT/UCI modes only. A separate before/after run was recorded
for the six-mode probe on this repository's kernels: baseline upstream `main`
(`ca9c0e800`, no EL0 grant) versus the DZE/UCT/UCI change (`9cad479a5`,
non-VMM gated), both built from the official `kplat-aarch64/qemu_defconfig`,
release mode, and run on QEMU 10.0.13 TCG (`virt,gic-version=3`, `cortex-a76`,
four CPUs, 1 GiB), booting this repository's Debian/OpenRC ext4 rootfs with
the probe at `/root`, built with AArch64 musl GCC 11.2.1 and the flags above.
Each side ran `all --all-cpus`: one unbound run and singleton-affinity runs on
CPUs 0–3 per mode.

| `all --all-cpus` result | Baseline `main` (`ca9c0e800`) | With DZE/UCT/UCI (`9cad479a5`) |
| --- | --- | --- |
| `ctr`, `dc`, `ic`, `jit` | All 20 terminate with SIGILL | All 20 pass |
| `zva` | 5 skips — with `DZE=0` the architecture reports `DCZID_EL0.DZP=1`, so the probe records "unsupported" instead of trapping | All 5 pass — `DZP=0`, `DC ZVA` zeroes the 64-byte line, read-back verified |
| `udf` negative control | All 5 correctly terminate with SIGILL | All 5 correctly terminate with SIGILL |
| Summary / exit status | 5 pass, 20 fail, 5 skip / 1 | 30 pass, 0 fail, 0 skip / 0 |

The `zva` skip-on-baseline behavior is architectural: `SCTLR_EL1.DZE=0` traps
`DC ZVA` at EL0 *and* makes `DCZID_EL0.DZP` read as 1. The patched run showing
`DZP=0` with verified line zeroing is therefore also positive evidence that
the DZE grant targets the correct control.

### Coverage limits

The regression targets instruction permissions in the ordinary AArch64
EL1/EL0 regime. Setting SCTLR_EL1.DZE/UCT/UCI removes those controls' traps;
other architectural and higher-level trap controls can still apply. The
generated instruction stores currently target little-endian AArch64.

Success under QEMU TCG does not establish real hardware instruction/data cache
coherence, cross-CPU generated-code publication, heterogeneous cache geometry,
VHE behavior, suspend/resume, or all instruction forms controlled by UCI.
The probe deliberately does not assert timing thresholds or a fixed cache-line
size. A policy that intentionally prohibits executable mappings may reject the
JIT test; that is recorded as a failure with its syscall stage, not disguised
as an instruction-permission pass.

Primary contract references:

- [Linux v6.12 initial SCTLR configuration](https://github.com/torvalds/linux/blob/v6.12/arch/arm64/include/asm/sysreg.h#L824) includes DZE/UCT/UCI for the normal EL1/EL0 setup.
- [LLVM 20.1 clear-cache implementation](https://github.com/llvm/llvm-project/blob/llvmorg-20.1.0/compiler-rt/lib/builtins/clear_cache.c#L115) reads CTR and uses user cache maintenance and barriers.
- [Linux waitpid(2)](https://man7.org/linux/man-pages/man2/waitpid.2.html) defines signal termination and child reaping.
- [Linux sched_setaffinity(2)](https://man7.org/linux/man-pages/man2/sched_setaffinity.2.html) defines CPU-mask restrictions.

When integrating this executable into Starry, use a single `ci-test-iter` case
with an explicit guest command and retain its JSON/exit status. Do not report
a harness pass until that separately registered case has actually run.
