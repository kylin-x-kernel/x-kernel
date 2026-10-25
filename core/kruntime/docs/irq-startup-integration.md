# IRQ Startup Integration Validation

## Status And Revision

Local validation and patch review completed on 2026-10-25 within the coverage
limits below. This is Ticket07 integration evidence, not a declaration that
the complete KIRQ refactor is ready or that this delivery is merged.

Initial runs used Ticket06 commit `aa3f51528`. Later runs use its review fix
`25f161a9f`. Per-CPU timer runs additionally include the Ticket07 changes in
`task/ktask/ut-tests/tests_timer_irq.rs`; the corrected GICv2 run and subsequent
runs also include the private-runqueue fixture in
`task/ktask/ut-tests/run_queue/tests.rs`. These changes affect tests only.
Ticket06 merged into main as `07739d871`; Ticket07 was rebased onto it on
2026-10-25. Its committed tree is identical to `25f161a9f`, so the squash merge
introduces no source differences from the later runs above. Their recorded
commit/build identities remain unchanged: they are not post-merge reruns.

The earlier [phase-split report](irq-startup-validation.md) and
[console report](../../../drivers/platform/console/docs/input-startup-validation.md)
remain separate evidence with their original limits.

## Reproduction

Start each run from the selected platform's `qemu_defconfig`, then run
`make defconfig`. Use a private writable disk, no network/vsock, and an external
timeout. The combined ARM/RISC-V suite is:

```sh
timeout 420 make unittest \
  UNITTEST_CRATE=ktask,kruntime,irq_driver,timer_driver,kipi,console_driver \
  XKMAKE_ARGS='--no-net --no-vsock'
```

Use `ACCEL=n` for x86 TCG. Select `UNITTEST_CRATE=ktask` for the focused per-CPU
timer runs. LoongArch uses `ktask,kruntime,kplat_loongarch64,console_driver`.
The new expiry/rearm cases wait for callback acknowledgement, not a guest
latency threshold. A callback reporting invalid context or rearm failure ends
the wait immediately, cancels the timer and reports probe failure. Missing IRQ
delivery without a callback still requires the external timeout; each target
CPU is printed before its task starts so a stalled join can be attributed.
The present-CPU iteration relies on the runtime's all-discovered-CPU service
rendezvous before unittest entry, not on present being a generic online mask.
Older tests retain their existing diagnostic timeouts.

After the prerequisite closeout, review follow-up added a failed-callback wait
regression. AArch64 default SMP4 `make unittest UNITTEST_CRATE=ktask` passed
135/135 with coverage export on `6741af91d` plus that follow-up. CPU0..3 each
reported status 0 for one and three expirations; the failed-callback regression
and private-runqueue publish rejection test passed. Log:
`/tmp/irq07-review-ktask-ut.log`. This focused run does not replace the earlier
multi-architecture matrix.

For GICv2, disable NMI and watchdog selections, regenerate configuration and
append `QEMU_ARGS='-machine virt,gic-version=2'`. A separately dumped DT with
the same machine options identified `arm,cortex-a15-gic`, confirming the
override. For real PMU NMI tests, disable watchdog/hardlockup so the test owns
that source. Select pseudo-NMI or hardware NMI explicitly. Hardware mode uses
`QEMU_ARGS='-cpu max'`; the runtime must report `Hardware (GICv3.3)`.

## Functional Results

All successful rows exited zero, stopped QEMU and generated coverage reports.
Counts include existing tests and are not additive coverage metrics.

| Configuration | Suite | Passed | Failed / ignored |
| --- | --- | --- | --- |
| x86 TCG, SMP4 | console/runtime/IPI/IRQ/timer baseline | 130 | 0 / 0 |
| x86 TCG, SMP4 | ktask with per-CPU timer extension | 129 | 0 / 0 |
| x86 SMP kernel, one runtime CPU | baseline ktask | 123 | 0 / 6 |
| RISC-V SMP4 | combined baseline | 257 | 0 / 0 |
| RISC-V SMP4 | ktask with per-CPU timer extension | 129 | 0 / 0 |
| ARM GICv3, default PMR/watchdog | ktask with per-CPU timer extension | 134 | 0 / 0 |
| ARM GICv3, PMR without watchdog | combined baseline, real NMI | 275 | 0 / 0 |
| ARM GICv2, no NMI/watchdog | combined, isolated runqueue fixture | 274 | 0 / 0 |
| ARM GICv3, hardware NMI, CPU max | combined, both Ticket07 test changes | 275 | 0 / 0 |
| LoongArch default non-SMP, one CPU | platform/runtime/ktask/console | 175 | 0 / 0 |
| RISC-V SMP4, post-merge 07739d871 | combined, both Ticket07 test changes | 257 | 0 / 0 |

The focused timer extension reports expiry counts 1 and 3 with status 0 on
CPU0..3 for ARM, x86 and RISC-V, and CPU0 for LoongArch. A one-CPU execution
of an SMP kernel is not a pure UP build. LoongArch's default Cargo feature
selection does not enable SMP/IPI and its defconfig sets NR_CPUS=1.

Both PMR and hardware-NMI runs printed real PMU NMI results for all four CPUs:
129 irq_work nodes, zero source-context allocation bytes, one worker handoff
and three deferred payloads per CPU. The source fired while holding the GIC
lock. This is stronger than a green aggregate: unavailable-backend probes can
return early. GICv2 results do not claim NMI or irq_work backend delivery.
Hardware mode does not establish nested NMI or physical-board correctness.

### Post-merge Smoke

After rebasing onto `07739d871`, the combined RISC-V suite above passed again
with both Ticket07 test changes present (`git_dirty=true`). CPU0..3 each
reported status 0 for expiry counts 1 and 3. The host command exited zero,
QEMU exited, and coverage export completed. This is the post-merge smoke;
the other platform rows retain their original pre-merge identities.

Log: `/tmp/irq07-postmerge-riscv-ut.log`. Configuration SHA256:
`3d0011256701c411134448a71de189c27a35970b0313b56d2889a4b33079c39e`.
Build ID: `e622236d79ecc0310f6e4482bda1cf2c35d1424977fb5b2e9df53ab0e43692d7`.
Reproduction used the RISC-V defconfig, `make defconfig`, the combined command
above, `DISK_IMG=/tmp/irq07-console-riscv.img`, and a worktree-local isolated
`TARGET_DIR=target/uart-probe`.

### Test Isolation Correction

The first GICv2 run had 273 passes and one failure in the pre-existing
`publish_reject_keeps_previous_owner`: the live runqueue's `nr_home` changed
between samples. Local IRQ/preemption exclusion does not exclude remote
publishers, so that fixture could observe legitimate unrelated changes.
The exact remote publisher was not traced. The test now uses an unpublished
private runqueue while retaining rejection, previous-owner and unchanged-count
assertions. The repeated combined run passed 274 tests. This is a fixture
correction, not evidence of a production GIC or scheduler fix.

## Failure And Pending-Event Evidence

Temporary local probes were removed after verification; they are not product
hooks or a second startup state machine.

- x86 preparation: return BadState after normal masked APIC initialization.
  Boot stopped at the preparation error; activation and CPU-service markers
  were absent. This tests provider-error handling, not hardware rollback.
- x86 mandatory timer registration: the first actual registration returned
  `Ok(true)`, a duplicate returned `Ok(false)`, and boot stopped at the mandatory
  registration check before activation/services. No allocator OOM was injected.
- ARM initial timer observation: activation produces an expired timer, but
  local service initialization subsequently reprograms or disarms it. Initial
  expiry must not be equated with a deadline that remains pending unchanged.
- ARM controlled pending event: after local services were prepared, immediately
  before the existing CPU IRQ enable, arm an already-due physical timer and
  observe CTL=0x5 with no prior timer delivery. On every CPU, the first real
  handler observed the same CVAL and CTL=0x5 before calling `on_timer_fire`.
  No manual dispatch, replacement handler or premature CPU IRQ enable was used.
  This diagnostic suite passed 87 tests; the restored-source suite passed 86.

The controlled ARM run used configuration SHA256
`a8198b9bcdc4ac22a7439f76f2934e93b952179d3afcd8f613bace79bccac638`.
Expected and first-handler CVAL values matched on CPU0..3 respectively:
877277733, 906246873, 906358233, 906377986. These are raw diagnostic values,
not latency measurements. This establishes pending delivery at initial CPU
IRQ admission, not preservation of firmware pending bits or superseded timers.

## Interactive Console Checks

Normal non-UT SMP4 boots at `25f161a9f` used private copies of existing
architecture-specific rootfs images, with no rootfs or init-script edits.
Both x86 TCG and RISC-V passed the following sequence through QEMU serial:

1. Run `read -r reply`, send a new line, and verify the printed value matches.
2. Launch `(sleep 1; printf 'IRQ07_BACKGROUND\n') &` and send no further input
   until its output appears while the shell waits for input.
3. Run `cat`, send a line and observe terminal echo and application output.
4. Send Ctrl-C, verify the prompt returns, and run another marker command.
5. Run `sync; poweroff -f`; the complete host command exits zero.

Logs: `/tmp/irq07-console-x86.log` and `/tmp/irq07-console-riscv.log`.
Commands use `timeout 420 script -q -e -f -c 'make run ...' LOG`,
`XKMAKE_ARGS='--no-net --no-vsock'`, a private `DISK_IMG`, and `ACCEL=n`
for x86. Platform defconfig and `make defconfig` precede each run.

| Platform | Configuration SHA256 | Build ID |
| --- | --- | --- |
| x86 | 4f202b3132fb82f3f030aa9752c800b7d3db3f2c7c150b294b1e004d1db1b7d3 | 95f05af99a35c9f28f48efa114e0b1df287fd7825eea03c583dd99570a9b6244 |
| RISC-V | f777f0735427ff9103b35a15922cb6e38a2537a029ee5554ce7018b62eb31b90 | 54ed1bf78eed70ab000a1624d73ec224f169c23be1deae732dbb92606f26e794 |

Both runs also logged `Unhandled IRQ 4096` near serial input, at warning counts
1, 2 and 4. Preserve this observation despite successful interactive behavior.
An unmodified, clean main build at `7a26f7f0a` reproduced counts 1, 2 and 4
on RISC-V with the same private rootfs and read/cat/Ctrl-C sequence, then
powered off successfully. Log: `/tmp/irq07-console-riscv-main.log`; build ID:
`e56ef81c6512eea72eacc73efb84fd8bbd2131cad20665fccd31879664217616`.
This establishes that the warning exists before Ticket06, not that every
warning is harmless or that the x86 cause is identical.

A temporary RISC-V probe mapped virq 4096 to PLIC domain 2, hwirq 10 (UART).
It sampled IIR and collected-byte counts under the existing port lock without
changing handled status. Representative consecutive observations were:

| IIR | Bytes collected by this handler | Cumulative handler bytes | Bytes collected by read_data |
| --- | --- | --- | --- |
| 0xcc (RX timeout) | 51 | 51 | 0 |
| 0xc1 (no pending cause) | 0 | 51 | 0 |
| 0xc4 (RX data) | 23 | 74 | 0 |
| 0xc1 (no pending cause) | 0 | 74 | 0 |

Each empty invocation returned NOT_HANDLED. These observations show delivery
after a preceding handler drained data; they do not support attributing these
samples to read_data draining first. Other collection paths were not counted.
The precise source of the extra controller delivery remains untraced. IIR
sampling and logging can perturb timing, so this is diagnostic evidence, not
a timing or loss-rate measurement.

Linux's `drivers/tty/serial/8250/8250_port.c:serial8250_handle_irq()` also
returns zero when IIR has UART_IIR_NO_INT set. Returning HANDLED unconditionally
would hide the observation rather than establish correct acknowledgement.
Keep controller/UART redelivery diagnosis separate from startup acceptance;
this ticket does not modify the existing handler's handled-status policy.
On 2026-10-25 the user explicitly deferred further diagnosis until the KIRQ
refactor is complete. This scheduling decision does not establish harmlessness
or close the underlying observation.

Probe log: `/tmp/irq07-console-riscv-probe-isolated.log`; patch retained at
`/tmp/irq07-uart-probe-final.patch`. Build ID:
`025ac5ced39490f3866f2d68281f7d74ea35465968b3eb14a9c1df739676a341`.
The probe was removed from all three console source files after verification.
An earlier probe build reused another worktree's target directory and failed
against a stale ConsoleIf artifact before boot; the successful probe used a
separate target directory. Do not share incremental targets between these
baseline and modified source trees.

## Ownership And Handoff

The integrated source retains the O01/O05 ownership split: runtime sequences
preparation, mandatory action installation, local activation and CPU services;
platform adapters select controller/timer implementations; console owns its RX
generation and optional-input transaction. Neither generic kirq nor a new global
ready flag takes over device startup. AP runqueue publication permits enqueueing,
not synchronous execution; service readiness is published separately after
activation. Mandatory failures stop bring-up, whereas console setup failures
retain polling without disabling a potentially shared mapping.

The preparation/registration fault probes, controlled pending-event run and
console failure fixtures support those startup contracts. This handoff does not
establish future Line operation admission or Domain publication/retirement
safety. Those work packages must preserve direct-Virq timer/IPI handling and
source-owner ordering, and add their own concurrency and retirement tests.

## Prerequisite Closeout

Ticket07 also carries two independently reviewable prerequisite commits:

- `af62714ed`: isolate XArray task/context/migration fixtures in the optional
  `kxarray-kernel-tests` crate, avoiding a future `kirq -> kxarray -> kirq`
  dependency cycle. Basic UT, host tests and doctests remain in the container;
  neither the XArray algorithm nor RCU algorithm changes.
- `932da2937`: give PCH PIC and EIOINTC separate controller-local locks for
  complete initialization and shared-register RMW transactions. ISR/W1C
  completion remains independent of the control locks.

These changes were validated on `6d96eeb1a` plus the prerequisite patches before
being cherry-picked onto Ticket07. These are not reruns of the full combined
Ticket07 tree. The original local commits are `fa4aa5cd8` and `994bb7095`.

| Check | Result |
| --- | --- |
| LoongArch default UP, platform/runtime/ktask UT | 161 passed, 0 failed/ignored; coverage exported |
| LoongArch one-CPU CAS-backend diagnostic, platform UT | 15 passed, 0 failed/ignored; coverage exported |
| LoongArch default `make UNITTEST=y clippy` | Passed |
| AArch64 SMP4, `kxarray,kxarray_kernel_tests` UT | 24 passed, including all 13 moved integration tests; coverage exported |
| AArch64 `make UNITTEST=y clippy` | Passed |
| XArray host tests and doctests | 7 unit tests and 6 doctests passed |

The CAS diagnostic temporarily selected the platform's existing `smp` feature
while retaining one runtime CPU; that temporary default was removed. It checks
actual lock acquisition/exclusion, not AP bring-up or physical multi-node IOCSR
behavior. Dependency inspection confirmed that the container no longer depends
on kirq/ktask/khal and ordinary configurations do not select the test crate.

Review follow-up replaced the lock-held W1C test with a recorder-side IRQ-state
assertion: the mock call begins unmasked and the write must observe that state.
Unlike `try_lock`, this detects a control `SpinNoIrq` spanning the write even
when UP compiles out the CAS flag. A temporary regression acquiring
`REGISTER_LOCK` in `complete_irq_impl` produced exactly one assertion failure
(15 passed, 1 failed), not a timeout. After removing that temporary change,
default LoongArch platform UT passed 16/16 and exported coverage. Logs:
`/tmp/irq07-w1c-negative.log` and `/tmp/irq07-w1c-positive.log`.
This detects lock acquisition spanning the W1C access, not arbitrary locking
elsewhere in the function; product code is unchanged by this follow-up.

An independent detached snapshot validator also reported AArch64 SMP4 Clippy,
24/24 guest tests (all 13 moved integration cases), coverage export and the
7+6 host checks passing. Its source snapshot was
`/tmp/kxarray-isolation-source.Brr9hl-git`; logs are
`/tmp/kxarray-isolation-snapshot-{clippy,qemu}.log` and
`/tmp/kxarray-isolation-host.log`. That validation restarted in a snapshot
after detecting concurrent platform configuration changes in the shared worktree.
Do not count the abandoned shared-worktree run as complete validation.

An isolated target directory alone does not isolate `.config` or generated
`.cargo/.xconfig.toml`. Concurrent platform validators must use independent
source worktrees/snapshots as well as independent build directories and disks.
For that validator's isolated host build, `TARGET_DIR`, `CARGO_TARGET_DIR`,
`K_PLAT_NAME=kplat-aarch64` and cleared `RUSTFLAGS` were set explicitly.

## Coverage Limits

- The integrated base is `07739d871` with the Ticket07 test changes described
  above. Only the RISC-V combined run was repeated after the squash merge;
  equality of the committed base trees was checked separately.
- LoongArch AP bring-up remains untested. AP code exists, but merely increasing
  QEMU CPU count does not enable its missing SMP feature selection. Do not
  silently change feature assembly or claim AP support from the default run.
- Track the existing UART unhandled-IRQ observation separately. RISC-V has
  baseline reproduction and source/cause samples above; x86 source attribution
  is not established. ARM interactive evidence is in the console report.
- Code-convention and requirement/coverage reviews found no actionable defect
  in the retained test changes. Diagnostic logs and probe patches were copied
  out of `/tmp` into the foundation worktree's ignored
  `.scratch/kirq-restart/artifacts/startup07/` evidence directory; these are
  local records, not uploaded CI artifacts. UNITTEST Clippy passed for the extended RISC-V
  timer tests and the ARM GICv2 isolated-fixture configuration. After removing
  the UART probe, RISC-V `make UNITTEST=y clippy` passed again using the isolated
  target directory; workspace formatting and `git diff --check` also passed.

No physical-platform, performance or long-duration stress claim is made.
Line admission, Domain/XArray publication, management mutex and MSI changes
remain outside this ticket and require their own tests after implementation.
